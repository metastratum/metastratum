use chrono::{DateTime, Utc};
use mst_core::ids::{HostId, StoragePoolId, TenantId, UserId, VmId, VolumeId};
use mst_core::vm::VmSpec;
use sqlx::PgExecutor;
use sqlx::types::Json;

use crate::{Result, conflict_on_unique};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Vm {
    pub id: VmId,
    pub tenant_id: TenantId,
    pub name: String,
    pub host_id: Option<HostId>,
    pub spec: Json<VmSpec>,
    pub generation: i64,
    pub status: String,
    pub desired_power: String,
    pub power: String,
    pub power_reason: Option<String>,
    pub power_observed_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Kept to be cloned; it never starts.
    pub template: bool,
    /// Every feature its CPU model has, once worked out (see
    /// `mst_controller::cpu`).
    pub cpu_features: Option<Vec<String>>,
}

macro_rules! vm_columns {
    () => {
        "id, tenant_id, name, host_id, spec, generation, status, desired_power, power, power_reason, \
         power_observed_at, error, \
         created_by, created_at, updated_at, template, cpu_features"
    };
}

/// Records the features the VM's CPU model has (`None`: to work out again).
pub async fn set_cpu_features(
    db: impl PgExecutor<'_>,
    id: VmId,
    features: Option<&[String]>,
) -> Result<()> {
    sqlx::query("UPDATE vms SET cpu_features = $2 WHERE id = $1")
        .bind(id)
        .bind(features)
        .execute(db)
        .await?;
    Ok(())
}

pub struct NewVm<'a> {
    pub id: VmId,
    pub tenant_id: TenantId,
    pub name: &'a str,
    pub host_id: HostId,
    pub spec: &'a VmSpec,
    pub desired_power: &'a str,
    pub created_by: Option<UserId>,
}

pub async fn create(db: impl PgExecutor<'_>, new: NewVm<'_>) -> Result<Vm> {
    sqlx::query_as(concat!(
        "INSERT INTO vms (id, tenant_id, name, host_id, spec, desired_power, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING ",
        vm_columns!()
    ))
    .bind(new.id)
    .bind(new.tenant_id)
    .bind(new.name)
    .bind(new.host_id)
    .bind(Json(new.spec))
    .bind(new.desired_power)
    .bind(new.created_by)
    .fetch_one(db)
    .await
    .map_err(|e| conflict_on_unique(e, "a VM with this name"))
}

pub async fn get(db: impl PgExecutor<'_>, id: VmId) -> Result<Vm> {
    sqlx::query_as(concat!("SELECT ", vm_columns!(), " FROM vms WHERE id = $1"))
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| {
            mst_core::Error::NotFound {
                kind: "vm",
                id: id.to_string(),
            }
            .into()
        })
}

/// VMs owned by the tenant, or by it and all its descendants.
pub async fn list(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    descendants: bool,
) -> Result<Vec<Vm>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        vm_columns!(),
        " FROM vms WHERE tenant_id = $1
            OR ($2 AND tenant_id IN (SELECT id FROM tenants
                                     WHERE path <@ (SELECT path FROM tenants WHERE id = $1)))
         ORDER BY name, id"
    ))
    .bind(tenant_id)
    .bind(descendants)
    .fetch_all(db)
    .await?)
}

/// Every VM, of every tenant.
pub async fn all(db: impl PgExecutor<'_>) -> Result<Vec<Vm>> {
    Ok(
        sqlx::query_as(concat!("SELECT ", vm_columns!(), " FROM vms ORDER BY id"))
            .fetch_all(db)
            .await?,
    )
}

pub async fn list_on_host(db: impl PgExecutor<'_>, host_id: HostId) -> Result<Vec<Vm>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        vm_columns!(),
        " FROM vms WHERE host_id = $1"
    ))
    .bind(host_id)
    .fetch_all(db)
    .await?)
}

pub async fn set_status(
    db: impl PgExecutor<'_>,
    id: VmId,
    status: &str,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query("UPDATE vms SET status = $2, error = $3, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(status)
        .bind(error)
        .execute(db)
        .await?;
    Ok(())
}

/// Marks the VM as being deleted, unless it is already, or migrating.
/// Returns whether it did.
pub async fn start_deleting(db: impl PgExecutor<'_>, id: VmId) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE vms SET status = 'deleting', error = NULL, updated_at = now()
         WHERE id = $1 AND status NOT IN ('deleting', 'migrating')",
    )
    .bind(id)
    .execute(db)
    .await?
    .rows_affected()
        == 1)
}

/// Sets the VM's status as a task on it ends, unless it is being deleted
/// meanwhile (the delete, queued behind, sees to it).
pub async fn settle(
    db: impl PgExecutor<'_>,
    id: VmId,
    status: &str,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE vms SET status = $2, error = $3, updated_at = now()
         WHERE id = $1 AND status <> 'deleting'",
    )
    .bind(id)
    .bind(status)
    .bind(error)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn set_desired_power(db: impl PgExecutor<'_>, id: VmId, power: &str) -> Result<()> {
    sqlx::query("UPDATE vms SET desired_power = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(power)
        .execute(db)
        .await?;
    Ok(())
}

/// Records the power state a host reports, and why it changed if known.
/// Returns whether anything changed. A running VM shut down or powered off
/// other than through the API (from inside the guest, say) stays stopped:
/// it is now meant to be, as if stopped through the API, so it changes like
/// any stopped VM and is not restarted elsewhere if its host fails. One that
/// crashed or failed is still meant to run. While a VM migrates, its
/// source's reports (paused, then gone) say nothing about the VM; the
/// migration records the outcome itself.
pub async fn observe_power(
    db: impl PgExecutor<'_>,
    id: VmId,
    host_id: HostId,
    power: &str,
    reason: Option<&str>,
) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE vms SET power = $3, power_reason = $4, power_observed_at = now(), updated_at = now(),
                desired_power = CASE WHEN power <> 'stopped' AND $3 = 'stopped'
                                      AND $4 IN ('shutdown', 'destroyed')
                                     THEN 'stopped' ELSE desired_power END
         WHERE id = $1 AND host_id = $2 AND status <> 'migrating'
           AND (power <> $3 OR power_reason IS DISTINCT FROM $4)",
    )
    .bind(id)
    .bind(host_id)
    .bind(power)
    .bind(reason)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Moves a VM to the host it was migrated to: it runs there now, and its
/// host-local volumes are there too.
pub async fn moved(db: &mut sqlx::PgConnection, id: VmId, host: HostId) -> Result<()> {
    sqlx::query(
        "UPDATE vms SET host_id = $2, status = 'ready', power = 'running', power_reason = NULL,
                power_observed_at = now(), updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(host)
    .execute(&mut *db)
    .await?;
    sqlx::query("UPDATE volumes SET host_id = $2 WHERE vm_id = $1 AND host_id IS NOT NULL")
        .bind(id)
        .bind(host)
        .execute(&mut *db)
        .await?;
    Ok(())
}

/// The latest copy of a VM's TPM state: (state, sealed; its digest).
pub async fn tpm_state(db: impl PgExecutor<'_>, id: VmId) -> Result<Option<(String, String)>> {
    Ok(
        sqlx::query_as("SELECT state, digest FROM vm_tpm_states WHERE vm_id = $1")
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

/// Keeps a copy of a VM's TPM state (sealed), in place of the one before.
/// Nothing if the VM is gone.
pub async fn set_tpm_state(
    db: impl PgExecutor<'_>,
    id: VmId,
    sealed: &str,
    digest: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO vm_tpm_states (vm_id, state, digest)
         SELECT id, $2, $3 FROM vms WHERE id = $1
         ON CONFLICT (vm_id) DO UPDATE SET state = $2, digest = $3, updated_at = now()",
    )
    .bind(id)
    .bind(sealed)
    .bind(digest)
    .execute(db)
    .await?;
    Ok(())
}

/// Forgets the copy of a VM's TPM state (its TPM was removed).
pub async fn drop_tpm_state(db: impl PgExecutor<'_>, id: VmId) -> Result<()> {
    sqlx::query("DELETE FROM vm_tpm_states WHERE vm_id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// VMs with a TPM, on hosts: (VM, host).
pub async fn with_tpm(db: impl PgExecutor<'_>) -> Result<Vec<(VmId, HostId)>> {
    Ok(sqlx::query_as(
        "SELECT id, host_id FROM vms WHERE (spec->>'tpm')::boolean AND host_id IS NOT NULL
         AND status IN ('ready', 'error') ORDER BY id",
    )
    .fetch_all(db)
    .await?)
}

/// VMs whose first boot is pending: a Windows VM's password is kept for it
/// (see `CloudInit::first_boot_password`).
pub async fn first_boots_pending(db: impl PgExecutor<'_>) -> Result<Vec<VmId>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM vms WHERE spec->'cloud_init' ? 'first_boot_password' ORDER BY created_at",
    )
    .fetch_all(db)
    .await?)
}

/// The VM, locked until the transaction ends.
pub async fn get_for_update(db: &mut sqlx::PgConnection, id: VmId) -> Result<Vm> {
    sqlx::query_as(concat!(
        "SELECT ",
        vm_columns!(),
        " FROM vms WHERE id = $1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(&mut *db)
    .await?
    .ok_or_else(|| {
        mst_core::Error::NotFound {
            kind: "vm",
            id: id.to_string(),
        }
        .into()
    })
}

/// Replaces a VM's spec as a new generation of its definition. Returns the
/// generation.
pub async fn redefine(db: &mut sqlx::PgConnection, id: VmId, spec: &VmSpec) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "UPDATE vms SET spec = $2, generation = generation + 1, updated_at = now() WHERE id = $1 RETURNING generation",
    )
    .bind(id)
    .bind(Json(spec))
    .fetch_one(&mut *db)
    .await?)
}

pub async fn delete(db: impl PgExecutor<'_>, id: VmId) -> Result<()> {
    sqlx::query("DELETE FROM vms WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Volume {
    pub id: VolumeId,
    pub tenant_id: TenantId,
    pub vm_id: Option<VmId>,
    pub host_id: Option<HostId>,
    pub pool_id: StoragePoolId,
    pub size_bytes: i64,
}

pub async fn create_volume(db: impl PgExecutor<'_>, v: &Volume) -> Result<()> {
    sqlx::query(
        "INSERT INTO volumes (id, tenant_id, vm_id, host_id, pool_id, size_bytes)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(v.id)
    .bind(v.tenant_id)
    .bind(v.vm_id)
    .bind(v.host_id)
    .bind(v.pool_id)
    .bind(v.size_bytes)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn volumes_for_vm(db: impl PgExecutor<'_>, vm_id: VmId) -> Result<Vec<Volume>> {
    Ok(sqlx::query_as(
        "SELECT id, tenant_id, vm_id, host_id, pool_id, size_bytes FROM volumes
         WHERE vm_id = $1 ORDER BY created_at",
    )
    .bind(vm_id)
    .fetch_all(db)
    .await?)
}

pub async fn delete_volume(db: impl PgExecutor<'_>, id: VolumeId) -> Result<()> {
    sqlx::query("DELETE FROM volumes WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn set_template(db: impl PgExecutor<'_>, id: VmId, template: bool) -> Result<()> {
    sqlx::query("UPDATE vms SET template = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(template)
        .execute(db)
        .await?;
    Ok(())
}

/// Whether a VM is still being created with copies of `source`'s disks.
pub async fn being_cloned(db: impl PgExecutor<'_>, source: VmId) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM vms v, jsonb_array_elements(v.spec->'disks') d
             WHERE v.status = 'creating' AND d->>'cloned_from' IN (
                 SELECT s->>'volume_id' FROM vms src, jsonb_array_elements(src.spec->'disks') s
                 WHERE src.id = $1))",
    )
    .bind(source)
    .fetch_one(db)
    .await?)
}

pub async fn set_volume_size(db: impl PgExecutor<'_>, id: VolumeId, size: i64) -> Result<()> {
    sqlx::query("UPDATE volumes SET size_bytes = $2 WHERE id = $1")
        .bind(id)
        .bind(size)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn set_spec(db: impl PgExecutor<'_>, id: VmId, spec: &VmSpec) -> Result<()> {
    sqlx::query("UPDATE vms SET spec = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(Json(spec))
        .execute(db)
        .await?;
    Ok(())
}
