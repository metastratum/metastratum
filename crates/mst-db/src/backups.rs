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
        "UPDATE vms SET backup_schedule = NULL, backup_keep = NULL, backup_repository_id = NULL,
             backup_full_every = NULL
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
    /// `running`, `ready`, `failed` or `deleting` (being merged into the
    /// backup built on it, and removed).
    pub state: String,
    pub scheduled: bool,
    /// `full`, or `incremental`: how it was taken.
    pub kind: String,
    /// The backup it builds on now; `None` when it stands alone.
    pub parent_id: Option<BackupId>,
    /// The backups built on one another, named after the first.
    pub chain_id: BackupId,
    /// Why it was taken in full although asked to be incremental.
    pub full_reason: Option<String>,
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
        "id, tenant_id, vm_id, vm_name, repository_id, state, scheduled, kind, parent_id,
         chain_id, full_reason, spec, size_bytes, stored_bytes, consistent, error, created_at,
         completed_at"
    };
}

/// What a backup is to be: full, or (`parent`) the changes since another.
pub enum Taking<'a> {
    Full { reason: Option<&'a str> },
    Incremental { parent: &'a Backup },
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
    taking: Taking<'_>,
) -> Result<BackupId> {
    let id = BackupId::new();
    let (kind, parent, chain, reason) = match taking {
        Taking::Full { reason } => ("full", None, id, reason),
        Taking::Incremental { parent } => ("incremental", Some(parent.id), parent.chain_id, None),
    };
    sqlx::query(
        "INSERT INTO backups (id, tenant_id, vm_id, vm_name, repository_id, state, scheduled, spec,
                              size_bytes, kind, parent_id, chain_id, full_reason)
         VALUES ($1, $2, $3, $4, $5, 'running', $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(vm_id)
    .bind(vm_name)
    .bind(repository_id)
    .bind(scheduled)
    .bind(sqlx::types::Json(spec))
    .bind(size_bytes)
    .bind(kind)
    .bind(parent)
    .bind(chain)
    .bind(reason)
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
/// systems were `consistent` or not. One asked to be incremental that was
/// taken in full (`full_reason`) stands alone, and starts a chain. The VM
/// counts its incremental backups since its last full one.
pub async fn finish(
    db: &mut PgConnection,
    id: BackupId,
    stored: i64,
    consistent: bool,
    full_reason: Option<&str>,
) -> Result<()> {
    let b = get(&mut *db, id).await?;
    let incremental = b.kind == "incremental" && full_reason.is_none();
    sqlx::query(
        "UPDATE backups SET state = 'ready', stored_bytes = $2, consistent = $3, error = NULL,
             completed_at = now(),
             kind = CASE WHEN $4 THEN kind ELSE 'full' END,
             parent_id = CASE WHEN $4 THEN parent_id END,
             chain_id = CASE WHEN $4 THEN chain_id ELSE id END,
             full_reason = coalesce($5, full_reason)
         WHERE id = $1",
    )
    .bind(id)
    .bind(stored)
    .bind(consistent)
    .bind(incremental)
    .bind(full_reason)
    .execute(&mut *db)
    .await?;
    if let Some(vm) = b.vm_id {
        sqlx::query(
            "UPDATE vms SET backup_incrementals = CASE WHEN $2 THEN backup_incrementals + 1 ELSE 0 END
             WHERE id = $1",
        )
        .bind(vm)
        .bind(incremental)
        .execute(&mut *db)
        .await?;
    }
    Ok(())
}

/// A backup failed. Nothing builds on it, and it builds on nothing.
pub async fn fail(db: impl PgExecutor<'_>, id: BackupId, error: &str) -> Result<()> {
    sqlx::query(
        "UPDATE backups SET state = 'failed', error = $2, completed_at = now(), parent_id = NULL
         WHERE id = $1",
    )
    .bind(id)
    .bind(error)
    .execute(db)
    .await?;
    Ok(())
}

/// A backup is to be deleted: from now on nothing builds on it or restores
/// from it, and its files may change (merged into its children).
pub async fn set_deleting(db: impl PgExecutor<'_>, id: BackupId) -> Result<()> {
    sqlx::query("UPDATE backups SET state = 'deleting', error = NULL WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Deleting a backup failed; it stays `deleting`, to be deleted again.
pub async fn delete_failed(db: impl PgExecutor<'_>, id: BackupId, error: &str) -> Result<()> {
    sqlx::query("UPDATE backups SET error = $2 WHERE id = $1 AND state = 'deleting'")
        .bind(id)
        .bind(error)
        .execute(db)
        .await?;
    Ok(())
}

/// The backups built on `id` (not failed ones), oldest first.
pub async fn children(db: impl PgExecutor<'_>, id: BackupId) -> Result<Vec<Backup>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        backup_columns!(),
        " FROM backups WHERE parent_id = $1 AND state <> 'failed' ORDER BY created_at"
    ))
    .bind(id)
    .fetch_all(db)
    .await?)
}

/// `deleted` was merged into its children (now taking the bytes given):
/// they build on what it built on, and its record goes.
pub async fn merged(
    db: &mut PgConnection,
    deleted: &Backup,
    children: &[(BackupId, i64)],
) -> Result<()> {
    for (child, stored) in children {
        sqlx::query("UPDATE backups SET parent_id = $2, stored_bytes = $3 WHERE id = $1")
            .bind(child)
            .bind(deleted.parent_id)
            .bind(stored)
            .execute(&mut *db)
            .await?;
    }
    // Failed ones never built on it; anything else left does not either.
    sqlx::query("UPDATE backups SET parent_id = $2 WHERE parent_id = $1")
        .bind(deleted.id)
        .bind(deleted.parent_id)
        .execute(&mut *db)
        .await?;
    delete(&mut *db, deleted.id).await
}

/// The newest complete backup of a VM in a repository: what an incremental
/// one there builds on.
pub async fn newest_ready(
    db: impl PgExecutor<'_>,
    vm_id: VmId,
    repository_id: BackupRepositoryId,
) -> Result<Option<Backup>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        backup_columns!(),
        " FROM backups WHERE vm_id = $1 AND repository_id = $2 AND state = 'ready'
         ORDER BY created_at DESC LIMIT 1"
    ))
    .bind(vm_id)
    .bind(repository_id)
    .fetch_optional(db)
    .await?)
}

/// The VM's backups taken up to `parent` (included), newest first: where
/// its change tracking may have started. Changes since any of them include
/// the changes since `parent`.
pub async fn taken_before(
    db: impl PgExecutor<'_>,
    vm_id: VmId,
    parent: &Backup,
) -> Result<Vec<BackupId>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM backups WHERE vm_id = $1 AND created_at <= $2
         ORDER BY created_at DESC LIMIT 32",
    )
    .bind(vm_id)
    .bind(parent.created_at)
    .fetch_all(db)
    .await?)
}

/// Whether one of the backup's chain up to it (it, its parent, ...) is
/// being deleted: its files may be changing.
pub async fn deleting_below(db: impl PgExecutor<'_>, id: BackupId) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "WITH RECURSIVE up AS (
             SELECT id, parent_id, state FROM backups WHERE id = $1
             UNION ALL
             SELECT b.id, b.parent_id, b.state FROM backups b JOIN up ON b.id = up.parent_id)
         SELECT EXISTS (SELECT 1 FROM up WHERE state = 'deleting')",
    )
    .bind(id)
    .fetch_one(db)
    .await?)
}

/// Whether a backup being taken builds on this one.
pub async fn child_running(db: impl PgExecutor<'_>, id: BackupId) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM backups WHERE parent_id = $1 AND state = 'running')",
    )
    .bind(id)
    .fetch_one(db)
    .await?)
}

/// Holds the lock serializing work on a chain of backups (taking one on
/// it, deleting one of it), on a connection of its own: released when
/// dropped, or if this process dies.
pub struct ChainLock(PgConnection);

impl ChainLock {
    pub async fn take(pool: &sqlx::PgPool, chain: BackupId) -> Result<Self> {
        use sqlx::Connection;
        let mut conn = PgConnection::connect_with(&pool.connect_options()).await?;
        sqlx::query("SELECT pg_advisory_lock(hashtextextended('backup-chain:' || $1::text, 0))")
            .bind(chain)
            .execute(&mut conn)
            .await?;
        Ok(Self(conn))
    }

    pub async fn release(self) {
        use sqlx::Connection;
        // Closing the session drops its advisory locks.
        let _ = self.0.close().await;
    }
}

pub async fn delete(db: impl PgExecutor<'_>, id: BackupId) -> Result<()> {
    sqlx::query("DELETE FROM backups WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// A VM's schedule: how often, how many to keep, where, and how often in
/// full.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Schedule {
    pub vm_id: VmId,
    pub every: String,
    pub keep: i32,
    pub repository_id: BackupRepositoryId,
    /// A full backup every this many (1: every one).
    pub full_every: i32,
    /// The VM's incremental backups since its last full one.
    pub incrementals: i32,
}

impl Schedule {
    /// Whether its next backup is to be full.
    pub fn full_due(&self) -> bool {
        self.incrementals + 1 >= self.full_every
    }
}

macro_rules! schedule_columns {
    () => {
        "v.id AS vm_id, v.backup_schedule AS every, v.backup_keep AS keep,
         v.backup_repository_id AS repository_id, coalesce(v.backup_full_every, 1) AS full_every,
         v.backup_incrementals AS incrementals"
    };
}

pub async fn schedule_of(db: impl PgExecutor<'_>, vm_id: VmId) -> Result<Option<Schedule>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        schedule_columns!(),
        " FROM vms v WHERE v.id = $1 AND v.backup_schedule IS NOT NULL"
    ))
    .bind(vm_id)
    .fetch_optional(db)
    .await?)
}

/// A schedule to set: how often, how many to keep, where, and a full
/// backup every how many.
pub struct NewSchedule<'a> {
    pub every: &'a str,
    pub keep: i32,
    pub repository_id: BackupRepositoryId,
    pub full_every: i32,
}

/// Sets or (with `None`) clears a VM's schedule.
pub async fn set_schedule(
    db: impl PgExecutor<'_>,
    vm_id: VmId,
    schedule: Option<NewSchedule<'_>>,
) -> Result<()> {
    let (every, keep, repo, full_every) = match schedule {
        Some(s) => (
            Some(s.every),
            Some(s.keep),
            Some(s.repository_id),
            Some(s.full_every),
        ),
        None => (None, None, None, None),
    };
    sqlx::query(
        "UPDATE vms SET backup_schedule = $2, backup_keep = $3, backup_repository_id = $4,
             backup_full_every = $5 WHERE id = $1",
    )
    .bind(vm_id)
    .bind(every)
    .bind(keep)
    .bind(repo)
    .bind(full_every)
    .execute(db)
    .await?;
    Ok(())
}

/// VMs whose schedule wants a backup now: none taken by it within its
/// interval, and none running.
pub async fn due(db: impl PgExecutor<'_>) -> Result<Vec<Schedule>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        schedule_columns!(),
        " FROM vms v
         WHERE v.backup_schedule IS NOT NULL AND v.status <> 'deleting'
           AND NOT EXISTS (
             SELECT 1 FROM backups b WHERE b.vm_id = v.id AND (b.state = 'running' OR (
               b.scheduled AND b.state = 'ready' AND b.created_at > now() - CASE v.backup_schedule
                 WHEN 'daily' THEN interval '1 day' ELSE interval '7 days' END)))"
    ))
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

/// Whether a VM is still being created from the backup, or from one built
/// on it (which reads its files too).
pub async fn being_restored(db: impl PgExecutor<'_>, id: BackupId) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "WITH RECURSIVE down AS (
             SELECT id FROM backups WHERE id = $1
             UNION ALL
             SELECT b.id FROM backups b JOIN down ON b.parent_id = down.id)
         SELECT EXISTS (SELECT 1 FROM vms, down WHERE vms.status = 'creating'
             AND vms.spec->'disks' @> jsonb_build_array(jsonb_build_object('restored_from',
                 jsonb_build_object('backup_id', down.id::text))))",
    )
    .bind(id)
    .fetch_one(db)
    .await?)
}
