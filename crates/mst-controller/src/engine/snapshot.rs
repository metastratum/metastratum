//! Snapshot tasks: taking, reverting to and deleting snapshots on the VM's
//! host. Each is idempotent on the agent, so a resumed task repeats it.

use std::time::Duration;

use anyhow::{Context, Result};
use mst_core::ids::{HostId, SnapshotId};
use mst_proto::command::Cmd;
use mst_proto::{BackupDisk, DeleteSnapshot, RevertSnapshot, SnapshotVm};

use super::TaskCtx;
use super::backup::send;

/// Snapshots are quick, but a guest's file systems may take a while to
/// freeze, and a revert of a large Ceph volume rewrites it.
const TIMEOUT: Duration = Duration::from_secs(3600);

fn target(ctx: &TaskCtx) -> Result<SnapshotId> {
    ctx.task
        .target_id
        .map(SnapshotId)
        .context("task has no target snapshot")
}

/// The snapshot, its VM's host and disks.
async fn load(ctx: &TaskCtx) -> Result<(mst_db::snapshots::Snapshot, HostId, Vec<BackupDisk>)> {
    let db = &ctx.state.db;
    let snap = mst_db::snapshots::get(db, target(ctx)?).await?;
    let vm = mst_db::vms::get(db, snap.vm_id).await?;
    let host = vm.host_id.context("the VM is not placed on a host")?;
    let mut disks = vec![];
    let slots = vm.spec.0.disk_slots();
    for (i, (d, slot)) in vm.spec.0.disks.iter().zip(slots).enumerate() {
        let pool = mst_db::storage_pools::get(db, d.pool_id).await?;
        disks.push(BackupDisk {
            index: i as u32,
            volume_id: d.volume_id.to_string(),
            pool: Some(crate::api::agent_pool(&pool)?),
            target: slot.target,
        });
    }
    Ok((snap, host, disks))
}

#[derive(serde::Deserialize)]
struct Taken {
    consistent: Option<bool>,
}

pub async fn create(ctx: &TaskCtx) -> Result<()> {
    let db = &ctx.state.db;
    let id = target(ctx)?;
    let result = async {
        let (snap, host, disks) = load(ctx).await?;
        ctx.step("taking the snapshot", 0.2).await;
        let out = send(
            &ctx.state,
            host,
            Cmd::SnapshotVm(SnapshotVm {
                vm_id: snap.vm_id.to_string(),
                snapshot_id: id.to_string(),
                disks,
            }),
            TIMEOUT,
        )
        .await?;
        let taken: Taken = serde_json::from_str(&out).context("the agent's answer")?;
        anyhow::Ok(taken)
    }
    .await;
    match &result {
        Ok(t) => mst_db::snapshots::set_state(db, id, "ready", t.consistent, None).await?,
        Err(e) => {
            mst_db::snapshots::set_state(db, id, "failed", None, Some(&format!("{e:#}"))).await?
        }
    }
    result.map(|_| ())
}

pub async fn revert(ctx: &TaskCtx) -> Result<()> {
    let db = &ctx.state.db;
    let id = target(ctx)?;
    let result = async {
        let (snap, host, disks) = load(ctx).await?;
        ctx.step("reverting the disks", 0.2).await;
        send(
            &ctx.state,
            host,
            Cmd::RevertSnapshot(RevertSnapshot {
                vm_id: snap.vm_id.to_string(),
                snapshot_id: id.to_string(),
                disks,
            }),
            TIMEOUT,
        )
        .await
    }
    .await;
    // The snapshot stays either way; a failure is shown with it.
    let error = result.as_ref().err().map(|e| format!("{e:#}"));
    mst_db::snapshots::set_state(db, id, "ready", None, error.as_deref()).await?;
    result.map(|_| ())
}

pub async fn delete(ctx: &TaskCtx) -> Result<()> {
    let db = &ctx.state.db;
    let id = target(ctx)?;
    let result = async {
        let (snap, host, disks) = match load(ctx).await {
            Ok(l) => l,
            // Gone with its VM (a retried task).
            Err(e)
                if e.downcast_ref::<mst_db::Error>().is_some_and(|e| {
                    matches!(e, mst_db::Error::Domain(mst_core::Error::NotFound { .. }))
                }) =>
            {
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        ctx.step("deleting the snapshot", 0.2).await;
        send(
            &ctx.state,
            host,
            Cmd::DeleteSnapshot(DeleteSnapshot {
                vm_id: snap.vm_id.to_string(),
                snapshot_id: id.to_string(),
                disks,
            }),
            TIMEOUT,
        )
        .await?;
        mst_db::snapshots::delete(db, id).await?;
        Ok(())
    }
    .await;
    if let Err(e) = &result {
        // Still there: back to how it was, with why it stayed. A snapshot
        // that failed to be taken stays failed.
        let state = match mst_db::snapshots::get(db, id).await {
            Ok(s) if s.consistent.is_some() => "ready",
            _ => "failed",
        };
        let _ = mst_db::snapshots::set_state(db, id, state, None, Some(&format!("{e:#}"))).await;
    }
    result
}
