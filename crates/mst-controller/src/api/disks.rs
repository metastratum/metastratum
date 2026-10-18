//! Disks: each VM's boot disk, and disks of their own, which are attached to
//! a VM (plugged in while it runs) or not, grown, and deleted.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use mst_core::ids::{HostId, LOCAL_POOL, StoragePoolId, TaskId, TenantId, UserId, VmId, VolumeId};
use mst_core::vm::{DiskSpec, MAX_DATA_DISKS};
use mst_db::disks::Disk;
use mst_db::sqlx::PgConnection;
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::authz::{Perm, Principal, audit, authorize, authorize_resource};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

const GIB: u64 = 1 << 30;
/// The largest disk, in GiB (as for VMs' disks).
const MAX_GIB: u64 = 16384;
const AGENT: std::time::Duration = std::time::Duration::from_secs(60);

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_disks, create_disk))
        .routes(routes!(get_disk, update_disk, delete_disk))
        .routes(routes!(vm_disks, attach_disk))
        .routes(routes!(detach_disk))
        .routes(routes!(move_disk))
}

#[derive(Serialize, ToSchema)]
pub struct DiskView {
    id: VolumeId,
    tenant_id: TenantId,
    /// Unset for disks made with a VM: its boot disk, and copies of disks
    /// made with a clone or a restore.
    name: Option<String>,
    size_gib: u64,
    storage_pool_id: StoragePoolId,
    storage_pool_name: String,
    /// `local`, `nfs` or `rbd`.
    storage_pool_kind: String,
    /// The host it is on, for disks on local storage: it attaches to VMs
    /// there.
    host_id: Option<HostId>,
    host_name: Option<String>,
    /// The VM it is attached to.
    vm_id: Option<VmId>,
    vm_name: Option<String>,
    /// The VM's boot disk, which stays with it.
    boot: bool,
    /// Where the guest finds it, for disks attached besides the boot disk:
    /// by its serial number, which is its ID.
    guest_path: Option<String>,
    /// `creating`, `ready`, `attaching`, `detaching`, `moving`, `deleting`
    /// or `failed`.
    state: String,
    /// What went wrong, when `failed`.
    error: Option<String>,
    created_at: DateTime<Utc>,
}

impl From<Disk> for DiskView {
    fn from(d: Disk) -> Self {
        let guest_path = (d.vm_id.is_some() && !d.boot && d.vm_disk_hotplug).then(|| {
            format!(
                "/dev/disk/by-id/scsi-0QEMU_QEMU_HARDDISK_{}",
                d.id.0.simple()
            )
        });
        Self {
            id: d.id,
            tenant_id: d.tenant_id,
            name: d.name,
            size_gib: (d.size_bytes as u64).div_ceil(GIB),
            storage_pool_id: d.pool_id,
            storage_pool_name: d.pool_name,
            storage_pool_kind: d.pool_kind,
            host_id: d.host_id,
            host_name: d.host_name,
            vm_id: d.vm_id,
            vm_name: d.vm_name,
            boot: d.boot,
            guest_path,
            state: d.state,
            error: d.error,
            created_at: d.created_at,
        }
    }
}

async fn load(state: &AppState, p: &Principal, id: VolumeId, perm: Perm) -> ApiResult<Disk> {
    let d = mst_db::disks::get(&state.db, id).await?;
    authorize_resource(&state.db, p, perm, d.tenant_id, "disk").await?;
    Ok(d)
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Those of this tenant and its sub-tenants; by default, of every
    /// tenant the caller sees VMs of.
    tenant_id: Option<TenantId>,
}

/// Disks, newest first: VMs' boot disks, and disks attached to a VM or not.
#[utoipa::path(get, path = "/disks", tag = "disks", params(ListQuery),
    responses((status = 200, body = Vec<DiskView>)))]
async fn list_disks(
    State(state): State<AppState>,
    p: Principal,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<DiskView>>> {
    let roots = match q.tenant_id {
        Some(t) => {
            authorize_resource(&state.db, &p, Perm::VmRead, t, "tenant").await?;
            vec![t]
        }
        None => p.scopes(Perm::VmRead),
    };
    Ok(Json(
        mst_db::disks::in_subtrees(&state.db, &roots)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

#[utoipa::path(get, path = "/disks/{id}", tag = "disks", params(("id" = VolumeId, Path)),
    responses((status = 200, body = DiskView), (status = 404)))]
async fn get_disk(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VolumeId>,
) -> ApiResult<Json<DiskView>> {
    Ok(Json(load(&state, &p, id, Perm::VmRead).await?.into()))
}

/// A VM's disks: its boot disk first, then the others as attached.
#[utoipa::path(get, path = "/vms/{id}/disks", tag = "disks", params(("id" = VmId, Path)),
    responses((status = 200, body = Vec<DiskView>), (status = 404)))]
async fn vm_disks(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
) -> ApiResult<Json<Vec<DiskView>>> {
    let vm = mst_db::vms::get(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::VmRead, vm.tenant_id, "vm").await?;
    Ok(Json(
        mst_db::disks::of_vm(&state.db, id)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

fn check_name(name: &str) -> ApiResult<&str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "a disk's name is 1 to 64 characters, on one line",
        ));
    }
    Ok(name)
}

#[derive(Deserialize, ToSchema)]
pub struct CreateDisk {
    /// Unique among the tenant's disks.
    name: String,
    /// 1 to 16384.
    size_gib: u64,
    /// Defaults to the VM's tenant, or the caller's.
    tenant_id: Option<TenantId>,
    /// Local storage by default.
    storage_pool_id: Option<StoragePoolId>,
    /// The VM to attach it to right away (plugged in if it runs), on whose
    /// host a disk on local storage is made.
    vm_id: Option<VmId>,
    /// For a disk on local storage attached to no VM: the host it is made
    /// on, and attaches to VMs on.
    host_id: Option<HostId>,
}

#[derive(Serialize, ToSchema)]
pub struct DiskTask {
    /// The task attaching, detaching or moving it, if one was started:
    /// the disk is `attaching`, `detaching` or `moving` until it is done.
    task_id: Option<TaskId>,
    #[serde(flatten)]
    disk: DiskView,
}

async fn disk_task(state: &AppState, id: VolumeId, task_id: Option<TaskId>) -> ApiResult<DiskTask> {
    Ok(DiskTask {
        task_id,
        disk: mst_db::disks::get(&state.db, id).await?.into(),
    })
}

/// Makes an empty disk, attached to a VM or not (given one, a task attaches
/// it). It counts against the tenant's disk quota.
#[utoipa::path(post, path = "/disks", tag = "disks", request_body = CreateDisk,
    responses((status = 201, body = DiskTask), (status = 400), (status = 404),
        (status = 409, description = "No room in the quota, or the VM cannot take it now"),
        (status = 503)))]
async fn create_disk(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<CreateDisk>,
) -> ApiResult<(StatusCode, Json<DiskTask>)> {
    let name = check_name(&req.name)?;
    if req.size_gib == 0 || req.size_gib > MAX_GIB {
        return Err(ApiError::bad_request("size_gib is 1 to 16384"));
    }
    let vm = match req.vm_id {
        Some(id) => {
            let vm = mst_db::vms::get(&state.db, id).await?;
            authorize_resource(&state.db, &p, Perm::VmWrite, vm.tenant_id, "vm").await?;
            if req.tenant_id.is_some_and(|t| t != vm.tenant_id) {
                return Err(ApiError::bad_request(
                    "a disk is attached to VMs of its own tenant",
                ));
            }
            Some(vm)
        }
        None => None,
    };
    let tenant = match &vm {
        Some(vm) => vm.tenant_id,
        None => req.tenant_id.unwrap_or(p.tenant_id),
    };
    authorize(&state.db, &p, Perm::VmWrite, tenant).await?;
    let pool =
        mst_db::storage_pools::get(&state.db, req.storage_pool_id.unwrap_or(LOCAL_POOL)).await?;
    if !super::storage_pools::may_use(&state, tenant, &pool).await? {
        return Err(ApiError::not_found("storage pool"));
    }
    // Where it is made: on the VM's host, the host named, or (shared
    // storage) a host that reaches it.
    let host = match (pool.kind.as_str(), &vm, req.host_id) {
        ("local", Some(vm), _) => vm
            .host_id
            .ok_or_else(|| ApiError::conflict("the VM is on no host"))?,
        ("local", None, Some(h)) => {
            check_host(&state, tenant, h).await?;
            h
        }
        ("local", None, None) => {
            return Err(ApiError::bad_request(
                "a disk on local storage is on a host: give the VM to attach it to (vm_id), or the host (host_id)",
            ));
        }
        _ => match vm.as_ref().and_then(|vm| vm.host_id) {
            Some(h) => h,
            None => host_reaching(&state, &pool).await?,
        },
    };

    // Recorded (and counted) first; made on its host outside any
    // transaction, so no lock waits on the host.
    let id = VolumeId::new();
    let size = req.size_gib * GIB;
    let mut tx = state.db.begin().await?;
    let delta: mst_db::quotas::Amounts = [("disk_gib".to_owned(), req.size_gib as i64)]
        .into_iter()
        .collect();
    mst_db::quotas::reserve(&mut tx, tenant, &delta).await?;
    mst_db::disks::create(
        &mut *tx,
        &mst_db::disks::NewDisk {
            id,
            tenant_id: tenant,
            name,
            pool_id: pool.id,
            host_id: (pool.kind == "local").then_some(host),
            size_bytes: size as i64,
        },
    )
    .await?;
    tx.commit().await?;
    let cmd = mst_proto::EnsureVolume {
        volume_id: id.to_string(),
        size_bytes: size,
        image_id: String::new(),
        pool: Some(super::agent_pool(&pool).map_err(ApiError::internal)?),
    };
    if let Err(e) = state
        .agents
        .call(host, mst_proto::command::Cmd::EnsureVolume(cmd), AGENT)
        .await
    {
        // Not made after all: it goes, with whatever its host made.
        remove_volume(&state, host, &pool, id).await;
        mst_db::vms::delete_volume(&state.db, id).await?;
        return Err(ApiError::unavailable(format!(
            "making the disk on its host: {e}"
        )));
    }
    mst_db::disks::set_state(&state.db, id, "ready", None).await?;
    audit(
        &state.db,
        Some(&p),
        Some(tenant),
        "disk.create",
        Some(("disk", id.0)),
        json!({"name": name, "size_gib": req.size_gib, "pool": pool.name, "vm": vm.as_ref().map(|v| &v.name)}),
    )
    .await;
    let task = match &vm {
        Some(vm) => {
            let mut tx = state.db.begin().await?;
            let disk = mst_db::disks::get_for_update(&mut tx, id).await?;
            // Made: should attaching be refused now, the disk stays.
            let task = begin_attach(&state, &mut tx, vm.id, &disk, Some(p.user_id)).await?;
            tx.commit().await?;
            Some(task)
        }
        None => None,
    };
    Ok((
        StatusCode::CREATED,
        Json(disk_task(&state, id, task).await?),
    ))
}

/// A host a tenant may put a disk on.
async fn check_host(
    state: &AppState,
    tenant: TenantId,
    host: HostId,
) -> ApiResult<mst_db::hosts::Host> {
    let h = mst_db::hosts::get(&state.db, host).await?;
    let allowed = super::clusters::allowed_clusters(state, tenant).await?;
    if allowed.is_some_and(|a| !h.cluster_id.is_some_and(|c| a.contains(&c))) {
        return Err(ApiError::not_found("host"));
    }
    if h.state != "ready" && h.state != "maintenance" {
        return Err(ApiError::conflict(format!("{} is {}", h.name, h.state)));
    }
    Ok(h)
}

/// A connected host that reaches a shared pool.
async fn host_reaching(state: &AppState, pool: &mst_db::storage_pools::Pool) -> ApiResult<HostId> {
    mst_db::hosts::list(&state.db)
        .await?
        .into_iter()
        .find(|h| {
            h.is_live()
                && h.state == "ready"
                && h.cluster_id.is_some_and(|c| pool.clusters.contains(&c))
        })
        .map(|h| h.id)
        .ok_or_else(|| ApiError::unavailable(format!("no connected host reaches {}", pool.name)))
}

/// The host that handles the disk: its own, or one reaching its pool.
async fn host_of(state: &AppState, d: &Disk) -> ApiResult<HostId> {
    let pool = mst_db::storage_pools::get(&state.db, d.pool_id).await?;
    match (pool.kind.as_str(), d.host_id) {
        ("local", Some(h)) => Ok(h),
        ("local", None) => Err(ApiError::conflict("the disk is on no host")),
        _ => host_reaching(state, &pool).await,
    }
}

/// Removes a volume made for a disk that is not kept after all.
async fn remove_volume(
    state: &AppState,
    host: HostId,
    pool: &mst_db::storage_pools::Pool,
    id: VolumeId,
) {
    let Ok(pool) = super::agent_pool(pool) else {
        return;
    };
    let cmd = mst_proto::DeleteVolume {
        volume_id: id.to_string(),
        pool: Some(pool),
    };
    if let Err(e) = state
        .agents
        .call(host, mst_proto::command::Cmd::DeleteVolume(cmd), AGENT)
        .await
    {
        tracing::warn!(volume = %id, error = %e, "removing a disk's volume after all");
    }
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateDisk {
    name: Option<String>,
    /// Larger than it is (disks only grow), at most 16384. A disk attached
    /// to a running VM grows under it, as `/vms/{id}/disks/{index}/resize`.
    size_gib: Option<u64>,
}

/// Renames or grows a disk.
#[utoipa::path(patch, path = "/disks/{id}", tag = "disks", params(("id" = VolumeId, Path)),
    request_body = UpdateDisk,
    responses((status = 200, body = DiskView), (status = 400), (status = 404), (status = 409),
        (status = 503)))]
async fn update_disk(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VolumeId>,
    Json(req): Json<UpdateDisk>,
) -> ApiResult<Json<DiskView>> {
    let disk = load(&state, &p, id, Perm::VmWrite).await?;
    if let Some(name) = &req.name {
        let name = check_name(name)?;
        mst_db::disks::rename(&state.db, id, name).await?;
        audit(
            &state.db,
            Some(&p),
            Some(disk.tenant_id),
            "disk.rename",
            Some(("disk", id.0)),
            json!({"name": name}),
        )
        .await;
    }
    if let Some(size_gib) = req.size_gib {
        match disk.vm_id {
            Some(vm_id) => {
                let _grown = super::vms::grow_disk(
                    &state,
                    &p,
                    vm_id,
                    super::vms::Which::Volume(id),
                    size_gib,
                )
                .await?;
            }
            None => grow_detached(&state, &p, id, size_gib).await?,
        }
    }
    Ok(Json(mst_db::disks::get(&state.db, id).await?.into()))
}

/// Grows a disk attached to no VM: recorded (and counted) first, grown on
/// its host outside any transaction, and put back should that fail.
async fn grow_detached(
    state: &AppState,
    p: &Principal,
    id: VolumeId,
    size_gib: u64,
) -> ApiResult<()> {
    let mut tx = state.db.begin().await?;
    let disk = mst_db::disks::get_for_update(&mut tx, id).await?;
    let size = size_gib.saturating_mul(GIB);
    let was = disk.size_bytes as u64;
    if size_gib > MAX_GIB || size <= was {
        return Err(ApiError::bad_request(format!(
            "the disk is {} GiB: it can grow, to at most 16384 GiB",
            was.div_ceil(GIB)
        )));
    }
    if disk.vm_id.is_some() || disk.state != "ready" {
        return Err(ApiError::conflict(format!(
            "the disk is {}: grow it afterwards",
            if disk.vm_id.is_some() {
                "being attached"
            } else {
                &disk.state
            }
        )));
    }
    let delta: mst_db::quotas::Amounts =
        [("disk_gib".to_owned(), (size - was).div_ceil(GIB) as i64)]
            .into_iter()
            .collect();
    mst_db::quotas::reserve(&mut tx, disk.tenant_id, &delta).await?;
    mst_db::vms::set_volume_size(&mut *tx, id, size as i64).await?;
    mst_db::disks::set_state(&mut *tx, id, "growing", None).await?;
    tx.commit().await?;
    let grown = async {
        let host = host_of(state, &disk).await?;
        let pool = mst_db::storage_pools::get(&state.db, disk.pool_id).await?;
        let cmd = mst_proto::ResizeDisk {
            // Attached to no VM: the volume itself grows.
            vm_id: String::new(),
            disk: Some(mst_proto::BackupDisk {
                index: 0,
                volume_id: id.to_string(),
                pool: Some(super::agent_pool(&pool).map_err(ApiError::internal)?),
                target: String::new(),
                parent_index: None,
            }),
            size_bytes: size,
        };
        state
            .agents
            .call(host, mst_proto::command::Cmd::ResizeDisk(cmd), AGENT)
            .await
            .map_err(|e| ApiError::unavailable(format!("growing the disk on its host: {e}")))
    }
    .await;
    if let Err(e) = grown {
        // Not grown: its size, and the quota, go back.
        mst_db::vms::set_volume_size(&state.db, id, was as i64).await?;
        mst_db::disks::set_state(&state.db, id, "ready", None).await?;
        return Err(e);
    }
    mst_db::disks::set_state(&state.db, id, "ready", None).await?;
    audit(
        &state.db,
        Some(p),
        Some(disk.tenant_id),
        "disk.resize",
        Some(("disk", id.0)),
        json!({"name": disk.name, "size_gib": size_gib}),
    )
    .await;
    Ok(())
}

/// Deletes a disk attached to no VM, with what it holds.
#[utoipa::path(delete, path = "/disks/{id}", tag = "disks", params(("id" = VolumeId, Path)),
    responses((status = 204), (status = 404),
        (status = 409, description = "The disk is attached, or busy"), (status = 503)))]
async fn delete_disk(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VolumeId>,
) -> ApiResult<StatusCode> {
    load(&state, &p, id, Perm::VmWrite).await?;
    let mut tx = state.db.begin().await?;
    let disk = mst_db::disks::get_for_update(&mut tx, id).await?;
    if let Some(vm) = &disk.vm_name {
        return Err(ApiError::conflict(format!(
            "the disk is attached to {vm}: detach it first"
        )));
    }
    if !matches!(disk.state.as_str(), "ready" | "failed") {
        return Err(ApiError::conflict(format!(
            "the disk is {}: delete it afterwards",
            disk.state
        )));
    }
    let host = host_of(&state, &disk).await?;
    let pool = mst_db::storage_pools::get(&mut *tx, disk.pool_id).await?;
    let cmd = mst_proto::DeleteVolume {
        volume_id: id.to_string(),
        pool: Some(super::agent_pool(&pool).map_err(ApiError::internal)?),
    };
    state
        .agents
        .call(host, mst_proto::command::Cmd::DeleteVolume(cmd), AGENT)
        .await
        .map_err(|e| ApiError::unavailable(format!("deleting the disk on its host: {e}")))?;
    mst_db::vms::delete_volume(&mut *tx, id).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(disk.tenant_id),
        "disk.delete",
        Some(("disk", id.0)),
        json!({"name": disk.name}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, ToSchema)]
pub struct AttachDisk {
    disk_id: VolumeId,
}

/// Attaches a disk to a VM of its tenant (a task): plugged in if it runs
/// (where it started since disks could be), otherwise from its next start.
/// A disk on local storage attaches to VMs on its host; move it there first
/// (`/disks/{id}/move` with the VM). Not while the VM has snapshots (they
/// cover the disks it had).
#[utoipa::path(post, path = "/vms/{id}/disks", tag = "disks", params(("id" = VmId, Path)),
    request_body = AttachDisk,
    responses((status = 202, body = DiskTask), (status = 404),
        (status = 409, description = "The disk or the VM cannot be attached now")))]
async fn attach_disk(
    State(state): State<AppState>,
    p: Principal,
    Path(vm_id): Path<VmId>,
    Json(req): Json<AttachDisk>,
) -> ApiResult<(StatusCode, Json<DiskTask>)> {
    let vm = mst_db::vms::get(&state.db, vm_id).await?;
    authorize_resource(&state.db, &p, Perm::VmWrite, vm.tenant_id, "vm").await?;
    load(&state, &p, req.disk_id, Perm::VmWrite).await?;
    let mut tx = state.db.begin().await?;
    let disk = mst_db::disks::get_for_update(&mut tx, req.disk_id).await?;
    let task = begin_attach(&state, &mut tx, vm_id, &disk, Some(p.user_id)).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(vm.tenant_id),
        "vm.attach_disk",
        Some(("vm", vm_id.0)),
        json!({"name": vm.name, "disk": disk.id, "disk_name": disk.name}),
    )
    .await;
    Ok((
        StatusCode::ACCEPTED,
        Json(disk_task(&state, disk.id, Some(task)).await?),
    ))
}

/// Refuses changing the VM's disks while it is busy, or has snapshots.
async fn check_vm(tx: &mut PgConnection, vm: &mst_db::vms::Vm, what: &str) -> ApiResult<()> {
    if vm.status != "ready" {
        return Err(ApiError::conflict(format!(
            "the VM is {}: {what} afterwards",
            vm.status
        )));
    }
    if vm.power != vm.desired_power {
        return Err(ApiError::conflict(format!(
            "the VM is starting or stopping: {what} once it is done"
        )));
    }
    super::check_disks_idle(&mut *tx, vm.id).await?;
    if mst_db::snapshots::count(&mut *tx, vm.id).await? > 0 {
        return Err(ApiError::conflict(format!(
            "the VM has snapshots, which cover the disks it has: delete them to {what}"
        )));
    }
    Ok(())
}

/// Starts attaching `disk` (locked) to the VM, in `tx`: the VM's
/// definition has it from now, and a task plugs it in (`engine::disk`).
pub async fn begin_attach(
    state: &AppState,
    tx: &mut PgConnection,
    vm_id: VmId,
    disk: &Disk,
    by: Option<UserId>,
) -> ApiResult<TaskId> {
    let vm = mst_db::vms::get_for_update(&mut *tx, vm_id).await?;
    if let Some(other) = &disk.vm_name {
        return Err(ApiError::conflict(format!(
            "the disk is attached to {other}: detach it first"
        )));
    }
    if disk.state != "ready" {
        return Err(ApiError::conflict(format!("the disk is {}", disk.state)));
    }
    if disk.tenant_id != vm.tenant_id {
        return Err(ApiError::conflict(
            "a disk attaches to VMs of its own tenant",
        ));
    }
    check_vm(&mut *tx, &vm, "attach the disk").await?;
    let host = vm
        .host_id
        .ok_or_else(|| ApiError::conflict("the VM is on no host"))?;
    let mut spec = vm.spec.0.clone();
    if spec.disks.len() > MAX_DATA_DISKS {
        return Err(ApiError::conflict(format!(
            "the VM has {MAX_DATA_DISKS} disks besides its boot disk, as many as it may"
        )));
    }
    let pool = mst_db::storage_pools::get(&mut *tx, disk.pool_id).await?;
    if pool.kind == "local" {
        if disk.host_id != Some(host) {
            return Err(ApiError::conflict(format!(
                "the disk is on {}, the VM on another host: move it to the VM (it moves, then attaches)",
                disk.host_name.as_deref().unwrap_or("no host")
            )));
        }
    } else {
        let cluster = mst_db::hosts::get(&mut *tx, host).await?.cluster_id;
        if !cluster.is_some_and(|c| pool.clusters.contains(&c)) {
            return Err(ApiError::conflict(format!(
                "the VM's host does not reach {}",
                pool.name
            )));
        }
    }
    if vm.power != "stopped" && spec.disk_hotplug != Some(true) {
        return Err(ApiError::conflict(
            "the VM was started before disks could be plugged in while it runs: stop it to attach the disk (from its next start they can be)",
        ));
    }
    let unit = spec.free_unit();
    spec.disks.push(DiskSpec {
        volume_id: disk.id,
        pool_id: disk.pool_id,
        size_bytes: disk.size_bytes as u64,
        image_id: disk.image_id,
        restored_from: None,
        cloned_from: None,
        unit: Some(unit),
        extra: Default::default(),
    });
    mst_db::vms::redefine(&mut *tx, vm.id, &spec).await?;
    mst_db::disks::set_vm(&mut *tx, disk.id, Some(vm.id)).await?;
    mst_db::disks::set_state(&mut *tx, disk.id, "attaching", None).await?;
    let task = mst_db::tasks::enqueue(
        &mut *tx,
        mst_db::tasks::NewTask {
            tenant_id: vm.tenant_id,
            kind: "disk.attach",
            target: Some(("disk", disk.id.0)),
            params: json!({"vm": vm.id}),
            created_by: by,
        },
    )
    .await?;
    let _ = state;
    Ok(task.id)
}

/// Detaches a disk (not the boot disk) from a VM (a task), unplugging it if
/// the VM runs: unmount it in the guest first. The disk is kept, to attach
/// again.
#[utoipa::path(delete, path = "/vms/{id}/disks/{disk_id}", tag = "disks",
    params(("id" = VmId, Path), ("disk_id" = VolumeId, Path)),
    responses((status = 202, body = DiskTask), (status = 404),
        (status = 409, description = "The VM cannot let go of the disk now")))]
async fn detach_disk(
    State(state): State<AppState>,
    p: Principal,
    Path((vm_id, disk_id)): Path<(VmId, VolumeId)>,
) -> ApiResult<(StatusCode, Json<DiskTask>)> {
    let vm = mst_db::vms::get(&state.db, vm_id).await?;
    authorize_resource(&state.db, &p, Perm::VmWrite, vm.tenant_id, "vm").await?;
    let mut tx = state.db.begin().await?;
    let vm = mst_db::vms::get_for_update(&mut tx, vm_id).await?;
    let disk = mst_db::disks::get_for_update(&mut tx, disk_id).await?;
    if disk.vm_id != Some(vm_id) {
        return Err(ApiError::not_found("disk"));
    }
    if disk.boot {
        return Err(ApiError::conflict("the boot disk stays with its VM"));
    }
    if disk.state != "ready" {
        return Err(ApiError::conflict(format!("the disk is {}", disk.state)));
    }
    check_vm(&mut tx, &vm, "detach the disk").await?;
    if vm.power != "stopped" && vm.spec.0.disk_hotplug != Some(true) {
        return Err(ApiError::conflict(
            "the VM was started before disks could be unplugged while it runs: stop it to detach the disk",
        ));
    }
    mst_db::disks::set_state(&mut *tx, disk_id, "detaching", None).await?;
    let task = mst_db::tasks::enqueue(
        &mut *tx,
        mst_db::tasks::NewTask {
            tenant_id: vm.tenant_id,
            kind: "disk.detach",
            target: Some(("disk", disk_id.0)),
            params: json!({"vm": vm_id}),
            created_by: Some(p.user_id),
        },
    )
    .await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(vm.tenant_id),
        "vm.detach_disk",
        Some(("vm", vm_id.0)),
        json!({"name": vm.name, "disk": disk_id, "disk_name": disk.name}),
    )
    .await;
    Ok((
        StatusCode::ACCEPTED,
        Json(disk_task(&state, disk_id, Some(task.id)).await?),
    ))
}

#[derive(Deserialize, ToSchema)]
pub struct MoveDisk {
    /// The pool it goes to; its own by default (to another host).
    storage_pool_id: Option<StoragePoolId>,
    /// The host it goes to, for local storage: by default the VM's
    /// (`vm_id`).
    host_id: Option<HostId>,
    /// A VM of its tenant to attach it to once it is there.
    vm_id: Option<VmId>,
}

/// Moves a disk attached to no VM: to another host (local storage), or to
/// another pool (a task). It is copied, with what its image held (the copy
/// depends on nothing), then the old copy is removed. Given a VM, it goes
/// where the VM can reach it and is attached once there.
#[utoipa::path(post, path = "/disks/{id}/move", tag = "disks", params(("id" = VolumeId, Path)),
    request_body = MoveDisk,
    responses((status = 202, body = DiskTask), (status = 400), (status = 404),
        (status = 409, description = "The disk is attached, or busy")))]
async fn move_disk(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VolumeId>,
    Json(req): Json<MoveDisk>,
) -> ApiResult<(StatusCode, Json<DiskTask>)> {
    let disk = load(&state, &p, id, Perm::VmWrite).await?;
    let pool =
        mst_db::storage_pools::get(&state.db, req.storage_pool_id.unwrap_or(disk.pool_id)).await?;
    if !super::storage_pools::may_use(&state, disk.tenant_id, &pool).await? {
        return Err(ApiError::not_found("storage pool"));
    }
    let vm = match req.vm_id {
        Some(v) => {
            let vm = mst_db::vms::get(&state.db, v).await?;
            authorize_resource(&state.db, &p, Perm::VmWrite, vm.tenant_id, "vm").await?;
            if vm.tenant_id != disk.tenant_id {
                return Err(ApiError::bad_request(
                    "a disk attaches to VMs of its own tenant",
                ));
            }
            Some(vm)
        }
        None => None,
    };
    let host = if pool.kind == "local" {
        let host = match (req.host_id, vm.as_ref().and_then(|v| v.host_id)) {
            (Some(h), Some(v)) if h != v => {
                return Err(ApiError::bad_request(
                    "a disk on local storage attaches to VMs on its host: give the VM's, or no host",
                ));
            }
            (Some(h), _) | (None, Some(h)) => h,
            (None, None) => {
                return Err(ApiError::bad_request(
                    "a disk on local storage is on a host: give the host to move it to (host_id), or the VM to attach it to (vm_id)",
                ));
            }
        };
        let h = check_host(&state, disk.tenant_id, host).await?;
        if !h.is_live() {
            return Err(ApiError::conflict(format!("{} is not connected", h.name)));
        }
        Some(host)
    } else {
        if let Some(vm) = &vm {
            let host = vm
                .host_id
                .ok_or_else(|| ApiError::conflict("the VM is on no host"))?;
            let cluster = mst_db::hosts::get(&state.db, host).await?.cluster_id;
            if !cluster.is_some_and(|c| pool.clusters.contains(&c)) {
                return Err(ApiError::bad_request(format!(
                    "the VM's host does not reach {}",
                    pool.name
                )));
            }
        }
        None
    };
    if pool.id == disk.pool_id && (pool.kind != "local" || host == disk.host_id) {
        return Err(ApiError::bad_request("the disk is there already"));
    }
    let source = mst_db::storage_pools::get(&state.db, disk.pool_id).await?;
    if super::storage_pools::same_storage(&source, &pool) {
        return Err(ApiError::bad_request(format!(
            "{} and {} are the same storage: the disk is there already",
            source.name, pool.name
        )));
    }

    let mut tx = state.db.begin().await?;
    let disk = mst_db::disks::get_for_update(&mut tx, id).await?;
    if let Some(vm) = &disk.vm_name {
        return Err(ApiError::conflict(format!(
            "the disk is attached to {vm}: it moves with it (migrate the VM), or detach it first"
        )));
    }
    if disk.state != "ready" {
        return Err(ApiError::conflict(format!(
            "the disk is {}: move it afterwards",
            disk.state
        )));
    }
    mst_db::disks::set_state(&mut *tx, id, "moving", None).await?;
    let task = mst_db::tasks::enqueue(
        &mut *tx,
        mst_db::tasks::NewTask {
            tenant_id: disk.tenant_id,
            kind: "disk.move",
            target: Some(("disk", id.0)),
            // Where it is now too: a resumed task picks up from there.
            params: json!({"pool": pool.id, "host": host, "vm": req.vm_id,
                           "from_pool": disk.pool_id, "from_host": disk.host_id}),
            created_by: Some(p.user_id),
        },
    )
    .await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(disk.tenant_id),
        "disk.move",
        Some(("disk", id.0)),
        json!({"name": disk.name, "pool": pool.name, "host": host, "vm": req.vm_id}),
    )
    .await;
    Ok((
        StatusCode::ACCEPTED,
        Json(disk_task(&state, id, Some(task.id)).await?),
    ))
}
