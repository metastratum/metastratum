//! Restarting a fenced host's VMs on the rest of its cluster.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use mst_core::ids::HostId;

use super::TaskCtx;

/// Starts can fail for a while after a host dies: its locks on shared disks
/// (NFS file leases, Ceph exclusive locks) take time to expire.
const START_ATTEMPTS: u32 = 6;
const START_RETRY: Duration = Duration::from_secs(30);

pub async fn recover(ctx: &TaskCtx) -> Result<()> {
    let db = &ctx.state.db;
    let host = HostId(ctx.task.target_id.context("task has no target host")?);
    if mst_db::hosts::get(db, host).await?.state != "fenced" {
        bail!("the host is not fenced; its VMs may still be running there");
    }
    // Migrations to or from the dead host are over: the VM stays (or
    // stopped) on its source.
    let stuck: Vec<_> = mst_db::sqlx::query_scalar::<_, mst_core::ids::VmId>(
        "SELECT vm_id FROM migrations WHERE source_host_id = $1 OR dest_host_id = $1",
    )
    .bind(host)
    .fetch_all(db)
    .await?;
    for vm in stuck {
        let mut tx = db.begin().await?;
        mst_db::migrations::rolled_back(&mut tx, vm).await?;
        tx.commit().await?;
    }

    let mut moved = vec![];
    let mut stayed = vec![];
    let mut failed = vec![];
    for vm in mst_db::vms::list_on_host(db, host).await? {
        // The host is off, and so is everything on it.
        mst_db::sqlx::query(
            "UPDATE vms SET power = 'stopped', power_reason = 'fenced', power_observed_at = now() WHERE id = $1",
        )
        .bind(vm.id)
        .execute(db)
        .await?;
        if vm.status != "ready" || vm.desired_power != "running" {
            continue;
        }
        let mut shared = true;
        for d in &vm.spec.0.disks {
            if mst_db::storage_pools::get(db, d.pool_id).await?.kind == "local" {
                shared = false;
            }
        }
        if !shared {
            stayed.push(format!("{} (local disk)", vm.name));
            continue;
        }
        // A new home in the same cluster.
        let mut tx = db.begin().await?;
        let (dest, dest_name) = match another_host(&mut tx, &vm, host).await? {
            Ok(d) => d,
            Err(why) => {
                failed.push(format!("{}: {why}", vm.name));
                continue;
            }
        };
        mst_db::sqlx::query("UPDATE vms SET host_id = $2, updated_at = now() WHERE id = $1")
            .bind(vm.id)
            .bind(dest)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        ctx.step(&format!("restarting {} on {dest_name}", vm.name), 0.5)
            .await;
        let mut attempt = 1;
        loop {
            match super::vm::provision(ctx, vm.id, false).await {
                Ok(()) => {
                    tracing::warn!(vm = %vm.id, to = %dest_name, "restarted after host failure");
                    moved.push(format!("{} → {dest_name}", vm.name));
                    break;
                }
                Err(e) if attempt < START_ATTEMPTS => {
                    tracing::info!(vm = %vm.id, attempt, error = %format!("{e:#}"), "restart failed; retrying (locks of the dead host may still hold)");
                    attempt += 1;
                    tokio::time::sleep(START_RETRY).await;
                }
                Err(e) => {
                    failed.push(format!("{}: {e:#}", vm.name));
                    break;
                }
            }
        }
    }
    // The dead host's view of the network goes too.
    ctx.state.net.kick();
    let summary = format!(
        "restarted {}; stayed: {}",
        if moved.is_empty() {
            "none".into()
        } else {
            moved.join(", ")
        },
        if stayed.is_empty() {
            "none".into()
        } else {
            stayed.join(", ")
        }
    );
    ctx.step(&summary, 1.0).await;
    if !failed.is_empty() {
        bail!(
            "{} VM(s) could not be restarted: {}",
            failed.len(),
            failed.join("; ")
        );
    }
    Ok(())
}

/// Another host for a VM on shared storage to run on than `from`: in the
/// same cluster (its CPU model), with every feature of the VM's CPU, and
/// room for it, as the scheduler picks. Hosts are locked until `tx` ends
/// (see `clusters::candidates_locked`). Why none fits, if none does.
pub(crate) async fn another_host(
    tx: &mut mst_db::sqlx::PgConnection,
    vm: &mst_db::vms::Vm,
    from: mst_core::ids::HostId,
) -> anyhow::Result<std::result::Result<(mst_core::ids::HostId, String), String>> {
    let candidates = mst_db::clusters::candidates_locked(tx).await?;
    let cluster = candidates
        .iter()
        .find(|c| c.id == from)
        .and_then(|c| c.cluster_id);
    // Hosts whose CPU lacks what the VM's has do not run it.
    let full = mst_db::hosts::list(&mut *tx).await?;
    let needs = crate::cpu::needs(vm, &full);
    let runs = |id: mst_core::ids::HostId| match (&needs, full.iter().find(|h| h.id == id)) {
        (Some(n), Some(h)) => crate::cpu::lacking(h, n).is_none_or(|l| l.is_empty()),
        _ => true,
    };
    let hosts: Vec<mst_scheduler::Host> = candidates
        .iter()
        .filter(|c| c.id != from)
        .filter(|c| runs(c.id))
        .map(|c| mst_scheduler::Host {
            id: c.id,
            name: c.name.clone(),
            cluster: c.cluster_id,
            live: c.live,
            state: c.state.clone(),
            kvm: c.kvm,
            cpus: c.cpu_count.max(0) as u32,
            memory_mib: c.memory_mib.max(0) as u64,
            cpu_overcommit: f64::from(c.cpu_overcommit),
            memory_overcommit: f64::from(c.memory_overcommit),
            used_vcpus: c.used_vcpus.max(0) as u64,
            used_memory_mib: c.used_memory_mib.max(0) as u64,
        })
        .collect();
    let req = mst_scheduler::Request {
        vcpus: vm.spec.0.reserved_vcpus(),
        memory_mib: vm.spec.0.reserved_memory_mib(),
        clusters: cluster.map(|c| [c].into_iter().collect()),
        host: None,
    };
    Ok(match mst_scheduler::place(&req, &hosts) {
        Ok(dest) => {
            let name = hosts
                .iter()
                .find(|h| h.id == dest)
                .map(|h| h.name.clone())
                .unwrap_or_default();
            Ok((dest, name))
        }
        Err(e) => Err(e.to_string()),
    })
}
