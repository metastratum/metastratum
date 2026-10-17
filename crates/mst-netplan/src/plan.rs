//! Desired rows + actual rows → one OVSDB transaction.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::{Map, Value, json};

use crate::compile::Desired;
use crate::row::{Datum, KEY, Table};

/// A row read from the northbound database (its managed columns).
#[derive(Clone, Debug)]
pub struct ActualRow {
    pub table: Table,
    pub uuid: String,
    pub cols: Map<String, Value>,
}

impl ActualRow {
    fn external_ids(&self) -> BTreeMap<String, String> {
        match canon(self.cols.get("external_ids").unwrap_or(&Value::Null)) {
            Canon::Map(m) => m,
            Canon::Set(_) => BTreeMap::new(),
        }
    }

    fn name(&self) -> Option<&str> {
        self.cols.get("name").and_then(Value::as_str)
    }
}

/// What converging takes.
#[derive(Debug, Default)]
pub struct Plan {
    /// Operations for one `transact` on `OVN_Northbound`; empty when in sync.
    pub ops: Vec<Value>,
    pub inserts: usize,
    pub updates: usize,
    pub deletes: usize,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

/// OVSDB values in a comparable form: a set of tagged atoms, or a map.
/// A lone atom and a one-element set are the same value in OVSDB.
#[derive(Debug, PartialEq, Eq)]
enum Canon {
    Set(BTreeSet<String>),
    Map(BTreeMap<String, String>),
}

fn atom(v: &Value) -> String {
    match v {
        Value::String(s) => format!("s:{s}"),
        Value::Number(n) => format!("n:{n}"),
        Value::Bool(b) => format!("b:{b}"),
        Value::Array(a) if a.len() == 2 && a[0] == "uuid" => {
            format!("u:{}", a[1].as_str().unwrap_or(""))
        }
        Value::Array(a) if a.len() == 2 && a[0] == "named-uuid" => {
            format!("n-u:{}", a[1].as_str().unwrap_or(""))
        }
        other => format!("?:{other}"),
    }
}

fn canon(v: &Value) -> Canon {
    match v {
        Value::Array(a) if a.len() == 2 && a[0] == "set" => Canon::Set(
            a[1].as_array()
                .map(|x| x.iter().map(atom).collect())
                .unwrap_or_default(),
        ),
        Value::Array(a) if a.len() == 2 && a[0] == "map" => Canon::Map(
            a[1].as_array()
                .map(|pairs| {
                    pairs
                        .iter()
                        .filter_map(|p| {
                            Some((
                                p.get(0)?.as_str()?.to_owned(),
                                p.get(1)?.as_str()?.to_owned(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        ),
        Value::Null => Canon::Set(BTreeSet::new()),
        other => Canon::Set(BTreeSet::from([atom(other)])),
    }
}

/// Where a desired row is (or will be): an existing UUID, or the name it
/// gets within the transaction.
#[derive(Clone)]
enum Target {
    Existing(String),
    New(String),
}

impl Target {
    fn json(&self) -> Value {
        match self {
            Target::Existing(u) => json!(["uuid", u]),
            Target::New(n) => json!(["named-uuid", n]),
        }
    }
}

fn encode(d: &Datum, targets: &HashMap<&str, Target>) -> Value {
    let resolve = |k: &String| targets.get(k.as_str()).map(Target::json);
    match d {
        Datum::Str(s) => json!(s),
        Datum::Int(i) => json!(i),
        Datum::Strs(v) => json!(["set", v]),
        Datum::Map(m) => json!([
            "map",
            m.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>()
        ]),
        Datum::IntMap(m) => json!([
            "map",
            m.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>()
        ]),
        Datum::Ref(r) => json!(["set", r.iter().filter_map(resolve).collect::<Vec<_>>()]),
        Datum::Refs(v) => json!(["set", v.iter().filter_map(resolve).collect::<Vec<_>>()]),
    }
}

/// Is this row one of metastratum's (current or from before keys existed)?
fn ours(ids: &BTreeMap<String, String>) -> bool {
    ids.keys().any(|k| k.starts_with("metastratum-"))
}

/// Plans the transaction that makes the database hold exactly the desired
/// rows among metastratum's own. Rows of other owners are never matched,
/// changed or deleted.
pub fn plan(desired: &Desired, actual: &[ActualRow]) -> Plan {
    // Claim existing rows: by key, else (rows from before keys) by name.
    let mut by_key: HashMap<(Table, String), Vec<&ActualRow>> = HashMap::new();
    let mut by_name: HashMap<(Table, &str), Vec<&ActualRow>> = HashMap::new();
    for a in actual {
        let ids = a.external_ids();
        if !ours(&ids) {
            continue;
        }
        match ids.get(KEY) {
            Some(k) => by_key.entry((a.table, k.clone())).or_default().push(a),
            None => {
                if let Some(n) = a.name() {
                    by_name.entry((a.table, n)).or_default().push(a);
                }
            }
        }
    }
    let mut claimed: BTreeSet<&str> = BTreeSet::new();
    let mut existing: HashMap<&str, &ActualRow> = HashMap::new();
    let mut targets: HashMap<&str, Target> = HashMap::new();
    for (i, r) in desired.rows.iter().enumerate() {
        let found = by_key
            .get(&(r.table, r.key.clone()))
            .and_then(|v| v.first())
            .or_else(|| {
                let name = r.str("name").filter(|_| r.table.has_name())?;
                by_name
                    .get(&(r.table, name))?
                    .iter()
                    .find(|a| !claimed.contains(a.uuid.as_str()))
            })
            .filter(|a| !claimed.contains(a.uuid.as_str()));
        match found {
            Some(a) => {
                claimed.insert(&a.uuid);
                existing.insert(&r.key, a);
                targets.insert(&r.key, Target::Existing(a.uuid.clone()));
            }
            None => {
                targets.insert(&r.key, Target::New(format!("row{i}")));
            }
        }
    }

    let mut p = Plan::default();
    for r in &desired.rows {
        let cols: Map<String, Value> = r
            .cols
            .iter()
            .map(|(c, d)| ((*c).to_owned(), encode(d, &targets)))
            .collect();
        match existing.get(r.key.as_str()) {
            Some(a) => {
                let changed: Map<String, Value> = cols
                    .into_iter()
                    .filter(|(c, v)| canon(v) != canon(a.cols.get(c).unwrap_or(&Value::Null)))
                    .collect();
                if !changed.is_empty() {
                    p.ops.push(json!({"op": "update", "table": r.table.name(),
                        "where": [["_uuid", "==", ["uuid", a.uuid]]], "row": changed}));
                    p.updates += 1;
                }
            }
            None => {
                let Target::New(name) = &targets[r.key.as_str()] else {
                    unreachable!()
                };
                p.ops.push(json!({"op": "insert", "table": r.table.name(), "uuid-name": name, "row": cols}));
                p.inserts += 1;
            }
        }
    }
    // Ours but unwanted. Non-root rows go once nothing refers to them; the
    // updates above already drop the references.
    for a in actual {
        if a.table.is_root() && ours(&a.external_ids()) && !claimed.contains(a.uuid.as_str()) {
            p.ops.push(json!({"op": "delete", "table": a.table.name(), "where": [["_uuid", "==", ["uuid", a.uuid]]]}));
            p.deletes += 1;
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use uuid::Uuid;

    use super::*;
    use crate::compile::compile;
    use crate::model::*;

    fn model() -> Model {
        let (t, vpc, net) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        Model {
            vpcs: vec![Vpc {
                id: vpc,
                tenant_id: t,
                gateway: None,
            }],
            networks: vec![Network {
                id: net,
                tenant_id: t,
                vpc_id: vpc,
                cidr: Cidr::parse("10.0.0.0/24").unwrap(),
                gateway: "10.0.0.1".parse().unwrap(),
                mtu: 1442,
                monitor_ip: None,
            }],
            ports: vec![Port {
                id: Uuid::from_u128(4),
                vm_id: Uuid::from_u128(5),
                tenant_id: t,
                tenant_lineage: vec![t],
                network_id: net,
                mac: "02:00:00:00:00:01".into(),
                ip: Ipv4Addr::new(10, 0, 0, 2),
                security_groups: vec![],
                chassis: vec![],
                migrating: false,
                bandwidth: None,
            }],
            ..Model::default()
        }
    }

    /// What the database would hold after applying a plan's inserts: rows
    /// as the server returns them (lone atoms unwrapped, UUIDs assigned).
    fn apply_inserts(p: &Plan) -> Vec<ActualRow> {
        let uuid_of = |name: &str| {
            format!(
                "00000000-0000-0000-0000-{:012}",
                name.trim_start_matches("row").parse::<u64>().unwrap()
            )
        };
        let fix = |v: &Value| -> Value {
            let s = v.to_string();
            let s = regex_lite_named(&s, &uuid_of);
            let v: Value = serde_json::from_str(&s).unwrap();
            // The server returns one-element sets as the bare atom.
            match &v {
                Value::Array(a) if a[0] == "set" && a[1].as_array().unwrap().len() == 1 => {
                    a[1][0].clone()
                }
                _ => v,
            }
        };
        p.ops
            .iter()
            .filter(|o| o["op"] == "insert")
            .map(|o| ActualRow {
                table: Table::ALL
                    .into_iter()
                    .find(|t| t.name() == o["table"])
                    .unwrap(),
                uuid: uuid_of(o["uuid-name"].as_str().unwrap()),
                cols: o["row"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), fix(v)))
                    .collect(),
            })
            .collect()
    }

    /// Replaces `["named-uuid","rowN"]` with `["uuid","<uuid>"]`.
    fn regex_lite_named(s: &str, uuid_of: &dyn Fn(&str) -> String) -> String {
        let mut out = String::new();
        let mut rest = s;
        let pat = "[\"named-uuid\",\"";
        while let Some(i) = rest.find(pat) {
            out += &rest[..i];
            let tail = &rest[i + pat.len()..];
            let end = tail.find('"').unwrap();
            out += &format!("[\"uuid\",\"{}\"", uuid_of(&tail[..end]));
            rest = &tail[end + 1..];
        }
        out + rest
    }

    #[test]
    fn an_empty_database_gets_everything_in_one_transaction() {
        let d = compile(&model());
        let p = plan(&d, &[]);
        assert_eq!(p.inserts, d.rows.len());
        assert_eq!((p.updates, p.deletes), (0, 0));
    }

    #[test]
    fn a_converged_database_needs_nothing() {
        let d = compile(&model());
        let after = apply_inserts(&plan(&d, &[]));
        let p = plan(&d, &after);
        assert!(p.is_empty(), "{:#?}", p.ops);
    }

    #[test]
    fn changes_touch_only_what_changed() {
        let mut m = model();
        let before = apply_inserts(&plan(&compile(&m), &[]));
        m.ports[0].ip = Ipv4Addr::new(10, 0, 0, 9);
        let p = plan(&compile(&m), &before);
        assert_eq!((p.inserts, p.updates, p.deletes), (0, 1, 0), "{:#?}", p.ops);
        assert_eq!(
            p.ops[0]["row"]["addresses"],
            json!(["set", ["02:00:00:00:00:01 10.0.0.9"]])
        );
        assert!(p.ops[0]["row"].get("name").is_none());
    }

    #[test]
    fn removed_things_go_and_foreign_rows_stay() {
        let m = model();
        let mut before = apply_inserts(&plan(&compile(&m), &[]));
        before.push(ActualRow {
            table: Table::LogicalSwitch,
            uuid: "f0000000-0000-0000-0000-000000000000".into(),
            cols: Map::from_iter([("name".to_owned(), json!("someone-elses"))]),
        });
        let p = plan(&compile(&Model::default()), &before);
        // Root rows of ours are deleted; ports and ACLs follow by reference.
        let deleted: Vec<&str> = p
            .ops
            .iter()
            .filter(|o| o["op"] == "delete")
            .map(|o| o["table"].as_str().unwrap())
            .collect();
        assert!(deleted.contains(&"Logical_Switch") && deleted.contains(&"Logical_Router"));
        assert!(
            !p.ops
                .to_vec()
                .iter()
                .any(|o| o.to_string().contains("f0000000")),
            "foreign row touched"
        );
        // The default-deny port group stays, now empty.
        assert_eq!(p.inserts, 0);
    }

    #[test]
    fn rows_from_before_keys_are_adopted_by_name() {
        let m = model();
        let net = m.networks[0].id;
        let legacy = ActualRow {
            table: Table::LogicalSwitch,
            uuid: "aaaaaaaa-0000-0000-0000-000000000000".into(),
            cols: Map::from_iter([
                ("name".to_owned(), json!(format!("mst-net-{net}"))),
                ("ports".to_owned(), json!(["set", []])),
                (
                    "external_ids".to_owned(),
                    json!(["map", [["metastratum-id", net.to_string()]]]),
                ),
            ]),
        };
        let p = plan(&compile(&m), &[legacy]);
        let update = p
            .ops
            .iter()
            .find(|o| o["op"] == "update")
            .expect("adopted, not recreated");
        assert_eq!(update["table"], "Logical_Switch");
        assert!(!p.ops.iter().any(|o| o["op"] == "insert"
            && o["table"] == "Logical_Switch"
            && o["row"]["name"] == json!(format!("mst-net-{net}"))));
    }

    #[test]
    fn duplicates_of_a_key_are_removed() {
        let d = compile(&model());
        let mut after = apply_inserts(&plan(&d, &[]));
        let mut dup = after
            .iter()
            .find(|a| a.table == Table::DhcpOptions)
            .unwrap()
            .clone();
        dup.uuid = "dddddddd-0000-0000-0000-000000000000".into();
        after.push(dup);
        let p = plan(&d, &after);
        assert_eq!((p.inserts, p.updates, p.deletes), (0, 0, 1));
    }
}
