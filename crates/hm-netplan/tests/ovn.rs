//! Plans applied to a real OVN northbound database: the schema accepts every
//! row, a second plan finds nothing to do, and removal leaves nothing behind.
//!
//! Needs an unused northbound database, since it owns every hypermon row in
//! it: `deploy/lab/lab.sh testdb`, then
//! `HM_TEST_OVN_NB_SCRATCH=tcp:10.77.0.10:6651 cargo test -p hm-netplan`.

use std::net::Ipv4Addr;

use hm_netplan::*;
use hm_ovsdb::Northbound;
use uuid::Uuid;

async fn actual(nb: &Northbound) -> Vec<ActualRow> {
    let mut out = vec![];
    for t in Table::ALL {
        for mut row in nb.select_all(t.name(), t.columns()).await.unwrap() {
            let uuid = row.remove("_uuid").unwrap()[1].as_str().unwrap().to_owned();
            out.push(ActualRow {
                table: t,
                uuid,
                cols: row,
            });
        }
    }
    out
}

async fn converge(nb: &Northbound, m: &Model) -> Plan {
    let d = compile(m);
    let p = plan(&d, &actual(nb).await);
    if !p.is_empty() {
        let res = nb.transact(p.ops.clone()).await.expect("transaction");
        // Per-operation errors come back as objects with "error".
        assert!(!res.iter().any(|r| r.get("error").is_some()), "{res:#?}");
    }
    p
}

fn model() -> Model {
    let id = |_| Uuid::now_v7();
    let (root, dev) = (id(()), id(()));
    let (vpc, net, ext, sg, web) = (id(()), id(()), id(()), id(()), id(()));
    let port = |ip: [u8; 4], groups: Vec<Uuid>| Port {
        id: id(()),
        vm_id: id(()),
        tenant_id: dev,
        tenant_lineage: vec![root, dev],
        network_id: net,
        mac: format!("02:00:00:00:00:{:02x}", ip[3]),
        ip: Ipv4Addr::from(ip),
        security_groups: groups,
        chassis: vec![],
        migrating: false,
        bandwidth: None,
    };
    let mut ports = vec![
        port([10, 0, 0, 2], vec![sg]),
        port([10, 0, 0, 3], vec![sg, web]),
    ];
    // One rate-limited port.
    ports[1].bandwidth = Some(Bandwidth { kbps: 100_000 });
    let rule = |direction, protocol, ports, remote| {
        (
            id(()),
            Match {
                direction,
                protocol,
                ports,
                remote,
            },
        )
    };
    Model {
        external_networks: vec![ExternalNetwork {
            id: ext,
            physical_network: "external".into(),
            cidr: Cidr::parse("10.78.0.0/24").unwrap(),
            gateway: "10.78.0.1".parse().unwrap(),
        }],
        vpcs: vec![Vpc {
            id: vpc,
            tenant_id: dev,
            gateway: Some(VpcGateway {
                external_network_id: ext,
                ip: "10.78.0.230".parse().unwrap(),
            }),
        }],
        networks: vec![Network {
            id: net,
            tenant_id: dev,
            vpc_id: vpc,
            cidr: Cidr::parse("10.0.0.0/24").unwrap(),
            gateway: "10.0.0.1".parse().unwrap(),
            mtu: 1442,
        }],
        floating_ips: vec![FloatingIp {
            id: id(()),
            external_network_id: ext,
            ip: "10.78.0.231".parse().unwrap(),
            port: Some(ports[1].id),
        }],
        ports,
        security_groups: vec![
            SecurityGroup {
                id: sg,
                tenant_id: dev,
                rules: vec![
                    rule(
                        Direction::Egress,
                        Protocol::Any,
                        None,
                        Remote::Cidr(Cidr::parse("0.0.0.0/0").unwrap()),
                    ),
                    rule(Direction::Ingress, Protocol::Any, None, Remote::Group(sg)),
                ],
            },
            SecurityGroup {
                id: web,
                tenant_id: dev,
                rules: vec![rule(
                    Direction::Ingress,
                    Protocol::Tcp,
                    Some((80, 443)),
                    Remote::Cidr(Cidr::parse("0.0.0.0/0").unwrap()),
                )],
            },
        ],
        policies: vec![Policy {
            id: id(()),
            tenant_id: root,
            rule: Match {
                direction: Direction::Egress,
                protocol: Protocol::Tcp,
                ports: Some((25, 25)),
                remote: Remote::Cidr(Cidr::parse("0.0.0.0/0").unwrap()),
            },
        }],
        chassis: vec![Chassis {
            name: "hm-host1".into(),
            physical_networks: vec!["external".into()],
        }],
    }
}

#[tokio::test]
async fn plans_converge_a_real_northbound_database() {
    let Ok(addr) = std::env::var("HM_TEST_OVN_NB_SCRATCH") else {
        eprintln!("HM_TEST_OVN_NB_SCRATCH not set; skipping");
        return;
    };
    let nb = Northbound::new(&addr, None);
    // Start from nothing of ours.
    converge(&nb, &Model::default()).await;

    let mut m = model();
    let first = converge(&nb, &m).await;
    assert!(first.inserts > 20, "{first:?}");
    let again = converge(&nb, &m).await;
    assert!(again.is_empty(), "not converged: {:#?}", again.ops);

    // A change is a small update.
    m.ports[0].ip = Ipv4Addr::new(10, 0, 0, 9);
    m.security_groups[1].rules.clear();
    m.ports[1].bandwidth = Some(Bandwidth { kbps: 20_000 });
    let change = converge(&nb, &m).await;
    assert_eq!(change.inserts, 0, "{:#?}", change.ops);
    assert!(converge(&nb, &m).await.is_empty());

    // Removing everything leaves only the (now empty) default-deny group.
    converge(&nb, &Model::default()).await;
    let left: Vec<_> = actual(&nb).await;
    let tables: Vec<&str> = left.iter().map(|r| r.table.name()).collect();
    assert!(
        tables.iter().all(|t| *t == "Port_Group" || *t == "ACL"),
        "left behind: {tables:?}"
    );
    assert_eq!(tables.iter().filter(|t| **t == "Port_Group").count(), 1);
}
