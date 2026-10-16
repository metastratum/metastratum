//! OVN northbound operations for hypermon networks.
//!
//! Every row hypermon creates carries `external_ids:hypermon-*` keys; rows
//! without them are never modified. Inserts are guarded by a `wait` operation
//! asserting the row does not exist yet, so concurrent controllers cannot
//! create duplicates: the loser's transaction fails and it re-reads.

use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::client::{Client, Error, Result, enc};

const DB: &str = "OVN_Northbound";
const ID: &str = "hypermon-id";
const TENANT: &str = "hypermon-tenant";
const NETWORK: &str = "hypermon-network";
const VM: &str = "hypermon-vm";

/// Geneve adds 58 bytes (IPv4 underlay with options); this keeps guests
/// within a standard 1500-byte underlay.
pub const DEFAULT_MTU: u32 = 1442;

#[derive(Clone, Debug)]
pub struct NetworkSpec<'a> {
    pub id: &'a str,
    pub tenant_id: &'a str,
    pub cidr: &'a str,
    pub gateway: &'a str,
    pub mtu: u32,
}

#[derive(Clone, Debug)]
pub struct PortSpec<'a> {
    /// The NIC ID; used as the logical switch port name and OVS iface-id.
    pub name: &'a str,
    pub network_id: &'a str,
    pub tenant_id: &'a str,
    pub vm_id: &'a str,
    pub mac: &'a str,
    pub ip: &'a str,
}

pub fn switch_name(network_id: &str) -> String {
    format!("hm-net-{network_id}")
}

/// MAC the OVN DHCP server answers from, derived from the network ID.
fn dhcp_server_mac(network_id: &str) -> String {
    let hex: String = network_id.chars().filter(char::is_ascii_hexdigit).collect();
    let tail = &hex[hex.len().saturating_sub(8)..];
    format!(
        "02:69:{}:{}:{}:{}",
        &tail[0..2],
        &tail[2..4],
        &tail[4..6],
        &tail[6..8]
    )
}

/// A northbound connection that reconnects lazily after failures.
pub struct Northbound {
    addr: String,
    client: Mutex<Option<Arc<Client>>>,
}

impl Northbound {
    pub fn new(addr: impl Into<String>) -> Self {
        Self {
            addr: addr.into(),
            client: Mutex::new(None),
        }
    }

    async fn client(&self) -> Result<Arc<Client>> {
        let mut slot = self.client.lock().await;
        if let Some(c) = slot.as_ref().filter(|c| !c.is_closed()) {
            return Ok(c.clone());
        }
        let c = Arc::new(Client::connect(&self.addr).await?);
        *slot = Some(c.clone());
        Ok(c)
    }

    async fn tx(&self, ops: Vec<Value>) -> Result<Vec<Value>> {
        let c = self.client().await?;
        match c.transact(DB, ops.clone()).await {
            // The connection dropped or went silent (server restarted, NB
            // leader change). Retry once on a fresh connection; every
            // transaction here is safe to repeat.
            Err(Error::Closed | Error::Timeout(_)) => {
                *self.client.lock().await = None;
                self.client().await?.transact(DB, ops).await
            }
            r => r,
        }
    }

    async fn select_uuids(&self, table: &str, cond: Value) -> Result<Vec<String>> {
        let res = self
            .tx(vec![
                json!({"op": "select", "table": table, "where": [cond], "columns": ["_uuid"]}),
            ])
            .await?;
        Ok(res[0]["rows"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| enc::as_uuid(&r["_uuid"]).map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn one_uuid(&self, table: &str, cond: Value) -> Result<Option<String>> {
        Ok(self.select_uuids(table, cond).await?.into_iter().next())
    }

    /// Inserts `row` into `table` unless a row matching `cond` exists.
    /// Returns the row's UUID either way.
    async fn insert_unless_exists(&self, table: &str, cond: Value, row: Value) -> Result<String> {
        if let Some(u) = self.one_uuid(table, cond.clone()).await? {
            return Ok(u);
        }
        let ops = vec![
            json!({"op": "wait", "table": table, "where": [cond.clone()], "columns": ["_uuid"],
                   "until": "==", "rows": [], "timeout": 0}),
            json!({"op": "insert", "table": table, "row": row}),
        ];
        match self.tx(ops).await {
            Ok(res) => enc::as_uuid(&res[1]["uuid"])
                .map(str::to_owned)
                .ok_or_else(|| Error::Protocol(res[1].to_string())),
            // Lost a race with another inserter: the row exists now.
            Err(Error::Transaction(e)) if e.contains("timed out") => self
                .one_uuid(table, cond)
                .await?
                .ok_or(Error::Transaction(e)),
            Err(e) => Err(e),
        }
    }

    /// Ensures the logical switch and DHCP options for a network exist.
    pub async fn ensure_network(&self, n: &NetworkSpec<'_>) -> Result<()> {
        let ids = || enc::map([(ID, n.id.to_owned()), (TENANT, n.tenant_id.to_owned())]);
        self.insert_unless_exists(
            "Logical_Switch",
            enc::has_external_id(ID, n.id),
            json!({"name": switch_name(n.id), "external_ids": ids()}),
        )
        .await?;
        self.insert_unless_exists(
            "DHCP_Options",
            enc::has_external_id(NETWORK, n.id),
            json!({
                "cidr": n.cidr,
                "options": enc::map([
                    ("server_id", n.gateway.to_owned()),
                    ("server_mac", dhcp_server_mac(n.id)),
                    ("lease_time", "3600".to_owned()),
                    ("router", n.gateway.to_owned()),
                    ("mtu", n.mtu.to_string()),
                ]),
                "external_ids": enc::map([(NETWORK, n.id.to_owned()), (TENANT, n.tenant_id.to_owned())]),
            }),
        )
        .await?;
        Ok(())
    }

    /// Ensures a VM port exists on its network's switch, with port security
    /// pinning it to its MAC and IP (no spoofing between tenants' VMs).
    pub async fn ensure_port(&self, p: &PortSpec<'_>) -> Result<()> {
        if self
            .one_uuid("Logical_Switch_Port", json!(["name", "==", p.name]))
            .await?
            .is_some()
        {
            return Ok(());
        }
        let ls = self
            .one_uuid("Logical_Switch", enc::has_external_id(ID, p.network_id))
            .await?
            .ok_or_else(|| {
                Error::Transaction(format!("network {} has no logical switch", p.network_id))
            })?;
        let dhcp = self
            .one_uuid("DHCP_Options", enc::has_external_id(NETWORK, p.network_id))
            .await?;
        let address = format!("{} {}", p.mac, p.ip);
        let ops = vec![
            json!({"op": "insert", "table": "Logical_Switch_Port", "uuid-name": "port", "row": {
                "name": p.name,
                "addresses": address,
                "port_security": address,
                "dhcpv4_options": enc::set(dhcp.as_deref().map(enc::uuid)),
                "external_ids": enc::map([
                    (ID, p.name.to_owned()),
                    (TENANT, p.tenant_id.to_owned()),
                    (VM, p.vm_id.to_owned()),
                ]),
            }}),
            json!({"op": "mutate", "table": "Logical_Switch", "where": [["_uuid", "==", enc::uuid(&ls)]],
                   "mutations": [["ports", "insert", enc::set([enc::named("port")])]]}),
        ];
        match self.tx(ops).await {
            Ok(_) => Ok(()),
            // A concurrent ensure_port won; the name index guarantees one port.
            Err(Error::Transaction(e)) if e.contains("constraint violation") => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Removes a VM port. Succeeds if it does not exist.
    pub async fn delete_port(&self, name: &str) -> Result<()> {
        let Some(port) = self
            .one_uuid("Logical_Switch_Port", json!(["name", "==", name]))
            .await?
        else {
            return Ok(());
        };
        // Ports are non-root rows: removing the reference deletes them.
        self.tx(vec![json!({"op": "mutate", "table": "Logical_Switch",
            "where": [["ports", "includes", enc::uuid(&port)]],
            "mutations": [["ports", "delete", enc::set([enc::uuid(&port)])]]})])
            .await?;
        Ok(())
    }

    /// Removes a network's switch and DHCP options. Succeeds if absent.
    pub async fn delete_network(&self, id: &str) -> Result<()> {
        self.tx(vec![
            json!({"op": "delete", "table": "Logical_Switch", "where": [enc::has_external_id(ID, id)]}),
            json!({"op": "delete", "table": "DHCP_Options", "where": [enc::has_external_id(NETWORK, id)]}),
        ])
        .await?;
        Ok(())
    }

    /// True once ovn-controller on the VM's host has bound the port, i.e. the
    /// VM's interface is plugged in and the network path is programmed.
    pub async fn port_up(&self, name: &str) -> Result<bool> {
        let res = self
            .tx(vec![json!({"op": "select", "table": "Logical_Switch_Port",
                "where": [["name", "==", name]], "columns": ["up"]})])
            .await?;
        Ok(res[0]["rows"][0]["up"] == json!(true)
            || res[0]["rows"][0]["up"] == json!(["set", [true]]))
    }
}
