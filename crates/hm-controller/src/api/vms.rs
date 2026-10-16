use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use hm_core::ids::{HostId, ImageId, NicId, TaskId, TenantId, VmId, VolumeId};
use hm_core::vm::{CloudInit, DiskSpec, Firmware, NicSpec, VmSpec, mac_for};
use hm_db::vms::Vm;
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::authz::{Perm, Principal, authorize, authorize_resource};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_vms, create_vm))
        .routes(routes!(get_vm, delete_vm))
        .routes(routes!(start_vm))
        .routes(routes!(stop_vm))
        .routes(routes!(reboot_vm))
}

const GIB: u64 = 1 << 30;

#[derive(Serialize, ToSchema)]
pub struct VmView {
    id: VmId,
    tenant_id: TenantId,
    name: String,
    host_id: Option<HostId>,
    host_name: Option<String>,
    /// Lifecycle: `creating`, `ready`, `error` or `deleting`.
    status: String,
    /// What the user asked for: `running` or `stopped`.
    desired_power: String,
    /// What the host reports: `running`, `stopped`, `paused`, `crashed`,
    /// `shutting_down`, `missing` or `unknown`.
    power: String,
    error: Option<String>,
    vcpus: u32,
    memory_mib: u64,
    firmware: Firmware,
    disks: Vec<DiskSpec>,
    nics: Vec<NicSpec>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn view(vm: Vm, hosts: &HashMap<HostId, String>) -> VmView {
    let spec = vm.spec.0;
    VmView {
        id: vm.id,
        tenant_id: vm.tenant_id,
        name: vm.name,
        host_name: vm.host_id.and_then(|h| hosts.get(&h).cloned()),
        host_id: vm.host_id,
        status: vm.status,
        desired_power: vm.desired_power,
        power: vm.power,
        error: vm.error,
        vcpus: spec.vcpus,
        memory_mib: spec.memory_mib,
        firmware: spec.firmware,
        disks: spec.disks,
        nics: spec.nics,
        created_at: vm.created_at,
        updated_at: vm.updated_at,
    }
}

async fn host_names(state: &AppState) -> ApiResult<HashMap<HostId, String>> {
    Ok(hm_db::hosts::list(&state.db)
        .await?
        .into_iter()
        .map(|h| (h.id, h.name))
        .collect())
}

/// Loads a VM the principal may act on with `perm`.
async fn load(state: &AppState, p: &Principal, id: VmId, perm: Perm) -> ApiResult<Vm> {
    let vm = hm_db::vms::get(&state.db, id).await?;
    authorize_resource(&state.db, p, perm, vm.tenant_id, "vm").await?;
    Ok(vm)
}

#[derive(Deserialize, IntoParams)]
pub struct ListQuery {
    /// Defaults to the caller's tenant.
    tenant_id: Option<TenantId>,
    /// Include VMs of all descendant tenants.
    #[serde(default)]
    descendants: bool,
}

#[utoipa::path(get, path = "/vms", tag = "vms", params(ListQuery),
    responses((status = 200, body = Vec<VmView>)))]
async fn list_vms(
    State(state): State<AppState>,
    p: Principal,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<VmView>>> {
    let tenant = q.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::VmRead, tenant).await?;
    let hosts = host_names(&state).await?;
    let vms = hm_db::vms::list(&state.db, tenant, q.descendants).await?;
    Ok(Json(vms.into_iter().map(|v| view(v, &hosts)).collect()))
}

#[utoipa::path(get, path = "/vms/{id}", tag = "vms", params(("id" = VmId, Path)),
    responses((status = 200, body = VmView), (status = 404)))]
async fn get_vm(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
) -> ApiResult<Json<VmView>> {
    let vm = load(&state, &p, id, Perm::VmRead).await?;
    Ok(Json(view(vm, &host_names(&state).await?)))
}

#[derive(Deserialize, ToSchema)]
pub struct CreateVm {
    /// Owning tenant; defaults to the caller's.
    tenant_id: Option<TenantId>,
    /// DNS-style label, unique within the tenant.
    name: String,
    vcpus: u32,
    memory_mib: u64,
    /// Size of the boot disk. Must be at least the image's size.
    disk_gib: u64,
    image_id: ImageId,
    #[serde(default)]
    firmware: Firmware,
    /// SSH public keys for the image's default user.
    #[serde(default)]
    ssh_authorized_keys: Vec<String>,
    /// Console password for the image's default user. Handy for the serial
    /// console; prefer SSH keys otherwise.
    password: Option<String>,
    /// Complete cloud-init user data. Overrides `ssh_authorized_keys` and
    /// `password`.
    user_data: Option<String>,
    /// Start the VM once created (default true).
    start: Option<bool>,
}

#[derive(Serialize, ToSchema)]
pub struct Accepted<T: ToSchema> {
    task_id: TaskId,
    #[serde(flatten)]
    item: T,
}

#[derive(Serialize, ToSchema)]
pub struct TaskRef {
    task_id: TaskId,
}

fn validate(req: &CreateVm) -> ApiResult<()> {
    let name_ok = !req.name.is_empty()
        && req.name.len() <= 63
        && req
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !req.name.starts_with('-')
        && !req.name.ends_with('-');
    if !name_ok {
        return Err(ApiError::bad_request(
            "name must be 1-63 letters, digits or hyphens, not starting or ending with a hyphen",
        ));
    }
    if !(1..=128).contains(&req.vcpus) {
        return Err(ApiError::bad_request("vcpus must be between 1 and 128"));
    }
    if !(128..=4 * 1024 * 1024).contains(&req.memory_mib) {
        return Err(ApiError::bad_request(
            "memory_mib must be between 128 and 4194304",
        ));
    }
    if !(1..=16384).contains(&req.disk_gib) {
        return Err(ApiError::bad_request(
            "disk_gib must be between 1 and 16384",
        ));
    }
    Ok(())
}

/// Builds `#cloud-config` user data from the convenience fields.
fn user_data(req: &CreateVm) -> String {
    let mut doc = serde_json::Map::new();
    if !req.ssh_authorized_keys.is_empty() {
        doc.insert("ssh_authorized_keys".into(), json!(req.ssh_authorized_keys));
    }
    if let Some(pw) = &req.password {
        doc.insert("password".into(), json!(pw));
        doc.insert("chpasswd".into(), json!({"expire": false}));
    }
    // JSON is valid YAML, so this needs no YAML serializer.
    format!("#cloud-config\n{}\n", serde_json::Value::Object(doc))
}

#[utoipa::path(post, path = "/vms", tag = "vms", request_body = CreateVm,
    responses((status = 202, body = Accepted<VmView>), (status = 400), (status = 409), (status = 503)))]
async fn create_vm(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<CreateVm>,
) -> ApiResult<(StatusCode, Json<Accepted<VmView>>)> {
    validate(&req)?;
    let tenant = req.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::VmWrite, tenant).await?;
    if !hm_db::images::is_usable_by(&state.db, req.image_id, tenant).await? {
        return Err(ApiError::not_found("image"));
    }

    let mut tx = state.db.begin().await?;
    let host = hm_db::hosts::pick_for(&mut *tx, req.memory_mib as i64)
        .await?
        .ok_or_else(|| ApiError::unavailable("no connected host has enough free memory"))?;
    let network = hm_db::networks::default_for(&mut tx, tenant).await?;

    let vm_id = VmId::new();
    let volume_id = VolumeId::new();
    let disk = DiskSpec {
        volume_id,
        size_bytes: req.disk_gib * GIB,
        image_id: Some(req.image_id),
    };
    let mut spec = VmSpec {
        vcpus: req.vcpus,
        memory_mib: req.memory_mib,
        firmware: req.firmware,
        disks: vec![disk.clone()],
        nics: vec![],
        cloud_init: Some(CloudInit {
            hostname: req.name.clone(),
            user_data: req.user_data.clone().unwrap_or_else(|| user_data(&req)),
        }),
    };
    let desired = if req.start.unwrap_or(true) {
        "running"
    } else {
        "stopped"
    };
    hm_db::vms::create(
        &mut *tx,
        hm_db::vms::NewVm {
            id: vm_id,
            tenant_id: tenant,
            name: &req.name,
            host_id: host.id,
            spec: &spec,
            desired_power: desired,
            created_by: Some(p.user_id),
        },
    )
    .await?;
    hm_db::vms::create_volume(
        &mut *tx,
        &hm_db::vms::Volume {
            id: volume_id,
            tenant_id: tenant,
            vm_id: Some(vm_id),
            host_id: Some(host.id),
            pool: "local".into(),
            size_bytes: disk.size_bytes as i64,
            image_id: Some(req.image_id),
        },
    )
    .await?;
    let nic_id = NicId::new();
    let nic =
        hm_db::networks::create_nic(&mut tx, nic_id, vm_id, network.id, &mac_for(nic_id)).await?;
    spec.nics.push(NicSpec {
        id: nic_id,
        network_id: network.id,
        mac: nic.mac,
        ip: nic.ip.parse().map_err(ApiError::internal)?,
    });
    hm_db::vms::set_spec(&mut *tx, vm_id, &spec).await?;
    let task = enqueue(&mut tx, &p, tenant, "vm.create", vm_id, json!({})).await?;
    tx.commit().await?;

    tracing::info!(vm = %vm_id, name = %req.name, host = %host.name, "VM accepted");
    let vm = hm_db::vms::get(&state.db, vm_id).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(Accepted {
            task_id: task,
            item: view(vm, &host_names(&state).await?),
        }),
    ))
}

async fn enqueue(
    tx: &mut hm_db::sqlx::PgConnection,
    p: &Principal,
    tenant: TenantId,
    kind: &str,
    vm: VmId,
    params: serde_json::Value,
) -> ApiResult<TaskId> {
    let task = hm_db::tasks::enqueue(
        &mut *tx,
        hm_db::tasks::NewTask {
            tenant_id: tenant,
            kind,
            target: Some(("vm", vm.0)),
            params,
            created_by: Some(p.user_id),
        },
    )
    .await?;
    Ok(task.id)
}

/// Queues a power action after recording the new desired state.
async fn power_action(
    state: &AppState,
    p: &Principal,
    id: VmId,
    kind: &str,
    desired: Option<&str>,
    params: serde_json::Value,
) -> ApiResult<(StatusCode, Json<TaskRef>)> {
    let vm = load(state, p, id, Perm::VmWrite).await?;
    if vm.status == "deleting" {
        return Err(ApiError::conflict("VM is being deleted"));
    }
    let mut tx = state.db.begin().await?;
    if let Some(d) = desired {
        hm_db::vms::set_desired_power(&mut *tx, id, d).await?;
    }
    let task_id = enqueue(&mut tx, p, vm.tenant_id, kind, id, params).await?;
    tx.commit().await?;
    Ok((StatusCode::ACCEPTED, Json(TaskRef { task_id })))
}

#[utoipa::path(post, path = "/vms/{id}/start", tag = "vms", params(("id" = VmId, Path)),
    responses((status = 202, body = TaskRef)))]
async fn start_vm(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
) -> ApiResult<(StatusCode, Json<TaskRef>)> {
    power_action(&state, &p, id, "vm.start", Some("running"), json!({})).await
}

#[derive(Deserialize, IntoParams)]
pub struct StopQuery {
    /// Power off immediately instead of asking the guest to shut down.
    #[serde(default)]
    force: bool,
}

#[utoipa::path(post, path = "/vms/{id}/stop", tag = "vms", params(("id" = VmId, Path), StopQuery),
    responses((status = 202, body = TaskRef)))]
async fn stop_vm(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
    Query(q): Query<StopQuery>,
) -> ApiResult<(StatusCode, Json<TaskRef>)> {
    power_action(
        &state,
        &p,
        id,
        "vm.stop",
        Some("stopped"),
        json!({"force": q.force}),
    )
    .await
}

#[utoipa::path(post, path = "/vms/{id}/reboot", tag = "vms", params(("id" = VmId, Path)),
    responses((status = 202, body = TaskRef)))]
async fn reboot_vm(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
) -> ApiResult<(StatusCode, Json<TaskRef>)> {
    power_action(&state, &p, id, "vm.reboot", None, json!({})).await
}

#[utoipa::path(delete, path = "/vms/{id}", tag = "vms", params(("id" = VmId, Path)),
    responses((status = 202, body = TaskRef), (status = 404)))]
async fn delete_vm(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
) -> ApiResult<(StatusCode, Json<TaskRef>)> {
    let vm = load(&state, &p, id, Perm::VmWrite).await?;
    let mut tx = state.db.begin().await?;
    hm_db::vms::set_status(&mut *tx, id, "deleting", None).await?;
    let task_id = enqueue(&mut tx, &p, vm.tenant_id, "vm.delete", id, json!({})).await?;
    tx.commit().await?;
    Ok((StatusCode::ACCEPTED, Json(TaskRef { task_id })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convenience_user_data_is_cloud_config() {
        let req: CreateVm = serde_json::from_value(json!({
            "name": "a", "vcpus": 1, "memory_mib": 512, "disk_gib": 10,
            "image_id": ImageId::new(), "ssh_authorized_keys": ["ssh-ed25519 AAAA x"], "password": "p'w\"d"
        }))
        .unwrap();
        let ud = user_data(&req);
        assert!(ud.starts_with("#cloud-config\n"));
        let body: serde_json::Value = serde_json::from_str(ud.lines().nth(1).unwrap()).unwrap();
        assert_eq!(body["password"], "p'w\"d");
        assert_eq!(body["chpasswd"]["expire"], false);
        assert_eq!(body["ssh_authorized_keys"][0], "ssh-ed25519 AAAA x");
    }

    #[test]
    fn names_are_validated() {
        let mk = |name: &str| -> CreateVm {
            serde_json::from_value(json!({"name": name, "vcpus": 1, "memory_mib": 512, "disk_gib": 10, "image_id": ImageId::new()})).unwrap()
        };
        assert!(validate(&mk("web-1")).is_ok());
        for bad in ["", "-a", "a-", "a_b", "a.b", &"x".repeat(64)] {
            assert!(validate(&mk(bad)).is_err(), "{bad:?}");
        }
    }
}
