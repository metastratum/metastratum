//! Disks: volumes as tenants see them, attached to a VM (its boot disk, or
//! one of its others) or on their own.

use chrono::{DateTime, Utc};
use mst_core::ids::{HostId, ImageId, StoragePoolId, TenantId, VmId, VolumeId};
use sqlx::{PgConnection, PgExecutor};

use crate::{Result, conflict_on_unique};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Disk {
    pub id: VolumeId,
    pub tenant_id: TenantId,
    /// Unset for disks made with a VM (its boot disk, a clone's copies).
    pub name: Option<String>,
    pub size_bytes: i64,
    pub pool_id: StoragePoolId,
    pub pool_name: String,
    /// `local`, `nfs` or `rbd`.
    pub pool_kind: String,
    /// Where it is, for disks on a host's local storage.
    pub host_id: Option<HostId>,
    pub host_name: Option<String>,
    pub vm_id: Option<VmId>,
    pub vm_name: Option<String>,
    /// Its VM's boot disk.
    pub boot: bool,
    /// Its VM's other disks sit on its SCSI controller (see
    /// `VmSpec::disk_hotplug`).
    pub vm_disk_hotplug: bool,
    pub image_id: Option<ImageId>,
    /// `creating`, `ready`, `attaching`, `detaching`, `moving`, `deleting`
    /// or `failed`.
    pub state: String,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

macro_rules! disk_select {
    () => {
        "SELECT v.id, v.tenant_id, v.name, v.size_bytes, v.pool_id, p.name AS pool_name,
                p.kind AS pool_kind, v.host_id, h.name AS host_name, v.vm_id, m.name AS vm_name,
                coalesce(m.spec->'disks'->0->>'volume_id' = v.id::text, false) AS boot,
                coalesce((m.spec->>'disk_hotplug')::boolean, false) AS vm_disk_hotplug,
                v.image_id, v.state, v.error, v.created_at
         FROM volumes v
         JOIN storage_pools p ON p.id = v.pool_id
         LEFT JOIN hosts h ON h.id = v.host_id
         LEFT JOIN vms m ON m.id = v.vm_id"
    };
}

fn not_found(id: VolumeId) -> crate::Error {
    mst_core::Error::NotFound {
        kind: "disk",
        id: id.to_string(),
    }
    .into()
}

/// The disks of tenants in the subtrees of `roots`, newest first.
pub async fn in_subtrees(db: impl PgExecutor<'_>, roots: &[TenantId]) -> Result<Vec<Disk>> {
    Ok(sqlx::query_as(concat!(
        disk_select!(),
        " WHERE v.tenant_id IN (SELECT id FROM tenants
             WHERE path <@ ANY (SELECT path FROM tenants WHERE id = ANY($1)))
         ORDER BY v.created_at DESC"
    ))
    .bind(roots.iter().map(|t| t.0).collect::<Vec<_>>())
    .fetch_all(db)
    .await?)
}

/// A VM's disks, in its definition's order: its boot disk first, then the
/// others as attached.
pub async fn of_vm(db: impl PgExecutor<'_>, vm: VmId) -> Result<Vec<Disk>> {
    Ok(sqlx::query_as(concat!(
        disk_select!(),
        " WHERE v.vm_id = $1
          ORDER BY (SELECT e.n FROM jsonb_array_elements(m.spec->'disks') WITH ORDINALITY e(d, n)
                    WHERE e.d->>'volume_id' = v.id::text) NULLS LAST, v.created_at"
    ))
    .bind(vm)
    .fetch_all(db)
    .await?)
}

pub async fn get(db: impl PgExecutor<'_>, id: VolumeId) -> Result<Disk> {
    sqlx::query_as(concat!(disk_select!(), " WHERE v.id = $1"))
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| not_found(id))
}

/// The disk, locked for the transaction.
pub async fn get_for_update(db: &mut PgConnection, id: VolumeId) -> Result<Disk> {
    sqlx::query_as(concat!(disk_select!(), " WHERE v.id = $1 FOR UPDATE OF v"))
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| not_found(id))
}

pub struct NewDisk<'a> {
    pub id: VolumeId,
    pub tenant_id: TenantId,
    pub name: &'a str,
    pub pool_id: StoragePoolId,
    pub host_id: Option<HostId>,
    pub size_bytes: i64,
}

/// Records a disk being made (`creating`), attached to nothing.
pub async fn create(db: impl PgExecutor<'_>, d: &NewDisk<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO volumes (id, tenant_id, name, pool_id, host_id, size_bytes, state)
         VALUES ($1, $2, $3, $4, $5, $6, 'creating')",
    )
    .bind(d.id)
    .bind(d.tenant_id)
    .bind(d.name)
    .bind(d.pool_id)
    .bind(d.host_id)
    .bind(d.size_bytes)
    .execute(db)
    .await
    .map_err(|e| conflict_on_unique(e, "a disk of the tenant with that name"))?;
    Ok(())
}

pub async fn set_state(
    db: impl PgExecutor<'_>,
    id: VolumeId,
    state: &str,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE volumes SET state = $2, error = $3, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(state)
        .bind(error)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn rename(db: impl PgExecutor<'_>, id: VolumeId, name: &str) -> Result<()> {
    sqlx::query("UPDATE volumes SET name = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(name)
        .execute(db)
        .await
        .map_err(|e| conflict_on_unique(e, "a disk of the tenant with that name"))?;
    Ok(())
}

/// Attaches the disk to `vm` (`None`: detaches it).
pub async fn set_vm(db: impl PgExecutor<'_>, id: VolumeId, vm: Option<VmId>) -> Result<()> {
    sqlx::query("UPDATE volumes SET vm_id = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(vm)
        .execute(db)
        .await?;
    Ok(())
}

/// Where the disk now is: its pool, and its host for local storage. A
/// moved disk is a copy with its image's data in it: it depends on no
/// image any more.
pub async fn moved(
    db: impl PgExecutor<'_>,
    id: VolumeId,
    pool: StoragePoolId,
    host: Option<HostId>,
) -> Result<()> {
    sqlx::query(
        "UPDATE volumes SET pool_id = $2, host_id = $3, image_id = NULL, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(pool)
    .bind(host)
    .execute(db)
    .await?;
    Ok(())
}

/// How many disks are on the host's own storage.
pub async fn count_on_host(db: impl PgExecutor<'_>, host: HostId) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT count(*) FROM volumes WHERE host_id = $1")
            .bind(host)
            .fetch_one(db)
            .await?,
    )
}

/// What one of the VM's disks is being made to do (attached, detached,
/// grown), if any.
pub async fn busy_on_vm(db: impl PgExecutor<'_>, vm: VmId) -> Result<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT state FROM volumes WHERE vm_id = $1 AND state IN ('attaching', 'detaching', 'growing') LIMIT 1",
    )
    .bind(vm)
    .fetch_optional(db)
    .await?)
}

/// How many disks are being moved onto the host.
pub async fn moving_to_host(db: impl PgExecutor<'_>, host: HostId) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM tasks WHERE kind = 'disk.move' AND state IN ('pending', 'running')
           AND params->>'host' = $1::text",
    )
    .bind(host)
    .fetch_one(db)
    .await?)
}
