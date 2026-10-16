//! Cleaning up: after deleted pools and images, and when the host is
//! decommissioned.

use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use hm_storage::PoolSpec;

use crate::session::Agent;

/// Libvirt secrets hypermon defines for Ceph pools are named so.
const SECRET_PREFIX: &str = "hypermon pool ";

/// Lets go of a deleted pool here, and removes its data if asked to.
pub async fn remove_pool(agent: &Agent, spec: PoolSpec, remove_data: bool) -> Result<String> {
    let mut removed = 0;
    if remove_data {
        let pool = agent.pools.get(&spec).await.context("reaching the pool")?;
        removed = pool.purge().await.context("removing hypermon's data")?;
        tracing::info!(pool = %spec.id, removed, "removed the pool's data");
    }
    agent.pools.forget(&spec.id).await?;
    agent.lv.remove_secret(&spec.id).await?;
    tracing::info!(pool = %spec.id, "let go of the pool");
    Ok(serde_json::json!({ "removed": removed }).to_string())
}

/// Removes a deleted image's cached copies: the local one, and those in the
/// given shared pools.
pub async fn delete_image(agent: &Agent, id: &str, shared: &[PoolSpec]) -> Result<()> {
    agent.pools.local().delete_image(id).await?;
    for spec in shared {
        agent
            .pools
            .get(spec)
            .await
            .with_context(|| format!("reaching pool {}", spec.id))?
            .delete_image(id)
            .await?;
    }
    tracing::info!(
        image = id,
        pools = shared.len(),
        "removed the image's cached copies"
    );
    Ok(())
}

/// Lets go of what was deleted while this agent was away: pools it still
/// holds on to, their secrets, and cached copies of images that no longer
/// exist (ones cached in the last hour stay: they may be newer than the
/// inventory).
pub async fn reconcile(agent: &Agent, inventory: &hm_proto::Inventory) {
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
    let recent = SystemTime::now() - Duration::from_secs(3600);
    let local = agent.pools.local();
    for (id, cached) in local.cached_images().await.unwrap_or_default() {
        if !images.contains(id.as_str()) && cached < recent {
            match local.delete_image(&id).await {
                Ok(()) => tracing::info!(image = %id, "removed an image deleted meanwhile"),
                Err(e) => {
                    tracing::warn!(image = %id, error = %format!("{e:#}"), "removing a deleted image")
                }
            }
        }
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

/// Removes hypermon from this host. Refuses while VMs are defined here.
/// The agent's own service is stopped last, after the controller has the
/// report ([`stop_self`]).
pub async fn decommission(agent: &Agent) -> Result<Report> {
    let domains = agent.lv.list_managed().await?;
    if !domains.is_empty() {
        bail!("{} VMs are still defined on this host", domains.len());
    }
    let mut report = Report::default();

    // OVN is the admin's, and stays: only what hypermon changed goes back.
    report.step(
        "restored OVN's southbound settings",
        crate::ovn::restore().await,
    );

    // Storage: every pool let go of, and hypermon's local files.
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

    // Libvirt: no TLS listener, no hypermon certificates.
    report.step(
        "closed libvirt's TLS listener",
        crate::tls::set_listening(false)
            .await
            .map(|()| String::new()),
    );
    report.step("removed hypermon's TLS files", {
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
        tracing::warn!("decommissioned: stopping and disabling hm-agent");
        let _ = run("systemctl", &["disable", "--now", "hm-agent.service"]).await;
    });
}
