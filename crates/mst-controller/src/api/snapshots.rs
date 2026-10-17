//! Snapshots of VMs' disks: points in time a stopped VM can be put back to.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use mst_core::ids::{SnapshotId, TaskId, TenantId, VmId};
use mst_db::sqlx::PgConnection;
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::authz::{Perm, Principal, audit, authorize_resource};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(vm_snapshots, create_snapshot))
        .routes(routes!(revert_snapshot))
        .routes(routes!(delete_snapshot))
}

#[derive(Serialize, ToSchema)]
pub struct SnapshotView {
    id: SnapshotId,
    tenant_id: TenantId,
    vm_id: VmId,
    name: String,
    /// `creating`, `ready`, `reverting`, `deleting` or `failed`.
    state: String,
    /// Whether its file systems were consistent: the VM was stopped, or its
    /// guest agent (qemu-guest-agent) froze them. Otherwise it is as after a
    /// crash, which journaling file systems recover from.
    consistent: Option<bool>,
    /// Why it failed to be taken, or why the last revert or deletion did.
    error: Option<String>,
    created_at: DateTime<Utc>,
}

impl From<mst_db::snapshots::Snapshot> for SnapshotView {
    fn from(s: mst_db::snapshots::Snapshot) -> Self {
        Self {
            id: s.id,
            tenant_id: s.tenant_id,
            vm_id: s.vm_id,
            name: s.name,
            state: s.state,
            consistent: s.consistent,
            error: s.error,
            created_at: s.created_at,
        }
    }
}

/// Refuses work on a VM's disks while a backup, a clone, another snapshot
/// operation, or attaching or detaching one of them has them.
pub async fn check_idle(db: &mut PgConnection, vm: VmId) -> ApiResult<()> {
    if let Some(what) = mst_db::snapshots::busy(&mut *db, vm).await? {
        return Err(ApiError::conflict(format!(
            "a snapshot of the VM is being {}",
            match what.as_str() {
                "creating" => "taken",
                "reverting" => "reverted to",
                _ => "deleted",
            }
        )));
    }
    if mst_db::backups::running_for(&mut *db, vm).await? {
        return Err(ApiError::conflict("a backup of the VM is being taken"));
    }
    if mst_db::vms::being_cloned(&mut *db, vm).await? {
        return Err(ApiError::conflict("a clone of the VM is being made"));
    }
    if let Some(what) = mst_db::disks::busy_on_vm(&mut *db, vm).await? {
        return Err(ApiError::conflict(format!(
            "a disk of the VM is being {}",
            match what.as_str() {
                "attaching" => "attached",
                "detaching" => "detached",
                _ => "grown",
            }
        )));
    }
    Ok(())
}

/// A VM's snapshots, oldest first.
#[utoipa::path(get, path = "/vms/{id}/snapshots", tag = "snapshots", params(("id" = VmId, Path)),
    responses((status = 200, body = Vec<SnapshotView>), (status = 404)))]
async fn vm_snapshots(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
) -> ApiResult<Json<Vec<SnapshotView>>> {
    let vm = mst_db::vms::get(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::VmRead, vm.tenant_id, "vm").await?;
    Ok(Json(
        mst_db::snapshots::of_vm(&state.db, vm.id)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

#[derive(Deserialize, ToSchema, Default)]
pub struct CreateSnapshot {
    /// Unique among the VM's snapshots; the date and time (UTC) by default.
    name: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct SnapshotAccepted {
    task_id: TaskId,
    #[serde(flatten)]
    snapshot: SnapshotView,
}

/// Takes a snapshot of a VM's disks, running or stopped. A running VM's
/// disks are all taken at the same moment, with its file systems frozen
/// then if its guest agent answers.
#[utoipa::path(post, path = "/vms/{id}/snapshots", tag = "snapshots", params(("id" = VmId, Path)),
    request_body = CreateSnapshot,
    responses((status = 202, body = SnapshotAccepted), (status = 400), (status = 404),
        (status = 409, description = "The VM is busy, or has as many snapshots as it may")))]
async fn create_snapshot(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
    body: Option<Json<CreateSnapshot>>,
) -> ApiResult<(StatusCode, Json<SnapshotAccepted>)> {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let name = match req.name.as_deref().map(str::trim) {
        None | Some("") => default_name(&state, id).await?,
        Some(n) if n.chars().count() > 64 || n.chars().any(char::is_control) => {
            return Err(ApiError::bad_request(
                "a snapshot's name is at most 64 characters, on one line",
            ));
        }
        Some(n) => n.to_owned(),
    };
    let mut tx = state.db.begin().await?;
    let vm = mst_db::vms::get_for_update(&mut tx, id).await?;
    authorize_resource(&state.db, &p, Perm::VmWrite, vm.tenant_id, "vm").await?;
    if vm.status != "ready" {
        return Err(ApiError::conflict(format!(
            "the VM is {}: take a snapshot afterwards",
            vm.status
        )));
    }
    if vm.host_id.is_none() {
        return Err(ApiError::conflict("the VM is not placed on a host"));
    }
    // Starting or shutting down: a snapshot taken now would race it.
    if vm.power != vm.desired_power {
        return Err(ApiError::conflict(format!(
            "the VM is {}: take a snapshot once it is",
            if vm.desired_power == "running" {
                "starting"
            } else {
                "stopping"
            }
        )));
    }
    check_idle(&mut tx, vm.id).await?;
    if mst_db::snapshots::count(&mut *tx, vm.id).await? >= mst_db::snapshots::MAX_PER_VM {
        return Err(ApiError::conflict(format!(
            "a VM may have {} snapshots; delete one first",
            mst_db::snapshots::MAX_PER_VM
        )));
    }
    // A snapshot keeps every block written after it: charged the disks'
    // size, which is as much as it can grow to.
    let bytes: i64 = mst_db::vms::volumes_for_vm(&mut *tx, vm.id)
        .await?
        .iter()
        .map(|v| v.size_bytes)
        .sum();
    let delta = [("snapshot_gib".to_owned(), (bytes + (1 << 30) - 1) >> 30)]
        .into_iter()
        .collect();
    mst_db::quotas::reserve(&mut tx, vm.tenant_id, &delta).await?;
    let sid = mst_db::snapshots::create(&mut *tx, vm.tenant_id, vm.id, &name, &vm.spec.0).await?;
    let task = enqueue(&mut tx, &p, vm.tenant_id, "snapshot.create", sid).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(vm.tenant_id),
        "snapshot.create",
        Some(("vm", vm.id.0)),
        json!({"snapshot": sid, "name": name}),
    )
    .await;
    let snapshot = mst_db::snapshots::get(&state.db, sid).await?.into();
    Ok((
        StatusCode::ACCEPTED,
        Json(SnapshotAccepted {
            task_id: task,
            snapshot,
        }),
    ))
}

/// The date and time, to the minute, or to the second if a snapshot was
/// taken this minute already.
async fn default_name(state: &AppState, vm: VmId) -> ApiResult<String> {
    let now = Utc::now();
    let short = now.format("%Y-%m-%d %H:%M UTC").to_string();
    let taken = mst_db::snapshots::of_vm(&state.db, vm)
        .await?
        .iter()
        .any(|s| s.name == short);
    Ok(if taken {
        now.format("%Y-%m-%d %H:%M:%S UTC").to_string()
    } else {
        short
    })
}

async fn enqueue(
    tx: &mut PgConnection,
    p: &Principal,
    tenant: TenantId,
    kind: &'static str,
    id: SnapshotId,
) -> ApiResult<TaskId> {
    Ok(mst_db::tasks::enqueue(
        &mut *tx,
        mst_db::tasks::NewTask {
            tenant_id: tenant,
            kind,
            target: Some(("snapshot", id.0)),
            params: json!({}),
            created_by: Some(p.user_id),
        },
    )
    .await?
    .id)
}

#[derive(Serialize, ToSchema)]
pub struct SnapshotTask {
    task_id: TaskId,
}

/// Locks a snapshot's VM for a change to the snapshot; the snapshot must be
/// ready and the VM's disks idle.
async fn lock(
    tx: &mut PgConnection,
    state: &AppState,
    p: &Principal,
    id: SnapshotId,
) -> ApiResult<(mst_db::snapshots::Snapshot, mst_db::vms::Vm)> {
    let snap = mst_db::snapshots::get(&state.db, id).await?;
    authorize_resource(&state.db, p, Perm::VmWrite, snap.tenant_id, "snapshot").await?;
    let vm = mst_db::vms::get_for_update(&mut *tx, snap.vm_id).await?;
    match vm.status.as_str() {
        "ready" | "error" => {}
        other => {
            return Err(ApiError::conflict(format!(
                "the VM is {other}: try again afterwards"
            )));
        }
    }
    check_idle(&mut *tx, vm.id).await?;
    let snap = mst_db::snapshots::get(&mut *tx, id).await?;
    Ok((snap, vm))
}

/// Puts a stopped VM's disks back as they were in a snapshot. What was
/// written since is lost; the VM's other snapshots stay.
#[utoipa::path(post, path = "/snapshots/{id}/revert", tag = "snapshots", params(("id" = SnapshotId, Path)),
    responses((status = 202, body = SnapshotTask), (status = 404),
        (status = 409, description = "The VM is running or busy, or the snapshot is not ready")))]
async fn revert_snapshot(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<SnapshotId>,
) -> ApiResult<(StatusCode, Json<SnapshotTask>)> {
    let mut tx = state.db.begin().await?;
    let (snap, vm) = lock(&mut tx, &state, &p, id).await?;
    if snap.state != "ready" {
        return Err(ApiError::conflict(format!(
            "the snapshot is {}",
            snap.state
        )));
    }
    let busy = mst_db::tasks::active_by_target(&mut *tx)
        .await?
        .into_iter()
        .any(|(t, _)| t == vm.id.0);
    if vm.status != "ready" || busy || vm.power != "stopped" || vm.desired_power != "stopped" {
        return Err(ApiError::conflict(format!(
            "stop {} first: only a stopped VM can be reverted",
            vm.name
        )));
    }
    mst_db::snapshots::set_state(&mut *tx, id, "reverting", None, None).await?;
    let task = enqueue(&mut tx, &p, snap.tenant_id, "snapshot.revert", id).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(snap.tenant_id),
        "snapshot.revert",
        Some(("vm", vm.id.0)),
        json!({"snapshot": id, "name": snap.name}),
    )
    .await;
    Ok((StatusCode::ACCEPTED, Json(SnapshotTask { task_id: task })))
}

/// Deletes a snapshot, keeping the VM's disks as they are.
#[utoipa::path(delete, path = "/snapshots/{id}", tag = "snapshots", params(("id" = SnapshotId, Path)),
    responses((status = 202, body = SnapshotTask), (status = 404),
        (status = 409, description = "The VM is busy")))]
async fn delete_snapshot(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<SnapshotId>,
) -> ApiResult<(StatusCode, Json<SnapshotTask>)> {
    let mut tx = state.db.begin().await?;
    let (snap, vm) = lock(&mut tx, &state, &p, id).await?;
    if vm.power != vm.desired_power {
        return Err(ApiError::conflict(format!(
            "{} is starting or stopping: try again once it is done",
            vm.name
        )));
    }
    mst_db::snapshots::set_state(&mut *tx, id, "deleting", None, None).await?;
    let task = enqueue(&mut tx, &p, snap.tenant_id, "snapshot.delete", id).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(snap.tenant_id),
        "snapshot.delete",
        Some(("vm", vm.id.0)),
        json!({"snapshot": id, "name": snap.name}),
    )
    .await;
    Ok((StatusCode::ACCEPTED, Json(SnapshotTask { task_id: task })))
}
