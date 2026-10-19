//! Backup tasks, and the schedule that starts them.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use mst_core::ids::{BackupId, HostId};
use mst_proto::command::Cmd;
use mst_proto::{BackupDisk, BackupVm, DeleteBackup};

use super::TaskCtx;
use crate::state::AppState;

/// A large disk takes long to copy; the agent reports as soon as it is done.
const BACKUP_TIMEOUT: Duration = Duration::from_secs(24 * 3600);
/// How often the schedule looks for VMs due a backup.
const SCHEDULE_EVERY: Duration = Duration::from_secs(60);

fn target(ctx: &TaskCtx) -> Result<BackupId> {
    ctx.task
        .target_id
        .map(BackupId)
        .context("task has no target backup")
}

#[derive(serde::Deserialize)]
struct Taken {
    stored_bytes: u64,
    #[serde(default)]
    consistent: bool,
    /// Some disk is stored as the changes since the parent.
    #[serde(default)]
    incremental: bool,
    #[serde(default)]
    full_reason: Option<String>,
}

/// Takes a backup on the VM's host.
pub async fn create(ctx: &TaskCtx) -> Result<()> {
    let db = &ctx.state.db;
    let id = target(ctx)?;
    let backup = mst_db::backups::get(db, id).await?;
    // Built on another, it holds the chain while it is taken: what it
    // builds on is not merged away meanwhile.
    let lock = match backup.parent_id {
        Some(_) => Some(mst_db::backups::ChainLock::take(db, backup.chain_id).await?),
        None => None,
    };
    let result = take(ctx, &backup).await;
    match &result {
        Ok(t) => {
            let reason = match t.incremental {
                false => t.full_reason.as_deref(),
                true => None,
            };
            let mut conn = db.acquire().await?;
            mst_db::backups::finish(&mut conn, id, t.stored_bytes as i64, t.consistent, reason)
                .await?
        }
        Err(e) => mst_db::backups::fail(db, id, &format!("{e:#}")).await?,
    }
    if let Some(lock) = lock {
        lock.release().await;
    }
    let taken = result?;
    tracing::info!(backup = %id, vm = %backup.vm_name, stored = taken.stored_bytes, consistent = taken.consistent, incremental = taken.incremental, "backup taken");
    if backup.scheduled
        && let Some(vm) = backup.vm_id
    {
        prune(&ctx.state, vm).await;
    }
    Ok(())
}

async fn take(ctx: &TaskCtx, backup: &mst_db::backups::Backup) -> Result<Taken> {
    let db = &ctx.state.db;
    let vm_id = backup.vm_id.context("the VM is gone")?;
    let vm = mst_db::vms::get(db, vm_id).await?;
    let host = vm.host_id.context("the VM is not placed on a host")?;
    let repo = mst_db::backups::get_repository(db, backup.repository_id).await?;
    // The disks recorded with the backup, in that order: a restore puts
    // them back so.
    let recorded: Vec<_> = backup.spec.0.disks.iter().map(|d| d.volume_id).collect();
    let now: Vec<_> = vm.spec.0.disks.iter().map(|d| d.volume_id).collect();
    if recorded != now {
        anyhow::bail!("the VM's disks changed since the backup was asked for: take it again");
    }
    // What it may build on: each disk the parent has too, and checkpoints
    // that have tracked the VM's changes since the parent was taken.
    let (parent, checkpoints) = match backup.parent_id {
        Some(p) => {
            let parent = mst_db::backups::get(db, p).await?;
            let checkpoints = mst_db::backups::taken_before(db, vm.id, &parent).await?;
            (Some(parent), checkpoints)
        }
        None => (None, vec![]),
    };
    let mut disks = vec![];
    let slots = vm.spec.0.disk_slots();
    for (i, (d, slot)) in vm.spec.0.disks.iter().zip(slots).enumerate() {
        let pool = mst_db::storage_pools::get(db, d.pool_id).await?;
        disks.push(BackupDisk {
            index: i as u32,
            volume_id: d.volume_id.to_string(),
            pool: Some(crate::api::agent_pool(&pool)?),
            target: slot.target,
            parent_index: parent.as_ref().and_then(|p| {
                p.spec
                    .0
                    .disks
                    .iter()
                    .position(|pd| pd.volume_id == d.volume_id)
                    .map(|j| j as u32)
            }),
        });
    }
    ctx.step(
        if parent.is_some() {
            "copying what changed"
        } else {
            "copying disks"
        },
        0.1,
    )
    .await;
    let out = send(
        &ctx.state,
        host,
        Cmd::BackupVm(BackupVm {
            vm_id: vm.id.to_string(),
            backup_id: backup.id.to_string(),
            repository: Some(crate::api::repository_pool(&repo)),
            disks,
            parent_id: parent.map(|p| p.id.to_string()).unwrap_or_default(),
            checkpoints: checkpoints.iter().map(ToString::to_string).collect(),
        }),
        BACKUP_TIMEOUT,
    )
    .await?;
    serde_json::from_str(&out).context("the agent's answer")
}

/// Sends a command, waiting out a moment without the host's agent (a
/// reconnect); commands sent here are idempotent.
pub(super) async fn send(
    state: &AppState,
    host: HostId,
    cmd: Cmd,
    timeout: Duration,
) -> Result<String> {
    use crate::agents::AgentError;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        match state.agents.send(host, cmd.clone(), timeout).await {
            Err(AgentError::Closed | AgentError::NotConnected)
                if tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            r => return r.map_err(|e| anyhow!(e)),
        }
    }
}

/// Deletes a backup: merged into the backups built on it, which keep
/// reading as they did, its files are removed, then its record.
pub async fn delete(ctx: &TaskCtx) -> Result<()> {
    let state = &ctx.state;
    let id = target(ctx)?;
    let backup = match mst_db::backups::get(&state.db, id).await {
        Ok(b) => b,
        // Already gone (a retried task).
        Err(mst_db::Error::Domain(mst_core::Error::NotFound { .. })) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    // One change to a chain at a time: a neighbour's merge changes the
    // files this one's reads.
    let lock = mst_db::backups::ChainLock::take(&state.db, backup.chain_id).await?;
    let result = delete_locked(ctx, id).await;
    lock.release().await;
    if let Err(e) = &result {
        mst_db::backups::delete_failed(&state.db, id, &format!("{e:#}")).await?;
    }
    result
}

async fn delete_locked(ctx: &TaskCtx, id: BackupId) -> Result<()> {
    let state = &ctx.state;
    // As it is now: earlier merges may have moved it down its chain, or
    // another deletion (waited for) removed it.
    let backup = match mst_db::backups::get(&state.db, id).await {
        Ok(b) => b,
        Err(mst_db::Error::Domain(mst_core::Error::NotFound { .. })) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let children = mst_db::backups::children(&state.db, id).await?;
    if children.iter().any(|c| c.state == "running") {
        bail!("a backup being taken builds on it");
    }
    let repo = mst_db::backups::get_repository(&state.db, backup.repository_id).await?;
    // Merging takes an agent that knows how; removing files, any.
    let host = any_host(state, backup.vm_id).await?;
    if !children.is_empty() {
        ctx.step("merging into the backup built on it", 0.1).await;
    }
    let out = send(
        state,
        host,
        Cmd::DeleteBackup(DeleteBackup {
            repository: Some(crate::api::repository_pool(&repo)),
            backup_id: id.to_string(),
            children: children.iter().map(|c| c.id.to_string()).collect(),
        }),
        BACKUP_TIMEOUT,
    )
    .await
    .context("removing the backup's files")?;
    #[derive(serde::Deserialize, Default)]
    struct Merged {
        #[serde(default)]
        children: std::collections::HashMap<String, i64>,
    }
    let merged: Merged = serde_json::from_str(&out).unwrap_or_default();
    let sizes: Vec<(BackupId, i64)> = children
        .iter()
        .map(|c| {
            let stored = merged
                .children
                .get(&c.id.to_string())
                .copied()
                .or(c.stored_bytes)
                .unwrap_or(0);
            (c.id, stored)
        })
        .collect();
    let mut tx = state.db.begin().await?;
    mst_db::backups::merged(&mut tx, &backup, &sizes).await?;
    tx.commit().await?;
    Ok(())
}

/// A host that can reach the repository: the VM's, or any connected one.
async fn any_host(state: &AppState, vm: Option<mst_core::ids::VmId>) -> Result<HostId> {
    let able = |h: &mst_db::hosts::Host| h.state != "decommissioned" && h.connected_to.is_some();
    if let Some(vm) = vm
        && let Ok(vm) = mst_db::vms::get(&state.db, vm).await
        && let Some(host) = vm.host_id
        && mst_db::hosts::get(&state.db, host)
            .await
            .is_ok_and(|h| able(&h))
    {
        return Ok(host);
    }
    mst_db::hosts::list(&state.db)
        .await?
        .into_iter()
        .find(able)
        .map(|h| h.id)
        .ok_or_else(|| anyhow!("no host is connected to reach the repository"))
}

/// Deletes a VM's scheduled backups beyond what its schedule keeps.
async fn prune(state: &AppState, vm: mst_core::ids::VmId) {
    let Ok(Some(schedule)) = mst_db::backups::schedule_of(&state.db, vm).await else {
        return;
    };
    let Ok(old) = mst_db::backups::beyond_keep(&state.db, vm, schedule.keep).await else {
        return;
    };
    for id in old {
        let queued = async {
            let b = mst_db::backups::get(&state.db, id).await?;
            crate::api::queue_backup_delete(state, &b, None)
                .await
                .map_err(|e| anyhow!(e.message))
        };
        if let Err(e) = queued.await {
            tracing::warn!(backup = %id, error = %format!("{e:#}"), "deleting an old scheduled backup");
        }
    }
}

/// Starts the backups schedules want. Runs on the leader.
pub async fn schedule(state: AppState) {
    loop {
        tokio::time::sleep(SCHEDULE_EVERY).await;
        if let Err(e) = schedule_once(&state).await {
            tracing::warn!(error = %format!("{e:#}"), "backup schedule");
        }
    }
}

async fn schedule_once(state: &AppState) -> Result<()> {
    for due in mst_db::backups::due(&state.db).await? {
        let vm = mst_db::vms::get(&state.db, due.vm_id).await?;
        let repo = mst_db::backups::get_repository(&state.db, due.repository_id).await?;
        let mode = if due.full_due() {
            crate::api::BackupMode::Full
        } else {
            crate::api::BackupMode::Incremental
        };
        match crate::api::start_backup(state, None, &vm, &repo, true, mode).await {
            Ok((_, b)) => {
                tracing::info!(vm = %vm.name, backup = %b.id, kind = %b.kind, "scheduled backup started")
            }
            // A busy VM is backed up on a later round; anything else is
            // worth saying.
            Err(e) if e.status == axum::http::StatusCode::CONFLICT => {
                tracing::debug!(vm = %vm.name, reason = %e.message, "scheduled backup postponed")
            }
            Err(e) => bail!("backing up {}: {}", vm.name, e.message),
        }
    }
    Ok(())
}
