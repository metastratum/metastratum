//! Cleaning up: after deleted pools and images, and when the host is
//! decommissioned.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use mst_storage::PoolSpec;

use crate::session::Agent;

/// Libvirt secrets metastratum defines for Ceph pools are named so.
const SECRET_PREFIX: &str = "metastratum pool ";

/// Lets go of a deleted pool here, and removes its data if asked to.
pub async fn remove_pool(agent: &Agent, spec: PoolSpec, remove_data: bool) -> Result<String> {
    let mut removed = 0;
    if remove_data {
        let pool = agent.pools.get(&spec).await.context("reaching the pool")?;
        removed = pool.purge().await.context("removing metastratum's data")?;
        tracing::info!(pool = %spec.id, removed, "removed the pool's data");
    }
    agent.pools.forget(&spec.id).await?;
    agent.lv.remove_secret(&spec.id).await?;
    tracing::info!(pool = %spec.id, "let go of the pool");
    Ok(serde_json::json!({ "removed": removed }).to_string())
}

/// Downloads and copies into image stores under way here, by image:
/// deleting an image cuts its own off, so a slow download of an image
/// deleted meanwhile does not keep going (for up to hours).
#[derive(Default)]
pub struct ImageWork {
    next: std::sync::atomic::AtomicU64,
    running: std::sync::Mutex<Vec<(String, u64, futures::future::AbortHandle)>>,
}

impl ImageWork {
    /// Runs `work` on image `id`, until it is done or the image deleted.
    pub async fn run<T>(&self, id: &str, work: impl Future<Output = Result<T>>) -> Result<T> {
        let (handle, registration) = futures::future::AbortHandle::new_pair();
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.running
            .lock()
            .unwrap()
            .push((id.to_owned(), n, handle));
        let out = futures::future::Abortable::new(work, registration).await;
        self.running.lock().unwrap().retain(|(_, m, _)| *m != n);
        out.unwrap_or_else(|_| bail!("image {id} was deleted meanwhile"))
    }

    /// Cuts off the work on image `id` under way (what it leaves goes).
    pub fn cut_off(&self, id: &str) {
        for (_, _, handle) in self.running.lock().unwrap().iter().filter(|w| w.0 == id) {
            handle.abort();
        }
    }
}

/// Removes a deleted image's copies: the one in this host's image store,
/// and those in the given shared pools' stores, cutting off downloads and
/// copies of it under way. Every pool is tried; the error names those that
/// failed.
pub async fn delete_image(
    agent: &Agent,
    id: &str,
    shared: &[PoolSpec],
    tenant_path: &str,
) -> Result<()> {
    agent.image_work.cut_off(id);
    let mut failed = vec![];
    if let Err(e) = agent.pools.store().remove(tenant_path, id).await {
        failed.push(format!("this host's store: {e:#}"));
    }
    for spec in shared {
        let removed = async {
            let pool = agent.pools.get(spec).await.context("reaching it")?;
            pool.remove_stored(tenant_path, id).await?;
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = removed {
            failed.push(format!("pool {}: {e:#}", spec.id));
        }
    }
    if !failed.is_empty() {
        bail!("removing image {id}: {}", failed.join("; "));
    }
    tracing::info!(
        image = id,
        pools = shared.len(),
        "removed the image's copies"
    );
    Ok(())
}

/// Removes from a store the images that no longer exist (stored an hour
/// ago or more, and before `keep` was listed, if that is known: newer ones
/// may be missing from it) and what downloads and copies cut off a day ago
/// or more left.
pub async fn prune_store(
    pool: &mst_storage::Pool,
    keep: &HashSet<&str>,
    listed_at: Option<SystemTime>,
) -> Result<Vec<String>> {
    let mut before = SystemTime::now() - Duration::from_secs(3600);
    if let Some(at) = listed_at {
        // Allowing for the controller's clock being off from this host's.
        before = before.min(at - Duration::from_secs(600));
    }
    let yesterday = SystemTime::now() - Duration::from_secs(86400);
    let removed = pool.prune_stored(keep, before, yesterday).await?;
    for id in &removed {
        tracing::info!(image = %id, "removed a deleted image from the store");
    }
    Ok(removed)
}

/// Lets go of what was deleted while this agent was away: pools it still
/// holds on to, their secrets, and copies in its store of images that no
/// longer exist (see [`prune_store`]).
pub async fn reconcile(agent: &Agent, inventory: &mst_proto::Inventory) {
    let pools: HashSet<&str> = inventory.pool_ids.iter().map(String::as_str).collect();
    let images: HashSet<&str> = inventory.image_ids.iter().map(String::as_str).collect();
    for id in agent.pools.attached().await.unwrap_or_default() {
        if !pools.contains(id.as_str()) {
            match agent.pools.forget(&id).await {
                Ok(()) => tracing::info!(pool = %id, "let go of a pool deleted meanwhile"),
                Err(e) => {
                    tracing::warn!(pool = %id, error = %format!("{e:#}"), "letting go of a deleted pool")
                }
            }
        }
    }
    for id in agent
        .lv
        .ceph_secrets(SECRET_PREFIX)
        .await
        .unwrap_or_default()
    {
        if !pools.contains(id.as_str()) {
            let _ = agent.lv.remove_secret(&id).await;
        }
    }
    let local = mst_storage::Pool::File(agent.pools.local());
    // Listed just now, as the agent connected.
    match prune_store(&local, &images, None).await {
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "pruning the image store"),
    }
}

/// What decommissioning did, step by step.
#[derive(Default, serde::Serialize)]
pub struct Report {
    pub done: Vec<String>,
    pub failed: Vec<String>,
}

impl Report {
    fn step(&mut self, what: &str, r: Result<String>) {
        match r {
            Ok(detail) if detail.is_empty() => self.done.push(what.to_owned()),
            Ok(detail) => self.done.push(format!("{what}: {detail}")),
            Err(e) => self.failed.push(format!("{what}: {e:#}")),
        }
    }
}

async fn run(program: &str, args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Removes metastratum from this host. Refuses while VMs are defined here.
/// The agent's own service is stopped last, after the controller has the
/// report ([`stop_self`]).
pub async fn decommission(agent: &Agent) -> Result<Report> {
    let domains = agent.lv.list_managed().await?;
    if !domains.is_empty() {
        bail!("{} VMs are still defined on this host", domains.len());
    }
    let mut report = Report::default();

    // OVN is the admin's, and stays: only what metastratum changed goes back.
    report.step(
        "restored OVN's southbound settings",
        crate::ovn::restore().await,
    );

    // Storage: every pool let go of, and metastratum's local files.
    for id in agent.pools.attached().await.unwrap_or_default() {
        report.step(
            &format!("let go of pool {id}"),
            agent
                .pools
                .forget(&id)
                .await
                .map(|()| String::new())
                .map_err(Into::into),
        );
    }
    match agent.lv.ceph_secrets(SECRET_PREFIX).await {
        Ok(secrets) => {
            for id in secrets {
                report.step(
                    &format!("removed libvirt secret {id}"),
                    agent
                        .lv
                        .remove_secret(&id)
                        .await
                        .map(|()| String::new())
                        .map_err(Into::into),
                );
            }
        }
        Err(e) => report.step("listing libvirt secrets", Err(e.into())),
    }
    report.step(
        "removed the image store",
        agent
            .pools
            .store()
            .purge()
            .await
            .map(|n| format!("{n} files"))
            .map_err(Into::into),
    );
    report.step(
        "removed cached images and local volumes",
        agent
            .pools
            .local()
            .purge()
            .await
            .map(|n| format!("{n} files"))
            .map_err(Into::into),
    );
    report.step(
        "removed cloud-init seeds",
        remove_files_in(&agent.files.cloud_init_dir, ".iso").await,
    );

    // QEMU: no metastratum certificates.
    report.step("removed metastratum's TLS files", {
        let _held = agent.qemu_tls.lock().await;
        crate::tls::remove_installed().map(|n| format!("{n} files"))
    });
    // The agent's identity: it could not connect again anyway (revoked).
    report.step(
        "removed the agent's identity",
        remove_files_in(&agent.state_dir, "").await,
    );
    Ok(report)
}

/// Removes the files (not directories) in `dir` whose names end in `suffix`.
async fn remove_files_in(dir: &std::path::Path, suffix: &str) -> Result<String> {
    let mut n = 0;
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok("none".into()),
        Err(e) => return Err(e.into()),
    };
    while let Some(e) = entries.next_entry().await? {
        if e.file_type().await?.is_file() && e.file_name().to_string_lossy().ends_with(suffix) {
            tokio::fs::remove_file(e.path()).await?;
            n += 1;
        }
    }
    Ok(format!("{n} files"))
}

/// Stops and disables the agent's service, once its answer has gone out.
pub fn stop_self() {
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(3)).await;
        tracing::warn!("decommissioned: stopping and disabling mst-agent");
        let _ = run("systemctl", &["disable", "--now", "mst-agent.service"]).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deleting_an_image_cuts_its_work_off() {
        let work = std::sync::Arc::new(ImageWork::default());
        let w = work.clone();
        let slow = tokio::spawn(async move {
            w.run("a", async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(())
            })
            .await
        });
        let other = work.run("b", async { Ok(7) });
        tokio::time::sleep(Duration::from_millis(50)).await;
        work.cut_off("a");
        let err = slow.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("deleted meanwhile"), "{err}");
        // Other images' work goes on.
        assert_eq!(other.await.unwrap(), 7);
        assert!(work.running.lock().unwrap().is_empty());
    }
}
