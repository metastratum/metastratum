//! Backups of VMs, and the repositories they are kept in.

use chrono::{DateTime, Utc};
use mst_core::ids::{BackupId, BackupRepositoryId, TenantId, VmId};
use serde_json::Value;
use sqlx::{PgConnection, PgExecutor};

use crate::{Result, conflict_on_unique};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Repository {
    pub id: BackupRepositoryId,
    pub name: String,
    /// `nfs`.
    pub kind: String,
    pub config: Value,
    pub created_at: DateTime<Utc>,
}

pub async fn list_repositories(db: impl PgExecutor<'_>) -> Result<Vec<Repository>> {
    Ok(sqlx::query_as(
        "SELECT id, name, kind, config, created_at FROM backup_repositories ORDER BY name",
    )
    .fetch_all(db)
    .await?)
}

pub async fn get_repository(db: impl PgExecutor<'_>, id: BackupRepositoryId) -> Result<Repository> {
    sqlx::query_as(
        "SELECT id, name, kind, config, created_at FROM backup_repositories WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        mst_core::Error::NotFound {
            kind: "backup repository",
            id: id.to_string(),
        }
        .into()
    })
}

pub async fn create_repository(
    db: impl PgExecutor<'_>,
    name: &str,
    kind: &str,
    config: &Value,
) -> Result<BackupRepositoryId> {
    let id = BackupRepositoryId::new();
    sqlx::query("INSERT INTO backup_repositories (id, name, kind, config) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(name)
        .bind(kind)
        .bind(config)
        .execute(db)
        .await
        .map_err(|e| conflict_on_unique(e, "a backup repository with this name"))?;
    Ok(id)
}

/// Removes a repository that holds no backups, its grants, and the schedules
/// that used it. False if backups are left.
pub async fn delete_repository(db: &mut PgConnection, id: BackupRepositoryId) -> Result<bool> {
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM backups WHERE repository_id = $1")
        .bind(id)
        .fetch_one(&mut *db)
        .await?;
    if left > 0 {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE vms SET backup_schedule = NULL, backup_keep = NULL, backup_repository_id = NULL
         WHERE backup_repository_id = $1",
    )
    .bind(id)
    .execute(&mut *db)
    .await?;
    sqlx::query("DELETE FROM tenant_grants WHERE kind = 'backup_repository' AND ref_id = $1")
        .bind(id.0)
        .execute(&mut *db)
        .await?;
    sqlx::query("DELETE FROM backup_repositories WHERE id = $1")
        .bind(id)
        .execute(&mut *db)
        .await?;
    Ok(true)
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Backup {
    pub id: BackupId,
    pub tenant_id: TenantId,
    pub vm_id: Option<VmId>,
    pub vm_name: String,
    pub repository_id: BackupRepositoryId,
    /// `running`, `ready` or `failed`.
    pub state: String,
    pub scheduled: bool,
    pub spec: sqlx::types::Json<mst_core::vm::VmSpec>,
    pub size_bytes: i64,
    pub stored_bytes: Option<i64>,
    pub consistent: Option<bool>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

macro_rules! backup_columns {
    () => {
        "id, tenant_id, vm_id, vm_name, repository_id, state, scheduled, spec, size_bytes,
         stored_bytes, consistent, error, created_at, completed_at"
    };
}

/// Records a backup about to be taken.
#[allow(clippy::too_many_arguments)]
pub async fn create(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    vm_id: VmId,
    vm_name: &str,
    repository_id: BackupRepositoryId,
    scheduled: bool,
    spec: &mst_core::vm::VmSpec,
    size_bytes: i64,
) -> Result<BackupId> {
    let id = BackupId::new();
    sqlx::query(
        "INSERT INTO backups (id, tenant_id, vm_id, vm_name, repository_id, state, scheduled, spec, size_bytes)
         VALUES ($1, $2, $3, $4, $5, 'running', $6, $7, $8)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(vm_id)
    .bind(vm_name)
    .bind(repository_id)
    .bind(scheduled)
    .bind(sqlx::types::Json(spec))
    .bind(size_bytes)
    .execute(db)
    .await?;
    Ok(id)
}

pub async fn get(db: impl PgExecutor<'_>, id: BackupId) -> Result<Backup> {
    sqlx::query_as(concat!(
        "SELECT ",
        backup_columns!(),
        " FROM backups WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        mst_core::Error::NotFound {
            kind: "backup",
            id: id.to_string(),
        }
        .into()
    })
}

/// A VM's backups, newest first.
pub async fn of_vm(db: impl PgExecutor<'_>, vm_id: VmId) -> Result<Vec<Backup>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        backup_columns!(),
        " FROM backups WHERE vm_id = $1 ORDER BY created_at DESC"
    ))
    .bind(vm_id)
    .fetch_all(db)
    .await?)
}

/// The backups of tenants (and their subtrees), newest first.
pub async fn in_subtrees(db: impl PgExecutor<'_>, roots: &[TenantId]) -> Result<Vec<Backup>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        backup_columns!(),
        " FROM backups WHERE tenant_id IN (SELECT id FROM tenants
             WHERE path <@ ANY (SELECT path FROM tenants WHERE id = ANY($1)))
         ORDER BY created_at DESC"
    ))
    .bind(roots.iter().map(|t| t.0).collect::<Vec<_>>())
    .fetch_all(db)
    .await?)
}

/// A backup was taken: it is ready, takes `stored` bytes, and its file
/// systems were `consistent` or not.
pub async fn finish(
    db: impl PgExecutor<'_>,
    id: BackupId,
    stored: i64,
    consistent: bool,
) -> Result<()> {
    sqlx::query(
        "UPDATE backups SET state = 'ready', stored_bytes = $2, consistent = $3, error = NULL,
             completed_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(stored)
    .bind(consistent)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn fail(db: impl PgExecutor<'_>, id: BackupId, error: &str) -> Result<()> {
    sqlx::query(
        "UPDATE backups SET state = 'failed', error = $2, completed_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(error)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete(db: impl PgExecutor<'_>, id: BackupId) -> Result<()> {
    sqlx::query("DELETE FROM backups WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// A VM's schedule: how often, how many to keep, and where.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Schedule {
    pub vm_id: VmId,
    pub every: String,
    pub keep: i32,
    pub repository_id: BackupRepositoryId,
}

pub async fn schedule_of(db: impl PgExecutor<'_>, vm_id: VmId) -> Result<Option<Schedule>> {
    Ok(sqlx::query_as(
        "SELECT id AS vm_id, backup_schedule AS every, backup_keep AS keep, backup_repository_id AS repository_id
         FROM vms WHERE id = $1 AND backup_schedule IS NOT NULL",
    )
    .bind(vm_id)
    .fetch_optional(db)
    .await?)
}

/// Sets or (with `None`) clears a VM's schedule.
pub async fn set_schedule(
    db: impl PgExecutor<'_>,
    vm_id: VmId,
    schedule: Option<(&str, i32, BackupRepositoryId)>,
) -> Result<()> {
    let (every, keep, repo) = match schedule {
        Some((e, k, r)) => (Some(e), Some(k), Some(r)),
        None => (None, None, None),
    };
    sqlx::query("UPDATE vms SET backup_schedule = $2, backup_keep = $3, backup_repository_id = $4 WHERE id = $1")
        .bind(vm_id)
        .bind(every)
        .bind(keep)
        .bind(repo)
        .execute(db)
        .await?;
    Ok(())
}

/// VMs whose schedule wants a backup now: none taken by it within its
/// interval, and none running.
pub async fn due(db: impl PgExecutor<'_>) -> Result<Vec<Schedule>> {
    Ok(sqlx::query_as(
        "SELECT v.id AS vm_id, v.backup_schedule AS every, v.backup_keep AS keep,
                v.backup_repository_id AS repository_id
         FROM vms v
         WHERE v.backup_schedule IS NOT NULL AND v.status <> 'deleting'
           AND NOT EXISTS (
             SELECT 1 FROM backups b WHERE b.vm_id = v.id AND (b.state = 'running' OR (
               b.scheduled AND b.state = 'ready' AND b.created_at > now() - CASE v.backup_schedule
                 WHEN 'daily' THEN interval '1 day' ELSE interval '7 days' END)))",
    )
    .fetch_all(db)
    .await?)
}

/// A VM's scheduled backups beyond the newest `keep` that are ready: the
/// ones its schedule no longer keeps.
pub async fn beyond_keep(db: impl PgExecutor<'_>, vm_id: VmId, keep: i32) -> Result<Vec<BackupId>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM backups WHERE vm_id = $1 AND scheduled AND state = 'ready'
         ORDER BY created_at DESC OFFSET $2",
    )
    .bind(vm_id)
    .bind(i64::from(keep))
    .fetch_all(db)
    .await?)
}

/// Whether a backup of the VM is being taken.
pub async fn running_for(db: impl PgExecutor<'_>, vm_id: VmId) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM backups WHERE vm_id = $1 AND state = 'running')",
    )
    .bind(vm_id)
    .fetch_one(db)
    .await?)
}

/// Whether a VM is still being created from one of the backup's disks.
pub async fn being_restored(db: impl PgExecutor<'_>, id: BackupId) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM vms WHERE status = 'creating'
             AND spec->'disks' @> jsonb_build_array(jsonb_build_object('restored_from',
                 jsonb_build_object('backup_id', $1::text))))",
    )
    .bind(id.to_string())
    .fetch_one(db)
    .await?)
}
