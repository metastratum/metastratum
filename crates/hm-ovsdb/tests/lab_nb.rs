//! Runs against a real OVN northbound database when `HM_TEST_OVN_NB` is set,
//! e.g. `HM_TEST_OVN_NB=tcp:10.77.0.10:6641` with the dev lab running.
//! Skipped otherwise.

use hm_ovsdb::{NetworkSpec, Northbound, PortSpec};

#[tokio::test]
async fn network_and_port_lifecycle() {
    let Ok(addr) = std::env::var("HM_TEST_OVN_NB") else {
        eprintln!("HM_TEST_OVN_NB not set; skipping");
        return;
    };
    let nb = Northbound::new(addr.clone());
    let net_id = format!("test-{}", std::process::id());
    let port = format!("{net_id}-p1");
    let net = NetworkSpec {
        id: &net_id,
        tenant_id: "t",
        cidr: "10.99.0.0/24",
        gateway: "10.99.0.1",
        mtu: 1442,
    };

    // Concurrent ensures must converge on one switch.
    let (a, b) = tokio::join!(nb.ensure_network(&net), nb.ensure_network(&net));
    a.unwrap();
    b.unwrap();
    nb.ensure_network(&net).await.unwrap();

    let p = PortSpec {
        name: &port,
        network_id: &net_id,
        tenant_id: "t",
        vm_id: "vm",
        mac: "02:68:00:00:00:01",
        ip: "10.99.0.2",
    };
    nb.ensure_port(&p).await.unwrap();
    nb.ensure_port(&p).await.unwrap();
    assert!(!nb.port_up(&port).await.unwrap(), "nothing is plugged in");

    nb.delete_port(&port).await.unwrap();
    nb.delete_port(&port).await.unwrap();
    nb.delete_network(&net_id).await.unwrap();
    eprintln!("network/port lifecycle ok against {addr}");
}
