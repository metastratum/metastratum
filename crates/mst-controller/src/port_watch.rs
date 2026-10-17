//! Watches where OVN binds VMs' ports. OVN's southbound access control lets
//! any host claim any port (the host role may write a port binding's
//! chassis), so a host taken over could pull another tenant's traffic to
//! itself. The controller knows where each VM runs: a port bound anywhere
//! else is reported, in the log, the `mst_port_binding_mismatches` metric and
//! the audit log. (ovn-controller on the right host claims the port back.)

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::state::AppState;

const EVERY: Duration = Duration::from_secs(60);
/// How long the watcher's own southbound certificate is used before a new
/// one is made.
const CERT_FOR: Duration = Duration::from_secs(86400);

/// A port bound somewhere it should not be.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Mismatch {
    pub port: Uuid,
    pub vm: Uuid,
    pub tenant: Uuid,
    pub expected: Vec<String>,
    pub actual: String,
}

/// Ports bound to a chassis they were not placed on. `bound` maps ports
/// (by name) to the chassis holding them; unbound ports are not in it.
pub fn mismatches(ports: &[mst_netplan::Port], bound: &HashMap<String, String>) -> Vec<Mismatch> {
    ports
        .iter()
        .filter(|p| !p.chassis.is_empty())
        .filter_map(|p| {
            let actual = bound.get(&p.id.to_string())?;
            (!p.chassis.contains(actual)).then(|| Mismatch {
                port: p.id,
                vm: p.vm_id,
                tenant: p.tenant_id,
                expected: p.chassis.clone(),
                actual: actual.clone(),
            })
        })
        .collect()
}

/// Runs on the leader (see [`crate::ha::lead`]).
pub async fn watch(state: AppState, remotes: Vec<String>) {
    if remotes.is_empty() {
        return std::future::pending().await;
    }
    let mut seen: HashSet<Mismatch> = HashSet::new();
    let mut tls: Option<(Arc<rustls::ClientConfig>, Instant)> = None;
    loop {
        tokio::time::sleep(EVERY).await;
        if tls.as_ref().is_none_or(|(_, at)| at.elapsed() > CERT_FOR) {
            match client_tls(&state) {
                Ok(t) => tls = Some((t, Instant::now())),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "a certificate for watching OVN port bindings");
                    continue;
                }
            }
        }
        let config = tls.as_ref().map(|(t, _)| t.clone());
        match check(&state, &remotes, config.as_ref(), &mut seen).await {
            Ok(()) => metrics::gauge!("mst_port_binding_mismatches").set(seen.len() as f64),
            Err(e) => tracing::debug!(error = %format!("{e:#}"), "checking OVN port bindings"),
        }
    }
}

/// Reading the southbound database takes a certificate from the OVN CA,
/// like hosts'; OVN's access control limits what it may write, not read.
fn client_tls(state: &AppState) -> anyhow::Result<Arc<rustls::ClientConfig>> {
    let (cert, key) = state
        .ovn_ca
        .issue_client(crate::ca::SERVER_NAME, CERT_FOR * 2)?;
    Ok(mst_ovsdb::tls::client_config(
        state.ovn_ca.cert_pem().as_bytes(),
        cert.as_bytes(),
        key.as_bytes(),
    )?)
}

async fn check(
    state: &AppState,
    remotes: &[String],
    tls: Option<&Arc<rustls::ClientConfig>>,
    seen: &mut HashSet<Mismatch>,
) -> anyhow::Result<()> {
    let mut conn = state.db.acquire().await?;
    let model = mst_db::netmodel::load(&mut conn, mst_ovsdb::nb::DEFAULT_MTU).await?;
    drop(conn);
    let bound = bindings(remotes, tls).await?;
    let now: HashSet<Mismatch> = mismatches(&model.ports, &bound).into_iter().collect();
    for m in now.difference(seen) {
        tracing::error!(
            port = %m.port, vm = %m.vm, expected = ?m.expected, actual = %m.actual,
            "a VM's port is bound to a host it does not run on: that host may have been taken over"
        );
        let entry = mst_db::audit::Entry {
            user_id: None,
            username: None,
            tenant_id: Some(mst_core::ids::TenantId(m.tenant)),
            action: "network.port_binding_mismatch",
            target: Some(("vm", m.vm)),
            success: false,
            details: json!({"port": m.port, "expected": m.expected, "bound_to": m.actual}),
        };
        let _ = mst_db::audit::record(&state.db, entry).await;
    }
    for m in seen.difference(&now) {
        tracing::info!(port = %m.port, vm = %m.vm, "the port is bound where its VM runs again");
    }
    *seen = now;
    Ok(())
}

/// Which chassis (by name) holds each VM port (by name).
async fn bindings(
    remotes: &[String],
    tls: Option<&Arc<rustls::ClientConfig>>,
) -> anyhow::Result<HashMap<String, String>> {
    let mut last = None;
    for remote in remotes {
        match read_bindings(remote, tls).await {
            Ok(b) => return Ok(b),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no southbound servers")))
}

async fn read_bindings(
    remote: &str,
    tls: Option<&Arc<rustls::ClientConfig>>,
) -> anyhow::Result<HashMap<String, String>> {
    let client = mst_ovsdb::Client::connect(remote, tls).await?;
    let res = client
        .transact(
            "OVN_Southbound",
            vec![
                json!({"op": "select", "table": "Chassis", "where": [], "columns": ["_uuid", "name"]}),
                json!({"op": "select", "table": "Port_Binding", "where": [["type", "==", ""]],
                       "columns": ["logical_port", "chassis"]}),
            ],
        )
        .await
        .with_context(|| format!("reading {remote}"))?;
    let rows = |i: usize| -> Vec<Value> {
        res.get(i)
            .and_then(|r| r["rows"].as_array().cloned())
            .unwrap_or_default()
    };
    let uuid = |v: &Value| match v {
        Value::Array(a) if a.first().and_then(Value::as_str) == Some("uuid") => {
            a.get(1).and_then(Value::as_str).map(str::to_owned)
        }
        _ => None,
    };
    let chassis: HashMap<String, String> = rows(0)
        .iter()
        .filter_map(|r| Some((uuid(&r["_uuid"])?, r["name"].as_str()?.to_owned())))
        .collect();
    Ok(rows(1)
        .iter()
        .filter_map(|r| {
            let port = r["logical_port"].as_str()?.to_owned();
            let name = chassis.get(&uuid(&r["chassis"])?)?.clone();
            Some((port, name))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(id: u128, chassis: &[&str]) -> mst_netplan::Port {
        mst_netplan::Port {
            id: Uuid::from_u128(id),
            vm_id: Uuid::from_u128(id + 100),
            tenant_id: Uuid::nil(),
            tenant_lineage: vec![],
            network_id: Uuid::nil(),
            mac: String::new(),
            ip: "10.0.0.2".parse().unwrap(),
            security_groups: vec![],
            chassis: chassis.iter().map(|c| (*c).to_owned()).collect(),
            migrating: chassis.len() > 1,
            bandwidth: None,
        }
    }

    #[test]
    fn ports_bound_elsewhere_are_found() {
        let ports = [
            port(1, &["h1"]),
            port(2, &["h1", "h2"]),
            port(3, &["h2"]),
            port(4, &["h3"]),
            port(5, &[]),
        ];
        let bound: HashMap<String, String> = [
            (1, "h1"), // where it runs
            (2, "h2"), // migrating there
            (3, "h9"), // taken
            (5, "h9"), // placed nowhere: anything goes
        ]
        .into_iter()
        .map(|(p, c)| (Uuid::from_u128(p).to_string(), c.to_owned()))
        .collect();
        let found = mismatches(&ports, &bound);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            (found[0].port, found[0].actual.as_str()),
            (Uuid::from_u128(3), "h9")
        );
    }
}
