//! Connecting to real northbound databases: over TLS, and to the leader of
//! a cluster. `deploy/lab/lab.sh testdb` starts them and prints the
//! variables these tests read (`MST_TEST_OVN_NB_SSL`, `MST_TEST_OVN_NB_PKI`,
//! `MST_TEST_OVN_NB_CLUSTER`); without them the tests skip.

use std::path::PathBuf;
use std::sync::Arc;

use mst_ovsdb::{Client, Error, Northbound};
use serde_json::json;

fn env(name: &str) -> Option<String> {
    let v = std::env::var(name).ok();
    if v.is_none() {
        eprintln!("{name} not set; skipping");
    }
    v
}

fn read(pki: &str, f: &str) -> Vec<u8> {
    std::fs::read(PathBuf::from(pki).join(f)).unwrap()
}

fn tls(pki: &str) -> Arc<rustls::ClientConfig> {
    mst_ovsdb::tls::client_config(
        &read(pki, "ca.pem"),
        &read(pki, "client.pem"),
        &read(pki, "client-key.pem"),
    )
    .unwrap()
}

#[tokio::test]
async fn tls_with_the_servers_ca_only() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (Some(addr), Some(pki)) = (env("MST_TEST_OVN_NB_SSL"), env("MST_TEST_OVN_NB_PKI")) else {
        return;
    };
    // The server's certificate names nothing (as ovs-pki's); the CA vouches.
    let nb = Northbound::new(&addr, Some(tls(&pki)));
    nb.select_all("Logical_Switch", &["name"]).await.unwrap();
    assert_eq!(nb.remote().await.as_deref(), Some(addr.as_str()));

    // Another CA's word is not enough, even with a valid client certificate.
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec![]).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let other_ca = params.self_signed(&key).unwrap().pem();
    let wrong = mst_ovsdb::tls::client_config(
        other_ca.as_bytes(),
        &read(&pki, "client.pem"),
        &read(&pki, "client-key.pem"),
    )
    .unwrap();
    let err = Client::connect(&addr, Some(&wrong)).await.err().unwrap();
    assert!(matches!(err, Error::Tls(_)), "{err}");

    // ssl: without TLS configuration is refused up front.
    let err = Client::connect(&addr, None).await.err().unwrap();
    assert!(matches!(err, Error::Tls(_)), "{err}");

    // What the server presents can be seen without a client certificate,
    // and judged against either CA.
    let seen = mst_ovsdb::tls::presented(&addr, &read(&pki, "ca.pem"))
        .await
        .unwrap();
    assert!(seen.trusted.is_ok(), "{:?}", seen.trusted);
    assert!(!seen.certificate.is_empty());
    let seen = mst_ovsdb::tls::presented(&addr, other_ca.as_bytes())
        .await
        .unwrap();
    assert!(seen.trusted.is_err(), "trusted by another CA");
}

#[tokio::test]
async fn only_the_cluster_leader_is_used() {
    let Some(cluster) = env("MST_TEST_OVN_NB_CLUSTER") else {
        return;
    };
    let servers: Vec<&str> = cluster.split(',').collect();
    // Which one leads, asked directly.
    let mut leaders = vec![];
    for s in &servers {
        let c = Client::connect(s, None).await.unwrap();
        let r = c
            .transact(
                "_Server",
                vec![json!({"op": "select", "table": "Database",
                    "where": [["name", "==", "OVN_Northbound"]], "columns": ["leader"]})],
            )
            .await
            .unwrap();
        if r[0]["rows"][0]["leader"] == true {
            leaders.push(s.to_string());
        }
    }
    assert_eq!(leaders.len(), 1, "{leaders:?}");

    // Whatever the order, and past a remote that is down.
    for first in 0..servers.len() {
        let mut order: Vec<&str> = vec!["tcp:10.77.0.10:1"];
        order.extend(servers.iter().cycle().skip(first).take(servers.len()));
        let nb = Northbound::new(&order.join(","), None);
        let name = format!("mst-remotes-{first}-{}", std::process::id());
        nb.transact(vec![
            json!({"op": "insert", "table": "Logical_Switch", "row": {"name": name}}),
        ])
        .await
        .unwrap();
        assert_eq!(nb.remote().await, Some(leaders[0].clone()));
        // What was written is read back at once.
        let rows = nb.select_all("Logical_Switch", &["name"]).await.unwrap();
        assert!(rows.iter().any(|r| r["name"] == name.as_str()));
        nb.transact(vec![json!({"op": "delete", "table": "Logical_Switch",
            "where": [["name", "==", name]]})])
            .await
            .unwrap();
    }
}
