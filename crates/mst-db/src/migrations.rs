//! Live migrations in progress.

use chrono::{DateTime, Utc};
use mst_core::ids::{HostId, TaskId, VmId};
use sqlx::{PgConnection, PgExecutor};

use crate::{Result, conflict_on_unique};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Migration {
    pub vm_id: VmId,
    pub task_id: TaskId,
    pub source_host_id: HostId,
    pub dest_host_id: HostId,
    /// `preparing`, `running` or `committing`.
    pub state: String,
    pub cancel_requested: bool,
    pub started_at: DateTime<Utc>,
    /// For migrations the controller drives: the step reached and what it
    /// handed on.
    pub handoff: sqlx::types::Json<serde_json::Value>,
}

macro_rules! columns {
    () => {
        "vm_id, task_id, source_host_id, dest_host_id, state, cancel_requested, started_at, handoff"
    };
}

/// Starts tracking a migration: reserves the VM's CPU and memory on the
/// destination and marks the VM migrating. Fails if one is already running.
pub async fn begin(
    db: &mut PgConnection,
    vm: VmId,
    task: TaskId,
    source: HostId,
    dest: HostId,
    vcpus: u32,
    memory_mib: u64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO migrations (vm_id, task_id, source_host_id, dest_host_id, vcpus, memory_mib)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(vm)
    .bind(task)
    .bind(source)
    .bind(dest)
    .bind(vcpus as i32)
    .bind(memory_mib as i64)
    .execute(&mut *db)
    .await
    .map_err(|e| conflict_on_unique(e, "a migration of this VM"))?;
    sqlx::query("UPDATE vms SET status = 'migrating', updated_at = now() WHERE id = $1")
        .bind(vm)
        .execute(&mut *db)
        .await?;
    Ok(())
}

/// Every migration under way.
pub async fn list(db: impl PgExecutor<'_>) -> Result<Vec<Migration>> {
    Ok(
        sqlx::query_as(concat!("SELECT ", columns!(), " FROM migrations"))
            .fetch_all(db)
            .await?,
    )
}

pub async fn get(db: impl PgExecutor<'_>, vm: VmId) -> Result<Option<Migration>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        columns!(),
        " FROM migrations WHERE vm_id = $1"
    ))
    .bind(vm)
    .fetch_optional(db)
    .await?)
}

/// Migrations run by a task (one for a VM's migration, several in turn for
/// a host's evacuation).
pub async fn of_task(db: impl PgExecutor<'_>, task: TaskId) -> Result<Vec<Migration>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        columns!(),
        " FROM migrations WHERE task_id = $1 ORDER BY started_at"
    ))
    .bind(task)
    .fetch_all(db)
    .await?)
}

pub async fn set_state(db: impl PgExecutor<'_>, vm: VmId, state: &str) -> Result<()> {
    sqlx::query("UPDATE migrations SET state = $2 WHERE vm_id = $1")
        .bind(vm)
        .bind(state)
        .execute(db)
        .await?;
    Ok(())
}

/// Records how far a driven migration has got.
pub async fn set_handoff(
    db: impl PgExecutor<'_>,
    vm: VmId,
    handoff: &serde_json::Value,
) -> Result<()> {
    sqlx::query("UPDATE migrations SET handoff = $2 WHERE vm_id = $1")
        .bind(vm)
        .bind(sqlx::types::Json(handoff))
        .execute(db)
        .await?;
    Ok(())
}

/// Asks the migration's task to abort it. Returns whether one was running.
pub async fn request_cancel(db: impl PgExecutor<'_>, vm: VmId) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE migrations SET cancel_requested = true WHERE vm_id = $1")
            .bind(vm)
            .execute(db)
            .await?
            .rows_affected()
            == 1,
    )
}

/// Ends tracking: the VM stays where it was (rolled back) and is ready again.
pub async fn rolled_back(db: &mut PgConnection, vm: VmId) -> Result<()> {
    sqlx::query("DELETE FROM migrations WHERE vm_id = $1")
        .bind(vm)
        .execute(&mut *db)
        .await?;
    sqlx::query("UPDATE vms SET status = 'ready', updated_at = now() WHERE id = $1 AND status = 'migrating'")
        .bind(vm)
        .execute(&mut *db)
        .await?;
    Ok(())
}

/// Ends tracking: the VM now lives on the destination.
pub async fn committed(db: &mut PgConnection, vm: VmId, dest: HostId) -> Result<()> {
    crate::vms::moved(db, vm, dest).await?;
    sqlx::query("DELETE FROM migrations WHERE vm_id = $1")
        .bind(vm)
        .execute(&mut *db)
        .await?;
    Ok(())
}
