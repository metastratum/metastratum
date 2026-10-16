//! Host failure: detection, fencing, and moving the host's VMs.
//!
//! One replica, the leader, watches the hosts of clusters with HA enabled.
//! A host is considered failed when:
//!
//! 1. its agent has been silent for [`GRACE`] (no session, no heartbeat),
//! 2. it does not answer on its libvirt TLS or SSH ports either (a crashed
//!    agent on a healthy host must not get the host shot), and
//! 3. at most half of its cluster looks failed (more is likelier a problem
//!    on the controller's side of the network than in the hosts).
//!
//! The leader then fences the host (see [`crate::fence`]). Only once the
//! fencing channel confirms it is off does a `host.recover` task restart its
//! VMs elsewhere. Without fencing configured nothing is restarted: running a
//! VM twice is worse than not running it.
//!
//! After a controller restart nothing is judged until the leader has been up
//! for twice the grace period, so a fleet that merely could not report
//! meanwhile is not taken for dead.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Utc;
use hm_core::ids::{ClusterId, HostId};
use hm_db::sqlx::{Connection, PgPool};

use crate::state::AppState;

/// Silence before a host is suspected.
pub const GRACE: Duration = Duration::from_secs(60);
const CHECK_EVERY: Duration = Duration::from_secs(10);
/// Advisory lock key for the leader ("hm-lead").
const LEADER: i64 = 0x0068_6d2d_6c65_6164;

/// Runs `work` on whichever replica holds the leader lock, for as long as
/// it holds it. The lock is tied to a database session: if this replica or
/// its connection dies, another takes over.
pub async fn lead<F, Fut>(db: PgPool, work: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    loop {
        match db.acquire().await {
            Ok(conn) => {
                // Held outside the pool, so the lock goes with the session.
                let mut conn = conn.detach();
                let got: Result<bool, _> =
                    hm_db::sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
                        .bind(LEADER)
                        .fetch_one(&mut conn)
                        .await;
                if got.unwrap_or(false) {
                    tracing::info!("leading");
                    metrics::gauge!("hm_leader").set(1.0);
                    let keepalive = async {
                        loop {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            if conn.ping().await.is_err() {
                                return;
                            }
                        }
                    };
                    tokio::select! {
                        () = work() => {}
                        () = keepalive => tracing::warn!("lost the database session; no longer leading"),
                    }
                    metrics::gauge!("hm_leader").set(0.0);
                }
            }
            Err(e) => tracing::warn!(error = %e, "leader election"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Whether a host answers on any of its management ports.
async fn answers(address: &str) -> bool {
    for port in [16514u16, 22] {
        let probe = tokio::net::TcpStream::connect((address, port));
        if matches!(
            tokio::time::timeout(Duration::from_secs(3), probe).await,
            Ok(Ok(_))
        ) {
            return true;
        }
    }
    false
}

/// The leader's watch over hosts.
pub async fn monitor(state: AppState) {
    let started = Instant::now();
    loop {
        tokio::time::sleep(CHECK_EVERY).await;
        if started.elapsed() < 2 * GRACE {
            continue;
        }
        if let Err(e) = check(&state).await {
            tracing::warn!(error = %format!("{e:#}"), "checking hosts");
        }
    }
}

async fn check(state: &AppState) -> Result<()> {
    let db = &state.db;
    let clusters: HashMap<ClusterId, bool> = hm_db::clusters::list(db)
        .await?
        .into_iter()
        .map(|c| (c.id, c.ha_enabled))
        .collect();
    let hosts = hm_db::hosts::list(db).await?;
    let silent = |h: &hm_db::hosts::Host| {
        !h.is_live()
            && h.last_seen_at
                .is_none_or(|t| (Utc::now() - t).to_std().unwrap_or_default() > GRACE)
    };
    for h in hosts
        .iter()
        .filter(|h| matches!(h.state.as_str(), "ready" | "maintenance"))
    {
        let Some(cluster) = h.cluster_id.filter(|c| clusters.get(c) == Some(&true)) else {
            continue;
        };
        if !silent(h) || answers(&h.address).await {
            continue;
        }
        let peers: Vec<_> = hosts
            .iter()
            .filter(|o| o.cluster_id == Some(cluster) && o.state != "decommissioned")
            .collect();
        let down = peers.iter().filter(|o| silent(o)).count();
        if down * 2 > peers.len() {
            tracing::error!(cluster = %cluster, down, of = peers.len(), "most of the cluster looks down; not fencing (check the controller's network)");
            continue;
        }
        fence_and_recover(state, h.id, &h.name).await;
    }
    Ok(())
}

async fn fence_and_recover(state: &AppState, host: HostId, name: &str) {
    let db = &state.db;
    let Ok(Some((kind, config))) = hm_db::hosts::fencing(db, host).await else {
        tracing::error!(%host, %name, "host has failed, but has no fencing configured: its VMs are not restarted elsewhere");
        return;
    };
    let config = match open_config(host, &config) {
        Ok(c) => c,
        Err(e) => return tracing::error!(%host, error = %format!("{e:#}"), "fencing configuration"),
    };
    tracing::warn!(%host, %name, %kind, "host has failed; fencing it");
    if let Err(e) = crate::fence::power_off(&kind, &config).await {
        // Not confirmed off: leave its VMs alone; try again next round.
        tracing::error!(%host, %name, error = %format!("{e:#}"), "fencing failed; the host's VMs stay put");
        metrics::counter!("hm_fencings_total", "outcome" => "failed").increment(1);
        crate::authz::audit(
            db,
            None,
            None,
            "host.fence",
            Some(("host", host.0)),
            serde_json::json!({"name": name, "error": format!("{e:#}")}),
        )
        .await;
        return;
    }
    tracing::warn!(%host, %name, "host fenced (confirmed off)");
    metrics::counter!("hm_fencings_total", "outcome" => "fenced").increment(1);
    let task = async {
        let mut tx = db.begin().await?;
        hm_db::hosts::set_state(&mut *tx, host, "fenced").await?;
        let root = hm_db::tenants::root(&mut *tx)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no root tenant"))?;
        hm_db::tasks::enqueue(
            &mut *tx,
            hm_db::tasks::NewTask {
                tenant_id: root.id,
                kind: "host.recover",
                target: Some(("host", host.0)),
                params: serde_json::json!({}),
                created_by: None,
            },
        )
        .await?;
        tx.commit().await?;
        anyhow::Ok(())
    };
    if let Err(e) = task.await {
        tracing::error!(%host, error = %format!("{e:#}"), "queueing recovery");
    }
    crate::authz::audit(
        db,
        None,
        None,
        "host.fence",
        Some(("host", host.0)),
        serde_json::json!({"name": name}),
    )
    .await;
}

/// A host's fencing configuration with its secrets opened.
pub fn open_config(host: HostId, config: &serde_json::Value) -> Result<serde_json::Value> {
    let mut c = config.clone();
    for f in crate::fence::SECRET_FIELDS {
        if let Some(v) = c.get(*f).and_then(|v| v.as_str()) {
            c[*f] = crate::secrets::open(&crate::fence::secret_context(host), v)?.into();
        }
    }
    Ok(c)
}
