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
const QUICK: Duration = Duration::from_secs(120);
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
}

/// Takes a backup on the VM's host.
pub async fn create(ctx: &TaskCtx) -> Result<()> {
    let db = &ctx.state.db;
    let id = target(ctx)?;
    let backup = mst_db::backups::get(db, id).await?;
    let result = take(ctx, &backup).await;
    match &result {
        Ok(t) => mst_db::backups::finish(db, id, t.stored_bytes as i64, t.consistent).await?,
        Err(e) => mst_db::backups::fail(db, id, &format!("{e:#}")).await?,
    }
    let taken = result?;
    tracing::info!(backup = %id, vm = %backup.vm_name, stored = taken.stored_bytes, consistent = taken.consistent, "backup taken");
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
    let mut disks = vec![];
    for (i, d) in vm.spec.0.disks.iter().enumerate() {
        let pool = mst_db::storage_pools::get(db, d.pool_id).await?;
        disks.push(BackupDisk {
            index: i as u32,
            volume_id: d.volume_id.to_string(),
            pool: Some(crate::api::agent_pool(&pool)?),
        });
    }
    ctx.step("copying disks", 0.1).await;
    let out = send(
        &ctx.state,
        host,
        Cmd::BackupVm(BackupVm {
            vm_id: vm.id.to_string(),
            backup_id: backup.id.to_string(),
            repository: Some(crate::api::repository_pool(&repo)),
            disks,
        }),
        BACKUP_TIMEOUT,
    )
    .await?;
    serde_json::from_str(&out).context("the agent's answer")
}

/// Sends a command, waiting out a moment without the host's agent (a
/// reconnect); commands sent here are idempotent.
async fn send(state: &AppState, host: HostId, cmd: Cmd, timeout: Duration) -> Result<String> {
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

/// Deletes a backup's files, then its record.
pub async fn delete(ctx: &TaskCtx) -> Result<()> {
    let state = &ctx.state;
    let id = target(ctx)?;
    let backup = match mst_db::backups::get(&state.db, id).await {
        Ok(b) => b,
        // Already gone (a retried task).
        Err(mst_db::Error::Domain(mst_core::Error::NotFound { .. })) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let repo = mst_db::backups::get_repository(&state.db, backup.repository_id).await?;
    let host = any_host(state, backup.vm_id).await?;
    send(
        state,
        host,
        Cmd::DeleteBackup(DeleteBackup {
            repository: Some(crate::api::repository_pool(&repo)),
            backup_id: id.to_string(),
        }),
        QUICK,
    )
    .await
    .context("removing the backup's files")?;
    mst_db::backups::delete(&state.db, id).await?;
    Ok(())
}

/// A host that can reach the repository: the VM's, or any connected one.
async fn any_host(state: &AppState, vm: Option<mst_core::ids::VmId>) -> Result<HostId> {
    if let Some(vm) = vm
        && let Ok(vm) = mst_db::vms::get(&state.db, vm).await
        && let Some(host) = vm.host_id
        && mst_db::hosts::get(&state.db, host)
            .await
            .is_ok_and(|h| h.connected_to.is_some())
    {
        return Ok(host);
    }
    mst_db::hosts::list(&state.db)
        .await?
        .into_iter()
        .find(|h| h.state != "decommissioned" && h.connected_to.is_some())
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
        match crate::api::start_backup(state, None, &vm, &repo, true).await {
            Ok((_, b)) => tracing::info!(vm = %vm.name, backup = %b.id, "scheduled backup started"),
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
