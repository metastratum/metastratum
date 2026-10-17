//! Snapshots of VMs' disks.

use chrono::{DateTime, Utc};
use mst_core::ids::{SnapshotId, TenantId, VmId};
use sqlx::PgExecutor;

use crate::{Result, conflict_on_unique};

/// How many snapshots a VM may have: each one slows its disks' writes a
/// little, and keeps what has changed since.
pub const MAX_PER_VM: i64 = 16;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Snapshot {
    pub id: SnapshotId,
    pub tenant_id: TenantId,
    pub vm_id: VmId,
    pub name: String,
    /// `creating`, `ready`, `reverting`, `deleting` or `failed`.
    pub state: String,
    pub spec: sqlx::types::Json<mst_core::vm::VmSpec>,
    pub consistent: Option<bool>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

macro_rules! columns {
    () => {
        "id, tenant_id, vm_id, name, state, spec, consistent, error, created_at"
    };
}

/// Records a snapshot about to be taken.
pub async fn create(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    vm_id: VmId,
    name: &str,
    spec: &mst_core::vm::VmSpec,
) -> Result<SnapshotId> {
    let id = SnapshotId::new();
    // Charged (`snapshot_gib`) the size of the VM's disks now.
    sqlx::query(
        "INSERT INTO snapshots (id, tenant_id, vm_id, name, state, spec, size_bytes)
         VALUES ($1, $2, $3, $4, 'creating', $5,
                 (SELECT coalesce(sum(size_bytes), 0) FROM volumes WHERE vm_id = $3))",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(vm_id)
    .bind(name)
    .bind(sqlx::types::Json(spec))
    .execute(db)
    .await
    .map_err(|e| conflict_on_unique(e, "a snapshot of this VM with this name"))?;
    Ok(id)
}

pub async fn get(db: impl PgExecutor<'_>, id: SnapshotId) -> Result<Snapshot> {
    sqlx::query_as(concat!(
        "SELECT ",
        columns!(),
        " FROM snapshots WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        mst_core::Error::NotFound {
            kind: "snapshot",
            id: id.to_string(),
        }
        .into()
    })
}

/// A VM's snapshots, oldest first.
pub async fn of_vm(db: impl PgExecutor<'_>, vm_id: VmId) -> Result<Vec<Snapshot>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        columns!(),
        " FROM snapshots WHERE vm_id = $1 ORDER BY created_at"
    ))
    .bind(vm_id)
    .fetch_all(db)
    .await?)
}

/// Sets a snapshot's state; `consistent` once taken, `error` when failed.
pub async fn set_state(
    db: impl PgExecutor<'_>,
    id: SnapshotId,
    state: &str,
    consistent: Option<bool>,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE snapshots SET state = $2, consistent = coalesce($3, consistent), error = $4
         WHERE id = $1",
    )
    .bind(id)
    .bind(state)
    .bind(consistent)
    .bind(error)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete(db: impl PgExecutor<'_>, id: SnapshotId) -> Result<()> {
    sqlx::query("DELETE FROM snapshots WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// What a VM's snapshots are doing, if anything: `creating`, `reverting` or
/// `deleting`.
pub async fn busy(db: impl PgExecutor<'_>, vm_id: VmId) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT state FROM snapshots WHERE vm_id = $1
             AND state IN ('creating', 'reverting', 'deleting') LIMIT 1",
    )
    .bind(vm_id)
    .fetch_optional(db)
    .await?)
}

/// How many snapshots a VM has.
pub async fn count(db: impl PgExecutor<'_>, vm_id: VmId) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT count(*) FROM snapshots WHERE vm_id = $1")
            .bind(vm_id)
            .fetch_one(db)
            .await?,
    )
}
