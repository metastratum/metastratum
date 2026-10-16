//! Cleaning up on hosts and storage after deletions, which the database
//! alone would leave behind: a deleted pool stays mounted and its data
//! stays on the storage, a deleted image stays cached, a decommissioned
//! host stays an OVN chassis.
//!
//! Hosts connected at the time clean up at once; the others when they next
//! connect (the controller's welcome lists what still exists).

use std::time::Duration;

use hm_proto::command::Cmd;
use serde_json::json;

use crate::state::AppState;

/// What exists, for agents to let go of the rest.
pub async fn inventory(db: &hm_db::sqlx::PgPool) -> anyhow::Result<hm_proto::Inventory> {
    Ok(hm_proto::Inventory {
        pool_ids: hm_db::storage_pools::list(db)
            .await?
            .into_iter()
            .map(|p| p.id.to_string())
            .collect(),
        image_ids: hm_db::images::all_ids(db)
            .await?
            .into_iter()
            .map(|i| i.to_string())
            .collect(),
    })
}

/// Identifies the storage behind a pool: two pools with the same one share
/// their data (the same NFS export, or the same Ceph pool).
pub fn backend(pool: &hm_db::storage_pools::Pool) -> Option<String> {
    let c = &pool.config;
    let s = |k: &str| c.get(k).and_then(|v| v.as_str()).unwrap_or("");
    match pool.kind.as_str() {
        "nfs" => Some(format!(
            "nfs:{}:{}",
            s("server"),
            s("export").trim_end_matches('/')
        )),
        "rbd" => {
            let mut monitors: Vec<&str> = c
                .get("monitors")
                .and_then(|m| m.as_array())
                .map(|m| m.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            monitors.sort_unstable();
            Some(format!("rbd:{}@{}", s("pool"), monitors.join(",")))
        }
        _ => None,
    }
}

async fn live_hosts(state: &AppState) -> Vec<hm_db::hosts::Host> {
    hm_db::hosts::list(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|h| h.state != "decommissioned" && h.is_live())
        .collect()
}

async fn record(
    state: &AppState,
    action: &str,
    target: (&str, uuid::Uuid),
    details: serde_json::Value,
) {
    // Under the provider tenant, where pools and images are managed.
    let root = hm_db::tenants::root(&state.db)
        .await
        .ok()
        .flatten()
        .map(|t| t.id);
    crate::authz::audit(&state.db, None, root, action, Some(target), details).await;
}

/// After a pool was deleted: hosts let go of it, and one removes
/// hypermon's data from its storage unless another pool still uses it.
pub async fn pool_deleted(state: AppState, pool: hm_db::storage_pools::Pool, shared: bool) {
    let spec = match crate::api::agent_pool(&pool) {
        Ok(s) => s,
        Err(e) => {
            return tracing::error!(pool = %pool.id, error = %format!("{e:#}"), "pool cleanup");
        }
    };
    let hosts = live_hosts(&state).await;
    let mut done = vec![];
    let mut failed = vec![];
    let mut data = if shared {
        json!("kept: another pool uses the same storage")
    } else if hosts.is_empty() {
        json!("kept: no host was connected to remove it")
    } else {
        serde_json::Value::Null
    };
    for h in &hosts {
        // The first host that can reach it removes the data.
        let remove_data = data.is_null();
        let cmd = Cmd::RemovePool(hm_proto::RemovePool {
            pool: Some(spec.clone()),
            remove_data,
        });
        match state.agents.send(h.id, cmd, Duration::from_secs(600)).await {
            Ok(payload) => {
                done.push(h.name.clone());
                if remove_data {
                    let removed = serde_json::from_str::<serde_json::Value>(&payload)
                        .ok()
                        .and_then(|v| v["removed"].as_u64())
                        .unwrap_or(0);
                    data = json!(format!("{removed} files removed by {}", h.name));
                }
            }
            Err(e) => failed.push(format!("{}: {e}", h.name)),
        }
    }
    if data.is_null() {
        data = json!("kept: no host could remove it");
    }
    tracing::info!(pool = %pool.name, ?done, ?failed, %data, "pool cleaned up");
    record(
        &state,
        "storage_pool.cleanup",
        ("storage_pool", pool.id.0),
        json!({"name": pool.name, "hosts": done, "failed": failed, "data": data}),
    )
    .await;
}

/// After an image was deleted: every host drops its local copy, and one
/// drops the copies in shared pools.
pub async fn image_deleted(state: AppState, image: hm_db::images::Image) {
    let shared: Vec<hm_proto::Pool> = hm_db::storage_pools::list(&state.db)
        .await
        .unwrap_or_default()
        .iter()
        .filter(|p| p.kind != "local")
        .filter_map(|p| crate::api::agent_pool(p).ok())
        .collect();
    let mut done = vec![];
    let mut failed = vec![];
    let mut shared_done = shared.is_empty();
    for h in live_hosts(&state).await {
        let pools = if shared_done { vec![] } else { shared.clone() };
        let cmd = Cmd::DeleteImage(hm_proto::DeleteImage {
            image_id: image.id.to_string(),
            pools: pools.clone(),
        });
        match state.agents.call(h.id, cmd, Duration::from_secs(300)).await {
            Ok(()) => {
                done.push(h.name.clone());
                shared_done = true;
            }
            Err(e) => failed.push(format!("{}: {e}", h.name)),
        }
    }
    record(
        &state,
        "image.cleanup",
        ("image", image.id.0),
        json!({"name": image.name, "hosts": done, "failed": failed, "shared_pools_cleaned": shared_done}),
    )
    .await;
}

/// Deletes a chassis from the southbound database, through the controller's
/// own connection to it (`--ovn-sb-admin`).
pub async fn delete_chassis(sb: &hm_ovsdb::Northbound, name: &str) -> anyhow::Result<()> {
    sb.transact(vec![
        json!({"op": "delete", "table": "Chassis_Private", "where": [["name", "==", name]]}),
        json!({"op": "delete", "table": "Chassis", "where": [["name", "==", name]]}),
        json!({"op": "delete", "table": "Encap", "where": [["chassis_name", "==", name]]}),
    ])
    .await?;
    Ok(())
}

/// Whether the southbound database still has the chassis.
pub async fn has_chassis(sb: &hm_ovsdb::Northbound, name: &str) -> anyhow::Result<bool> {
    let r = sb
        .transact(vec![json!({"op": "select", "table": "Chassis",
            "where": [["name", "==", name]], "columns": ["name"]})])
        .await?;
    Ok(r[0]["rows"].as_array().is_some_and(|r| !r.is_empty()))
}

#[cfg(test)]
mod tests {
    use hm_core::ids::StoragePoolId;
    use serde_json::json;

    fn pool(kind: &str, config: serde_json::Value) -> hm_db::storage_pools::Pool {
        hm_db::storage_pools::Pool {
            id: StoragePoolId::new(),
            name: "p".into(),
            kind: kind.into(),
            config,
            clusters: vec![],
            created_at: chrono::Utc::now(),
        }
    }

    /// Pools on the same storage share their data: deleting one must not
    /// remove what the other uses.
    #[test]
    fn pools_on_the_same_storage_are_recognised() {
        let b = |kind, c| super::backend(&pool(kind, c));
        assert_eq!(
            b("nfs", json!({"server": "10.0.0.1", "export": "/srv/vm/"})),
            b(
                "nfs",
                json!({"server": "10.0.0.1", "export": "/srv/vm", "options": "vers=4.1"})
            )
        );
        assert_ne!(
            b("nfs", json!({"server": "10.0.0.1", "export": "/srv/vm"})),
            b("nfs", json!({"server": "10.0.0.2", "export": "/srv/vm"}))
        );
        assert_eq!(
            b(
                "rbd",
                json!({"monitors": ["m2", "m1"], "pool": "vms", "user": "a", "key": "x"})
            ),
            b(
                "rbd",
                json!({"monitors": ["m1", "m2"], "pool": "vms", "user": "b", "key": "y"})
            )
        );
        assert_ne!(
            b("rbd", json!({"monitors": ["m1"], "pool": "vms"})),
            b("rbd", json!({"monitors": ["m1"], "pool": "other"}))
        );
        assert_eq!(b("local", json!({})), None);
    }
}
