//! Backups of VMs, and the repositories that keep them.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use mst_core::ids::{BackupId, BackupRepositoryId, TaskId, TenantId, UserId, VmId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::authz::{Perm, Principal, audit, authorize, authorize_resource};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_repositories, create_repository))
        .routes(routes!(delete_repository))
        .routes(routes!(usable_repositories))
        .routes(routes!(vm_backups, create_backup))
        .routes(routes!(set_schedule))
        .routes(routes!(list_backups))
        .routes(routes!(get_backup, delete_backup))
}

/// How a repository is described to agents: a shared pool they mount.
pub fn repository_pool(r: &mst_db::backups::Repository) -> mst_proto::Pool {
    mst_proto::Pool {
        id: r.id.to_string(),
        kind: r.kind.clone(),
        config_json: r.config.to_string(),
    }
}

async fn authorize_infra(state: &AppState, p: &Principal, perm: Perm) -> ApiResult<()> {
    let root = mst_db::tenants::root(&state.db)
        .await?
        .ok_or_else(|| ApiError::not_found("tenant"))?;
    authorize(&state.db, p, perm, root.id).await
}

#[derive(Serialize, ToSchema)]
pub struct RepositoryView {
    id: BackupRepositoryId,
    name: String,
    /// `nfs`.
    kind: String,
    server: String,
    export: String,
    created_at: DateTime<Utc>,
}

impl From<mst_db::backups::Repository> for RepositoryView {
    fn from(r: mst_db::backups::Repository) -> Self {
        let field = |k: &str| {
            r.config
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        };
        Self {
            server: field("server"),
            export: field("export"),
            id: r.id,
            name: r.name,
            kind: r.kind,
            created_at: r.created_at,
        }
    }
}

/// Every backup repository.
#[utoipa::path(get, path = "/backup-repositories", tag = "backups",
    responses((status = 200, body = Vec<RepositoryView>), (status = 403)))]
async fn list_repositories(
    State(state): State<AppState>,
    p: Principal,
) -> ApiResult<Json<Vec<RepositoryView>>> {
    authorize_infra(&state, &p, Perm::InfraRead).await?;
    Ok(Json(
        mst_db::backups::list_repositories(&state.db)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

#[derive(Deserialize, ToSchema)]
pub struct CreateRepository {
    name: String,
    /// The NFS server, and the export backups go in. Hosts mount it, and
    /// QEMU writes the backups as root: export it with `no_root_squash`.
    server: String,
    export: String,
    /// Mount options (default `vers=4.2,hard,noatime`).
    options: Option<String>,
}

/// Adds a backup repository: an NFS export the hosts mount, apart from the
/// storage VMs run on, so backups survive losing that.
#[utoipa::path(post, path = "/backup-repositories", tag = "backups", request_body = CreateRepository,
    responses((status = 201, body = RepositoryView), (status = 400), (status = 403), (status = 409)))]
async fn create_repository(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<CreateRepository>,
) -> ApiResult<(StatusCode, Json<RepositoryView>)> {
    authorize_infra(&state, &p, Perm::InfraWrite).await?;
    let name = req.name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(ApiError::bad_request("the name must be 1-64 characters"));
    }
    if req.server.trim().is_empty() || !req.export.starts_with('/') {
        return Err(ApiError::bad_request(
            "a repository needs an NFS server and an absolute export path",
        ));
    }
    let mut config =
        json!({"server": req.server.trim(), "export": req.export.trim_end_matches('/')});
    if let Some(o) = req
        .options
        .as_deref()
        .map(str::trim)
        .filter(|o| !o.is_empty())
    {
        config["options"] = o.into();
    }
    let id = mst_db::backups::create_repository(&state.db, name, "nfs", &config).await?;
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "backup_repository.create",
        Some(("backup_repository", id.0)),
        json!({"name": name, "config": config}),
    )
    .await;
    let r = mst_db::backups::get_repository(&state.db, id).await?;
    Ok((StatusCode::CREATED, Json(r.into())))
}

/// Removes a repository that holds no backups. Hosts unmount it; the files
/// on the server stay. Schedules that used it stop.
#[utoipa::path(delete, path = "/backup-repositories/{id}", tag = "backups", params(("id" = BackupRepositoryId, Path)),
    responses((status = 204), (status = 403), (status = 404), (status = 409, description = "Backups are left")))]
async fn delete_repository(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<BackupRepositoryId>,
) -> ApiResult<StatusCode> {
    authorize_infra(&state, &p, Perm::InfraWrite).await?;
    let repo = mst_db::backups::get_repository(&state.db, id).await?;
    let mut tx = state.db.begin().await?;
    if !mst_db::backups::delete_repository(&mut tx, id).await? {
        return Err(ApiError::conflict(
            "the repository still holds backups; delete them first",
        ));
    }
    tx.commit().await?;
    // Hosts let go of it (an unmount); nothing on the server is touched.
    let pool = repository_pool(&repo);
    for host in mst_db::hosts::list(&state.db).await? {
        if host.connected_to.is_none() {
            continue;
        }
        let cmd = mst_proto::command::Cmd::RemovePool(mst_proto::RemovePool {
            pool: Some(pool.clone()),
            remove_data: false,
        });
        let agents = state.agents.clone();
        tokio::spawn(async move {
            if let Err(e) = agents
                .call(host.id, cmd, std::time::Duration::from_secs(60))
                .await
            {
                tracing::warn!(host = %host.name, error = %e, "unmounting a backup repository");
            }
        });
    }
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "backup_repository.delete",
        Some(("backup_repository", id.0)),
        json!({"name": repo.name}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize, ToSchema)]
pub struct UsableRepository {
    id: BackupRepositoryId,
    name: String,
}

/// The repositories a tenant may keep backups in.
#[utoipa::path(get, path = "/tenants/{id}/backup-repositories", tag = "tenants", params(("id" = TenantId, Path)),
    responses((status = 200, body = Vec<UsableRepository>), (status = 404)))]
async fn usable_repositories(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TenantId>,
) -> ApiResult<Json<Vec<UsableRepository>>> {
    authorize_resource(&state.db, &p, Perm::VmRead, id, "tenant").await?;
    Ok(Json(
        usable(&state, id)
            .await?
            .into_iter()
            .map(|r| UsableRepository {
                id: r.id,
                name: r.name,
            })
            .collect(),
    ))
}

async fn usable(state: &AppState, tenant: TenantId) -> ApiResult<Vec<mst_db::backups::Repository>> {
    let mut out = vec![];
    for r in mst_db::backups::list_repositories(&state.db).await? {
        if mst_db::grants::may_use(&state.db, tenant, "backup_repository", r.id.0).await? {
            out.push(r);
        }
    }
    Ok(out)
}

/// The repository to use: the one asked for, or the tenant's only one.
async fn repository_for(
    state: &AppState,
    tenant: TenantId,
    asked: Option<BackupRepositoryId>,
) -> ApiResult<mst_db::backups::Repository> {
    let usable = usable(state, tenant).await?;
    match asked {
        Some(id) => usable
            .into_iter()
            .find(|r| r.id == id)
            .ok_or_else(|| ApiError::not_found("backup repository")),
        None => match <[_; 1]>::try_from(usable) {
            Ok([only]) => Ok(only),
            Err(v) if v.is_empty() => Err(ApiError::bad_request(
                "no backup repository is available to this tenant",
            )),
            Err(_) => Err(ApiError::bad_request("choose a backup repository")),
        },
    }
}

#[derive(Serialize, ToSchema)]
pub struct BackupView {
    id: BackupId,
    tenant_id: TenantId,
    /// The VM it was taken of, while that exists.
    vm_id: Option<VmId>,
    vm_name: String,
    repository_id: BackupRepositoryId,
    /// `running`, `ready` or `failed`.
    state: String,
    /// Taken by the VM's schedule (which also deletes it in time).
    scheduled: bool,
    vcpus: u32,
    memory_mib: u64,
    /// Each disk's size.
    disks: Vec<u64>,
    /// What it counts against quotas: its disks' sizes added up.
    size_bytes: i64,
    /// What it takes up in the repository.
    stored_bytes: Option<i64>,
    /// Whether its file systems were consistent: the VM was stopped, or its
    /// guest agent (qemu-guest-agent) froze them. Otherwise it is as after a
    /// crash, which journaling file systems recover from.
    consistent: Option<bool>,
    error: Option<String>,
    created_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
}

impl From<mst_db::backups::Backup> for BackupView {
    fn from(b: mst_db::backups::Backup) -> Self {
        let spec = &b.spec.0;
        Self {
            vcpus: spec.vcpus,
            memory_mib: spec.memory_mib,
            disks: spec.disks.iter().map(|d| d.size_bytes).collect(),
            id: b.id,
            tenant_id: b.tenant_id,
            vm_id: b.vm_id,
            vm_name: b.vm_name,
            repository_id: b.repository_id,
            state: b.state,
            scheduled: b.scheduled,
            size_bytes: b.size_bytes,
            stored_bytes: b.stored_bytes,
            consistent: b.consistent,
            error: b.error,
            created_at: b.created_at,
            completed_at: b.completed_at,
        }
    }
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct ScheduleView {
    /// `daily` or `weekly`.
    every: String,
    /// How many of its scheduled backups to keep; older ones are deleted.
    keep: u32,
    repository_id: BackupRepositoryId,
}

#[derive(Serialize, ToSchema)]
pub struct VmBackups {
    schedule: Option<ScheduleView>,
    /// Newest first.
    backups: Vec<BackupView>,
}

async fn load_vm(
    state: &AppState,
    p: &Principal,
    id: VmId,
    perm: Perm,
) -> ApiResult<mst_db::vms::Vm> {
    let vm = mst_db::vms::get(&state.db, id).await?;
    authorize_resource(&state.db, p, perm, vm.tenant_id, "vm").await?;
    Ok(vm)
}

/// A VM's backups and backup schedule.
#[utoipa::path(get, path = "/vms/{id}/backups", tag = "backups", params(("id" = VmId, Path)),
    responses((status = 200, body = VmBackups), (status = 404)))]
async fn vm_backups(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
) -> ApiResult<Json<VmBackups>> {
    let vm = load_vm(&state, &p, id, Perm::VmRead).await?;
    let schedule = mst_db::backups::schedule_of(&state.db, vm.id)
        .await?
        .map(|s| ScheduleView {
            every: s.every,
            keep: s.keep as u32,
            repository_id: s.repository_id,
        });
    let backups = mst_db::backups::of_vm(&state.db, vm.id)
        .await?
        .into_iter()
        .map(Into::into)
        .collect();
    Ok(Json(VmBackups { schedule, backups }))
}

#[derive(Deserialize, ToSchema, Default)]
pub struct CreateBackup {
    /// Where to keep it; the tenant's only repository by default.
    repository_id: Option<BackupRepositoryId>,
}

#[derive(Serialize, ToSchema)]
pub struct BackupAccepted {
    task_id: TaskId,
    #[serde(flatten)]
    backup: BackupView,
}

/// Starts a backup of a VM, running or stopped. A running VM's backup is of
/// the moment it starts, with its file systems frozen then if its guest
/// agent answers; the VM carries on meanwhile.
#[utoipa::path(post, path = "/vms/{id}/backups", tag = "backups", params(("id" = VmId, Path)),
    request_body = CreateBackup,
    responses((status = 202, body = BackupAccepted), (status = 400), (status = 404),
        (status = 409, description = "The VM is busy, or over quota")))]
async fn create_backup(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
    body: Option<Json<CreateBackup>>,
) -> ApiResult<(StatusCode, Json<BackupAccepted>)> {
    let vm = load_vm(&state, &p, id, Perm::VmWrite).await?;
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let repo = repository_for(&state, vm.tenant_id, req.repository_id).await?;
    let (task_id, backup) = start(&state, Some(p.user_id), &vm, &repo, false).await?;
    audit(
        &state.db,
        Some(&p),
        Some(vm.tenant_id),
        "backup.create",
        Some(("vm", vm.id.0)),
        json!({"backup": backup.id, "repository": repo.name}),
    )
    .await;
    Ok((
        StatusCode::ACCEPTED,
        Json(BackupAccepted {
            task_id,
            backup: backup.into(),
        }),
    ))
}

/// Records a backup of `vm` and queues the task that takes it. Shared by
/// the API and the schedule.
pub async fn start(
    state: &AppState,
    created_by: Option<UserId>,
    vm: &mst_db::vms::Vm,
    repo: &mst_db::backups::Repository,
    scheduled: bool,
) -> ApiResult<(TaskId, mst_db::backups::Backup)> {
    match vm.status.as_str() {
        "creating" | "deleting" | "migrating" | "error" => {
            return Err(ApiError::conflict(format!(
                "the VM is {}: back it up afterwards",
                vm.status
            )));
        }
        _ => {}
    }
    if vm.host_id.is_none() {
        return Err(ApiError::conflict("the VM is not placed on a host"));
    }
    // Starting or shutting down: a backup taken now would race it.
    if vm.power != vm.desired_power {
        return Err(ApiError::conflict(format!(
            "the VM is {}: back it up once it is",
            if vm.desired_power == "running" {
                "starting"
            } else {
                "stopping"
            }
        )));
    }
    let mut tx = state.db.begin().await?;
    // Locked: its disks stay as recorded here until the task has them (an
    // attach or detach waits, or is seen as busy below).
    let vm = &mst_db::vms::get_for_update(&mut tx, vm.id).await?;
    let spec = &vm.spec.0;
    let size: u64 = spec.disks.iter().map(|d| d.size_bytes).sum();
    // One at a time per VM, and not while a snapshot (or a disk) is worked on.
    super::snapshots::check_idle(&mut tx, vm.id).await?;
    let delta = [("backup_gib".to_owned(), size.div_ceil(1 << 30) as i64)]
        .into_iter()
        .collect();
    mst_db::quotas::reserve(&mut tx, vm.tenant_id, &delta).await?;
    let id = mst_db::backups::create(
        &mut *tx,
        vm.tenant_id,
        vm.id,
        &vm.name,
        repo.id,
        scheduled,
        spec,
        size as i64,
    )
    .await?;
    let task = mst_db::tasks::enqueue(
        &mut *tx,
        mst_db::tasks::NewTask {
            tenant_id: vm.tenant_id,
            kind: "backup.create",
            target: Some(("backup", id.0)),
            params: json!({}),
            created_by,
        },
    )
    .await?;
    tx.commit().await?;
    Ok((task.id, mst_db::backups::get(&state.db, id).await?))
}

#[derive(Deserialize, ToSchema)]
pub struct SetSchedule {
    /// `daily` or `weekly`; null stops scheduled backups (those taken stay).
    every: Option<String>,
    /// How many scheduled backups to keep (default 7).
    keep: Option<u32>,
    /// Where to keep them; the tenant's only repository by default.
    repository_id: Option<BackupRepositoryId>,
}

/// Sets a VM's backup schedule. The first scheduled backup follows within a
/// minute or so; each later one when the last is a day (or a week) old.
/// Beyond `keep`, the oldest scheduled backups are deleted; backups taken
/// by hand are kept until deleted.
#[utoipa::path(put, path = "/vms/{id}/backup-schedule", tag = "backups", params(("id" = VmId, Path)),
    request_body = SetSchedule,
    responses((status = 200, body = VmBackups), (status = 400), (status = 404)))]
async fn set_schedule(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
    Json(req): Json<SetSchedule>,
) -> ApiResult<Json<VmBackups>> {
    let vm = load_vm(&state, &p, id, Perm::VmWrite).await?;
    let schedule = match req.every.as_deref() {
        None => None,
        Some(every @ ("daily" | "weekly")) => {
            let keep = req.keep.unwrap_or(7);
            if !(1..=365).contains(&keep) {
                return Err(ApiError::bad_request("keep must be 1-365"));
            }
            let repo = repository_for(&state, vm.tenant_id, req.repository_id).await?;
            Some((every, keep as i32, repo.id))
        }
        Some(_) => return Err(ApiError::bad_request("every must be daily or weekly")),
    };
    mst_db::backups::set_schedule(&state.db, vm.id, schedule).await?;
    audit(
        &state.db,
        Some(&p),
        Some(vm.tenant_id),
        "vm.backup_schedule",
        Some(("vm", vm.id.0)),
        json!({"every": req.every, "keep": req.keep, "repository": schedule.map(|s| s.2)}),
    )
    .await;
    vm_backups(State(state), p, Path(id)).await
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Backups of this tenant and its sub-tenants; every tenant the caller
    /// may see by default.
    tenant_id: Option<TenantId>,
}

/// Backups, newest first, including those of VMs that are gone.
#[utoipa::path(get, path = "/backups", tag = "backups", params(ListQuery),
    responses((status = 200, body = Vec<BackupView>)))]
async fn list_backups(
    State(state): State<AppState>,
    p: Principal,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<BackupView>>> {
    let roots = match q.tenant_id {
        Some(t) => {
            authorize_resource(&state.db, &p, Perm::VmRead, t, "tenant").await?;
            vec![t]
        }
        None => p.scopes(Perm::VmRead),
    };
    Ok(Json(
        mst_db::backups::in_subtrees(&state.db, &roots)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

#[utoipa::path(get, path = "/backups/{id}", tag = "backups", params(("id" = BackupId, Path)),
    responses((status = 200, body = BackupView), (status = 404)))]
async fn get_backup(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<BackupId>,
) -> ApiResult<Json<BackupView>> {
    let b = mst_db::backups::get(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::VmRead, b.tenant_id, "backup").await?;
    Ok(Json(b.into()))
}

#[derive(Serialize, ToSchema)]
pub struct DeleteAccepted {
    task_id: TaskId,
}

/// Deletes a backup and its files.
#[utoipa::path(delete, path = "/backups/{id}", tag = "backups", params(("id" = BackupId, Path)),
    responses((status = 202, body = DeleteAccepted), (status = 404),
        (status = 409, description = "It is being taken, or restored")))]
async fn delete_backup(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<BackupId>,
) -> ApiResult<(StatusCode, Json<DeleteAccepted>)> {
    let b = mst_db::backups::get(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::VmWrite, b.tenant_id, "backup").await?;
    let task = queue_delete(&state, &b, Some(p.user_id)).await?;
    audit(
        &state.db,
        Some(&p),
        Some(b.tenant_id),
        "backup.delete",
        Some(("backup", b.id.0)),
        json!({"vm_name": b.vm_name, "created_at": b.created_at}),
    )
    .await;
    Ok((StatusCode::ACCEPTED, Json(DeleteAccepted { task_id: task })))
}

/// Queues a backup's deletion. Shared by the API and the schedule.
pub async fn queue_delete(
    state: &AppState,
    b: &mst_db::backups::Backup,
    created_by: Option<UserId>,
) -> ApiResult<TaskId> {
    if b.state == "running" {
        return Err(ApiError::conflict("the backup is being taken"));
    }
    if mst_db::backups::being_restored(&state.db, b.id).await? {
        return Err(ApiError::conflict("a VM is being restored from the backup"));
    }
    Ok(mst_db::tasks::enqueue(
        &state.db,
        mst_db::tasks::NewTask {
            tenant_id: b.tenant_id,
            kind: "backup.delete",
            target: Some(("backup", b.id.0)),
            params: json!({}),
            created_by,
        },
    )
    .await?
    .id)
}
