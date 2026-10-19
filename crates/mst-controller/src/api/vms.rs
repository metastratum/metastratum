use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use mst_core::ids::{
    BackupId, ClusterId, ExternalIpId, HostId, ImageId, LOCAL_POOL, NetworkId, NicId,
    SecurityGroupId, StoragePoolId, TaskId, TenantId, VmId, VolumeId,
};
use mst_core::vm::{CloudInit, DiskSpec, Firmware, NicSpec, VmSpec, mac_for};
use mst_db::vms::Vm;
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::authz::{Perm, Principal, audit, authorize, authorize_resource};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_vms, create_vm))
        .routes(routes!(get_vm, update_vm, delete_vm))
        .routes(routes!(start_vm))
        .routes(routes!(stop_vm))
        .routes(routes!(reboot_vm))
        .routes(routes!(set_nic_groups))
        .routes(routes!(migrate_vm))
        .routes(routes!(cancel_migration))
        .routes(routes!(restore_backup))
        .routes(routes!(resize_disk))
        .routes(routes!(vm_memory))
        .routes(routes!(clone_vm))
}

const GIB: u64 = 1 << 30;

#[derive(Serialize, ToSchema)]
pub struct VmView {
    id: VmId,
    tenant_id: TenantId,
    name: String,
    host_id: Option<HostId>,
    host_name: Option<String>,
    /// Whether the VM's host is connected and reporting. When it is not,
    /// `power` is the last state reported and may be out of date.
    host_connected: bool,
    /// Lifecycle: `creating`, `ready`, `error` or `deleting`.
    status: String,
    /// A template: kept to be cloned, never started.
    template: bool,
    /// The power state last requested through the API: `running` or
    /// `stopped`. Never changed by what happens on the host.
    desired_power: String,
    /// What the host last reported: `running`, `stopped`, `paused`,
    /// `crashed`, `shutting_down`, `missing` or `unknown`.
    power: String,
    /// Why a stopped or crashed VM stopped, when the host knew: `shutdown`
    /// (by the guest, or a shutdown request it honoured), `destroyed`
    /// (forced off), `crashed`, `failed`, `daemon`, ...
    power_reason: Option<String>,
    /// When `power` (or its reason) last changed.
    power_observed_at: Option<DateTime<Utc>>,
    /// Kind of the task in flight for this VM, e.g. `vm.start`.
    active_task: Option<String>,
    error: Option<String>,
    /// Virtual CPUs plugged in.
    vcpus: u32,
    /// What `vcpus` may grow to without a restart.
    max_vcpus: u32,
    /// CPUs are plugged in and out while it runs (it has a maximum), up to
    /// `max_vcpus` and down to one.
    cpu_hotplug: bool,
    /// What it has now (its balloon's size, while it runs).
    memory_mib: u64,
    /// What `memory_mib` may grow to without a restart.
    max_memory_mib: u64,
    /// The guest hands memory it frees back to its host.
    free_page_reporting: bool,
    firmware: Firmware,
    /// Can live-migrate; otherwise it has its host's CPU and disk caching.
    migratable: bool,
    /// The CPU model the guest sees: its cluster's (a model all its hosts
    /// run), or none for the host's CPU as is (a VM that does not migrate).
    cpu_model: Option<String>,
    disks: Vec<DiskSpec>,
    nics: Vec<NicView>,
    /// The live migration in progress, if any.
    migration: Option<MigrationView>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Serialize, ToSchema)]
pub struct MigrationView {
    task_id: TaskId,
    dest_host_id: HostId,
    dest_host_name: Option<String>,
    /// `preparing`, `running` or `committing`.
    state: String,
    cancel_requested: bool,
    started_at: DateTime<Utc>,
}

#[derive(Serialize, ToSchema)]
pub struct NicView {
    id: NicId,
    network_id: NetworkId,
    network_name: Option<String>,
    mac: String,
    #[schema(value_type = String, format = Ipv4)]
    ip: std::net::Ipv4Addr,
    /// What traffic the NIC passes; none passes nothing.
    security_group_ids: Vec<SecurityGroupId>,
    /// The floating IP forwarding to this NIC, if any.
    floating_ip: Option<String>,
    floating_ip_id: Option<ExternalIpId>,
}

/// What VM views need besides the VM row: host names and liveness, the
/// tasks in flight, and network details of NICs.
struct Context {
    hosts: HashMap<HostId, (String, bool)>,
    tasks: HashMap<uuid::Uuid, String>,
    networks: HashMap<NetworkId, String>,
    nic_groups: HashMap<NicId, Vec<SecurityGroupId>>,
    floating: HashMap<NicId, (ExternalIpId, String)>,
    migrations: HashMap<VmId, mst_db::migrations::Migration>,
}

async fn context(state: &AppState) -> ApiResult<Context> {
    type NicRow = (
        NicId,
        Option<SecurityGroupId>,
        Option<ExternalIpId>,
        Option<String>,
    );
    let nic_rows: Vec<NicRow> = mst_db::sqlx::query_as(
        "SELECT n.id, g.security_group_id, f.id, host(f.ip)
             FROM nics n LEFT JOIN nic_security_groups g ON g.nic_id = n.id
             LEFT JOIN external_ips f ON f.nic_id = n.id",
    )
    .fetch_all(&state.db)
    .await?;
    let mut nic_groups: HashMap<NicId, Vec<SecurityGroupId>> = HashMap::new();
    let mut floating = HashMap::new();
    for (nic, group, fip, ip) in nic_rows {
        let groups = nic_groups.entry(nic).or_default();
        if let Some(g) = group.filter(|g| !groups.contains(g)) {
            groups.push(g);
        }
        if let (Some(f), Some(ip)) = (fip, ip) {
            floating.insert(nic, (f, ip));
        }
    }
    let networks: Vec<(NetworkId, String)> =
        mst_db::sqlx::query_as("SELECT id, name FROM networks")
            .fetch_all(&state.db)
            .await?;
    let migrations = mst_db::migrations::list(&state.db).await?;
    Ok(Context {
        migrations: migrations.into_iter().map(|m| (m.vm_id, m)).collect(),
        networks: networks.into_iter().collect(),
        nic_groups,
        floating,
        hosts: mst_db::hosts::list(&state.db)
            .await?
            .into_iter()
            .map(|h| (h.id, (h.name.clone(), h.is_live())))
            .collect(),
        tasks: mst_db::tasks::active_by_target(&state.db)
            .await?
            .into_iter()
            .collect(),
    })
}

fn view(vm: Vm, ctx: &Context) -> VmView {
    let spec = vm.spec.0;
    let host = vm.host_id.and_then(|h| ctx.hosts.get(&h));
    VmView {
        id: vm.id,
        tenant_id: vm.tenant_id,
        name: vm.name,
        host_name: host.map(|(name, _)| name.clone()),
        host_connected: host.is_some_and(|(_, live)| *live),
        host_id: vm.host_id,
        status: vm.status,
        template: vm.template,
        desired_power: vm.desired_power,
        power: vm.power,
        power_reason: vm.power_reason,
        power_observed_at: vm.power_observed_at,
        active_task: ctx.tasks.get(&vm.id.0).cloned(),
        error: vm.error,
        vcpus: spec.vcpus,
        max_vcpus: spec.reserved_vcpus(),
        cpu_hotplug: spec.max_vcpus.is_some(),
        memory_mib: spec.memory_mib,
        max_memory_mib: spec.reserved_memory_mib(),
        free_page_reporting: spec.free_page_reporting,
        firmware: spec.firmware,
        migratable: spec.migratable,
        cpu_model: spec.cpu.as_ref().map(|c| c.model.clone()),
        disks: spec.disks,
        nics: spec
            .nics
            .into_iter()
            .map(|n| NicView {
                network_name: ctx.networks.get(&n.network_id).cloned(),
                security_group_ids: ctx.nic_groups.get(&n.id).cloned().unwrap_or_default(),
                floating_ip_id: ctx.floating.get(&n.id).map(|f| f.0),
                floating_ip: ctx.floating.get(&n.id).map(|f| f.1.clone()),
                id: n.id,
                network_id: n.network_id,
                mac: n.mac,
                ip: n.ip,
            })
            .collect(),
        migration: ctx.migrations.get(&vm.id).map(|m| MigrationView {
            task_id: m.task_id,
            dest_host_id: m.dest_host_id,
            dest_host_name: ctx.hosts.get(&m.dest_host_id).map(|h| h.0.clone()),
            state: m.state.clone(),
            cancel_requested: m.cancel_requested,
            started_at: m.started_at,
        }),
        created_at: vm.created_at,
        updated_at: vm.updated_at,
    }
}

/// Loads a VM the principal may act on with `perm`.
async fn load(state: &AppState, p: &Principal, id: VmId, perm: Perm) -> ApiResult<Vm> {
    let vm = mst_db::vms::get(&state.db, id).await?;
    authorize_resource(&state.db, p, perm, vm.tenant_id, "vm").await?;
    Ok(vm)
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
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
    let ctx = context(&state).await?;
    let vms = mst_db::vms::list(&state.db, tenant, q.descendants).await?;
    Ok(Json(vms.into_iter().map(|v| view(v, &ctx)).collect()))
}

#[utoipa::path(get, path = "/vms/{id}", tag = "vms", params(("id" = VmId, Path)),
    responses((status = 200, body = VmView), (status = 404)))]
async fn get_vm(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
) -> ApiResult<Json<VmView>> {
    let vm = load(&state, &p, id, Perm::VmRead).await?;
    Ok(Json(view(vm, &context(&state).await?)))
}

#[derive(Deserialize, ToSchema)]
pub struct CreateVm {
    /// Owning tenant; defaults to the caller's.
    tenant_id: Option<TenantId>,
    /// DNS-style label, unique within the tenant.
    name: String,
    vcpus: u32,
    /// What its virtual CPUs may grow to while it runs (plugged in, and out
    /// again); `vcpus` by default. Its host keeps this many for it, and they
    /// count against the tenant's vCPU quota.
    #[serde(default)]
    max_vcpus: Option<u32>,
    memory_mib: u64,
    /// What its memory may grow to while it runs (through its balloon, for
    /// guests with the driver); `memory_mib` by default. Its host keeps
    /// this much for it, and it counts against the tenant's memory quota.
    #[serde(default)]
    max_memory_mib: Option<u64>,
    /// The guest hands memory it frees back to its host (on by default).
    #[serde(default)]
    free_page_reporting: Option<bool>,
    /// Size of the boot disk. Must be at least the image's size.
    disk_gib: u64,
    image_id: ImageId,
    #[serde(default)]
    firmware: Firmware,
    /// SSH public keys for the image's default user.
    #[serde(default)]
    ssh_authorized_keys: Vec<String>,
    /// Console password for the image's default user. Handy for the serial
    /// console; prefer SSH keys otherwise. Only a SHA-512 crypt hash of it is
    /// stored or passed to the guest.
    password: Option<String>,
    /// Complete cloud-init user data. Overrides `ssh_authorized_keys` and
    /// `password`.
    user_data: Option<String>,
    /// Start the VM once created (default true).
    start: Option<bool>,
    /// Cluster to place the VM in; any cluster the tenant may use otherwise.
    cluster_id: Option<ClusterId>,
    /// Pool for the VM's disk; local storage on its host by default.
    storage_pool_id: Option<StoragePoolId>,
    /// Network to connect to (the tenant's own); its default network
    /// otherwise.
    network_id: Option<NetworkId>,
    /// Security groups for the NIC (the tenant's own); its `default` group
    /// otherwise. An empty list blocks all traffic but DHCP.
    security_group_ids: Option<Vec<SecurityGroupId>>,
    /// Whether the VM may live-migrate to other hosts of its cluster. If
    /// not, it gets its host's full CPU feature set and disks use the host's
    /// page cache. By default VMs are migratable when their cluster has more
    /// than one host.
    migratable: Option<bool>,
}

#[derive(Deserialize, ToSchema)]
pub struct RestoreBackup {
    /// Name of the new VM.
    name: String,
    /// Start it once restored (default true).
    start: Option<bool>,
    /// Cluster to place it in; any the tenant may use otherwise.
    cluster_id: Option<ClusterId>,
    /// Pool for its disks; local storage on its host by default.
    storage_pool_id: Option<StoragePoolId>,
    /// Network to connect it to; the tenant's default network otherwise.
    network_id: Option<NetworkId>,
    /// Security groups for its NIC; the tenant's `default` group otherwise.
    security_group_ids: Option<Vec<SecurityGroupId>>,
    /// SSH keys for the image's default user, as when creating a VM.
    #[serde(default)]
    ssh_authorized_keys: Vec<String>,
    /// A new console password for the image's default user.
    password: Option<String>,
}

/// Restores a backup as a new VM in the backup's tenant: the backed-up VM's
/// CPUs, memory, firmware and disks, on a new NIC. It boots as a new
/// instance, so cloud-init sets up its network again; the disks keep
/// everything else. The backup stays.
#[utoipa::path(post, path = "/backups/{id}/restore", tag = "backups", params(("id" = BackupId, Path)),
    request_body = RestoreBackup,
    responses((status = 202, body = Accepted<VmView>), (status = 403), (status = 404),
        (status = 409, description = "The backup is not complete")))]
async fn restore_backup(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<BackupId>,
    Json(req): Json<RestoreBackup>,
) -> ApiResult<(StatusCode, Json<Accepted<VmView>>)> {
    let backup = mst_db::backups::get(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::VmWrite, backup.tenant_id, "backup").await?;
    if backup.state != "ready" {
        return Err(ApiError::conflict("the backup is not complete"));
    }
    if mst_db::backups::deleting_below(&state.db, backup.id).await? {
        return Err(ApiError::conflict(
            "a backup it builds on is being deleted (merged into it): restore it once that is done",
        ));
    }
    let spec = &backup.spec.0;
    let disk_bytes: u64 = spec.disks.iter().map(|d| d.size_bytes).sum();
    let create = CreateVm {
        tenant_id: Some(backup.tenant_id),
        name: req.name,
        vcpus: spec.vcpus,
        max_vcpus: spec.max_vcpus,
        memory_mib: spec.memory_mib,
        max_memory_mib: spec.max_memory_mib,
        free_page_reporting: Some(spec.free_page_reporting),
        disk_gib: disk_bytes.div_ceil(GIB),
        image_id: ImageId(uuid::Uuid::nil()),
        firmware: spec.firmware,
        ssh_authorized_keys: req.ssh_authorized_keys,
        password: req.password,
        user_data: None,
        start: req.start,
        cluster_id: req.cluster_id,
        storage_pool_id: req.storage_pool_id,
        network_id: req.network_id,
        security_group_ids: req.security_group_ids,
        migratable: None,
    };
    let out = create_inner(state.clone(), p.clone(), create, Source::Backup(&backup)).await?;
    audit(
        &state.db,
        Some(&p),
        Some(backup.tenant_id),
        "backup.restore",
        Some(("backup", backup.id.0)),
        json!({"vm_name": backup.vm_name}),
    )
    .await;
    Ok(out)
}

#[derive(Deserialize, ToSchema)]
pub struct CloneVm {
    /// Name of the new VM.
    name: String,
    /// Start it once created (default true).
    start: Option<bool>,
    /// Cluster to place it in, for shared storage; any the tenant may use
    /// otherwise. A VM on local storage is cloned on its host.
    cluster_id: Option<ClusterId>,
    /// Network to connect it to; the tenant's default network otherwise.
    network_id: Option<NetworkId>,
    /// Security groups for its NIC; the tenant's `default` group otherwise.
    security_group_ids: Option<Vec<SecurityGroupId>>,
    /// SSH keys for the image's default user, as when creating a VM.
    #[serde(default)]
    ssh_authorized_keys: Vec<String>,
    /// A console password for the image's default user.
    password: Option<String>,
}

/// Creates a VM with copies of a stopped VM's (or template's) disks, in the
/// same tenant and storage pool, with its CPUs, memory and firmware, on a
/// new NIC. It boots as a new instance, so cloud-init sets up its network,
/// host name and host keys again; the disks keep everything else.
#[utoipa::path(post, path = "/vms/{id}/clone", tag = "vms", params(("id" = VmId, Path)),
    request_body = CloneVm,
    responses((status = 202, body = Accepted<VmView>), (status = 400), (status = 403), (status = 404),
        (status = 409, description = "The VM is running or busy")))]
async fn clone_vm(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
    Json(req): Json<CloneVm>,
) -> ApiResult<(StatusCode, Json<Accepted<VmView>>)> {
    let source = load(&state, &p, id, Perm::VmWrite).await?;
    if source.status != "ready" || source.power != "stopped" || source.desired_power != "stopped" {
        return Err(ApiError::conflict(format!(
            "stop {} first: only a stopped VM is cloned",
            source.name
        )));
    }
    super::check_disks_idle(&mut *state.db.acquire().await?, id).await?;
    let spec = &source.spec.0;
    let disk_bytes: u64 = spec.disks.iter().map(|d| d.size_bytes).sum();
    let create = CreateVm {
        tenant_id: Some(source.tenant_id),
        name: req.name,
        vcpus: spec.vcpus,
        max_vcpus: spec.max_vcpus,
        memory_mib: spec.memory_mib,
        max_memory_mib: spec.max_memory_mib,
        free_page_reporting: Some(spec.free_page_reporting),
        disk_gib: disk_bytes.div_ceil(GIB),
        image_id: ImageId(uuid::Uuid::nil()),
        firmware: spec.firmware,
        ssh_authorized_keys: req.ssh_authorized_keys,
        password: req.password,
        user_data: None,
        start: req.start,
        cluster_id: req.cluster_id,
        storage_pool_id: None,
        network_id: req.network_id,
        security_group_ids: req.security_group_ids,
        migratable: Some(spec.migratable),
    };
    create_inner(state, p, create, Source::Clone(&source)).await
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

/// `disk_gib` is the boot disk's size for VMs from images; for clones and
/// restores, the sum of the disks they copy (each within bounds already).
fn validate(req: &CreateVm, from_image: bool) -> ApiResult<()> {
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
    check_size(req.vcpus, req.memory_mib)?;
    check_max_vcpus(req.vcpus, req.max_vcpus)?;
    check_max_memory(req.memory_mib, req.max_memory_mib)?;
    if from_image && !(1..=16384).contains(&req.disk_gib) {
        return Err(ApiError::bad_request(
            "disk_gib must be between 1 and 16384",
        ));
    }
    Ok(())
}

/// SHA-512 crypt (`$6$`): cloud-init recognises it as already hashed, and
/// every Linux guest's libc can verify it. Newer schemes such as yescrypt
/// would be mistaken for a plain-text password by cloud-init.
fn hash_console_password(password: &str) -> ApiResult<String> {
    // The salt is used in its crypt-base64 form. glibc and libxcrypt only
    // use the first 16 characters of it, and 12 bytes encode to exactly 16;
    // longer salts produce hashes guests cannot verify.
    sha512_crypt(password, &rand::random::<[u8; 12]>())
}

fn sha512_crypt(password: &str, salt: &[u8; 12]) -> ApiResult<String> {
    use sha_crypt::{PasswordHasher, ShaCrypt};
    Ok(ShaCrypt::SHA512
        .hash_password_with_salt(password.as_bytes(), salt)
        .map_err(ApiError::internal)?
        .to_string())
}

/// Builds `#cloud-config` user data from the convenience fields. The password
/// is hashed here, so the plain text never reaches the database, the host or
/// the guest's disk.
/// A udev rule that brings CPUs plugged into a running guest online.
pub const CPU_HOTPLUG_RULE: &str = "SUBSYSTEM==\"cpu\", ACTION==\"add\", TEST==\"online\", ATTR{online}==\"0\", ATTR{online}=\"1\"\n";

fn user_data(req: &CreateVm) -> ApiResult<String> {
    let mut doc = serde_json::Map::new();
    if !req.ssh_authorized_keys.is_empty() {
        doc.insert("ssh_authorized_keys".into(), json!(req.ssh_authorized_keys));
    }
    if let Some(pw) = &req.password {
        doc.insert("password".into(), json!(hash_console_password(pw)?));
        doc.insert("chpasswd".into(), json!({"expire": false}));
    }
    // Linux leaves CPUs plugged in while it runs offline; this brings them
    // online as they arrive.
    doc.insert(
        "write_files".into(),
        json!([{"path": "/etc/udev/rules.d/80-metastratum-cpu-hotplug.rules",
                "content": CPU_HOTPLUG_RULE, "permissions": "0644"}]),
    );
    // JSON is valid YAML, so this needs no YAML serializer.
    Ok(format!(
        "#cloud-config\n{}\n",
        serde_json::Value::Object(doc)
    ))
}

#[utoipa::path(post, path = "/vms", tag = "vms", request_body = CreateVm,
    responses((status = 202, body = Accepted<VmView>), (status = 400), (status = 409), (status = 503)))]
async fn create_vm(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<CreateVm>,
) -> ApiResult<(StatusCode, Json<Accepted<VmView>>)> {
    create_inner(state, p, req, Source::Image).await
}

/// What a new VM's disks come from.
#[derive(Clone, Copy)]
enum Source<'a> {
    /// The request's image.
    Image,
    /// A backup's disks, restored.
    Backup(&'a mst_db::backups::Backup),
    /// Another VM's disks, copied in their pool (on their host, for local
    /// storage).
    Clone(&'a mst_db::vms::Vm),
}

/// Creates a VM from an image, a backup or another VM.
async fn create_inner(
    state: AppState,
    p: Principal,
    req: CreateVm,
    source: Source<'_>,
) -> ApiResult<(StatusCode, Json<Accepted<VmView>>)> {
    validate(&req, matches!(source, Source::Image))?;
    let tenant = req.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::VmWrite, tenant).await?;
    if matches!(source, Source::Image) {
        if !mst_db::images::is_usable_by(&state.db, req.image_id, tenant).await? {
            return Err(ApiError::not_found("image"));
        }
        // One being downloaded is waited for.
        let image = mst_db::images::get(&state.db, req.image_id).await?;
        if image.state == "failed" {
            return Err(ApiError::conflict(format!(
                "the image could not be downloaded ({}); download it again first",
                image.error.as_deref().unwrap_or("no reason given")
            )));
        }
        if let Some(size) = image.virtual_bytes
            && (req.disk_gib * GIB) < size as u64
        {
            return Err(ApiError::bad_request(format!(
                "the disk must be at least as large as the image: {} GiB",
                (size as u64).div_ceil(GIB)
            )));
        }
    }
    // A clone's disks are copied in their own pools: local ones on their
    // host, which pins the clone there.
    let pools: Vec<mst_db::storage_pools::Pool> = match source {
        Source::Clone(vm) => {
            let mut ids: Vec<StoragePoolId> = vm.spec.0.disks.iter().map(|d| d.pool_id).collect();
            ids.dedup();
            let mut pools = vec![];
            for id in ids {
                if !pools
                    .iter()
                    .any(|p: &mst_db::storage_pools::Pool| p.id == id)
                {
                    pools.push(mst_db::storage_pools::get(&state.db, id).await?);
                }
            }
            pools
        }
        _ => vec![
            mst_db::storage_pools::get(&state.db, req.storage_pool_id.unwrap_or(LOCAL_POOL))
                .await?,
        ],
    };
    for pool in &pools {
        if !super::storage_pools::may_use(&state, tenant, pool).await? {
            return Err(ApiError::not_found("storage pool"));
        }
    }
    let pin = match source {
        Source::Clone(vm) if pools.iter().any(|p| p.kind == "local") => vm.host_id,
        _ => None,
    };
    // Placed by a shared pool if there is one (its clusters), then checked
    // against the others.
    let pool = pools
        .iter()
        .find(|p| p.kind != "local")
        .or(pools.first())
        .cloned()
        .ok_or_else(|| ApiError::conflict("the VM has no disks"))?;

    let mut tx = state.db.begin().await?;
    let host = place(&state, &mut tx, tenant, &req, &pool, pin).await?;
    for other in pools.iter().filter(|p| p.kind != "local") {
        if !host.cluster.is_some_and(|c| other.clusters.contains(&c)) {
            return Err(ApiError::conflict(format!(
                "no host has room that reaches all of its disks' pools ({} too)",
                other.name
            )));
        }
    }
    let delta: mst_db::quotas::Amounts = [
        ("vms", 1),
        ("vcpus", i64::from(reserved_vcpus(&req))),
        ("memory_mib", reserved_memory(&req) as i64),
        ("disk_gib", req.disk_gib as i64),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    mst_db::quotas::reserve(&mut tx, tenant, &delta).await?;
    let network = match req.network_id {
        Some(id) => {
            let n = mst_db::networks::get(&mut *tx, id).await?;
            if n.tenant_id != tenant {
                return Err(ApiError::not_found("network"));
            }
            n
        }
        None => mst_db::networks::default_for(&mut tx, tenant).await?,
    };
    let groups = match &req.security_group_ids {
        Some(ids) => {
            check_groups(&mut tx, tenant, ids).await?;
            ids.clone()
        }
        None => vec![mst_db::netsec::default_group(&mut tx, tenant).await?.id],
    };

    let vm_id = VmId::new();
    let disks: Vec<DiskSpec> = match source {
        Source::Image => vec![DiskSpec {
            volume_id: VolumeId::new(),
            pool_id: pool.id,
            size_bytes: req.disk_gib * GIB,
            image_id: Some(req.image_id),
            restored_from: None,
            cloned_from: None,
            unit: None,
            extra: Default::default(),
        }],
        // Every disk the source has, at its size, made from the image it
        // was made from (copies, which depend on no image).
        Source::Clone(vm) => vm
            .spec
            .0
            .disks
            .iter()
            .map(|d| DiskSpec {
                volume_id: VolumeId::new(),
                pool_id: d.pool_id,
                size_bytes: d.size_bytes,
                image_id: d.image_id,
                restored_from: None,
                cloned_from: Some(d.volume_id),
                unit: d.unit,
                extra: Default::default(),
            })
            .collect(),
        // Every disk the backup has, at its size, in the chosen pool.
        Source::Backup(b) => b
            .spec
            .0
            .disks
            .iter()
            .enumerate()
            .map(|(i, d)| DiskSpec {
                volume_id: VolumeId::new(),
                pool_id: pool.id,
                size_bytes: d.size_bytes,
                image_id: None,
                restored_from: Some(mst_core::vm::RestoredFrom {
                    backup_id: b.id,
                    index: i as u32,
                }),
                cloned_from: None,
                unit: d.unit,
                extra: Default::default(),
            })
            .collect(),
    };
    // A VM whose cluster is a single host has nowhere to migrate to; let it
    // use everything the host offers instead.
    let migratable = match req.migratable {
        Some(m) => m,
        None => match host.cluster {
            Some(c) => mst_db::sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM hosts WHERE cluster_id = $1 AND state <> 'decommissioned'",
            )
            .bind(c)
            .fetch_one(&mut *tx)
            .await?
                > 1,
            None => false,
        },
    };
    let mut spec = VmSpec {
        // The create task sets the cluster's baseline CPU before defining the VM.
        cpu: None,
        migratable,
        extra: Default::default(),
        vcpus: req.vcpus,
        max_vcpus: req.max_vcpus.filter(|m| *m > req.vcpus),
        memory_mib: req.memory_mib,
        max_memory_mib: req.max_memory_mib.filter(|m| *m > req.memory_mib),
        free_page_reporting: req.free_page_reporting.unwrap_or(true),
        firmware: req.firmware,
        disks: disks.clone(),
        nics: vec![],
        cloud_init: Some(CloudInit {
            hostname: req.name.clone(),
            user_data: match &req.user_data {
                Some(ud) => ud.clone(),
                None => user_data(&req)?,
            },
        }),
    };
    let desired = if req.start.unwrap_or(true) {
        "running"
    } else {
        "stopped"
    };
    mst_db::vms::create(
        &mut *tx,
        mst_db::vms::NewVm {
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
    for disk in &disks {
        let local = pools
            .iter()
            .find(|p| p.id == disk.pool_id)
            .is_some_and(|p| p.kind == "local");
        mst_db::vms::create_volume(
            &mut *tx,
            &mst_db::vms::Volume {
                id: disk.volume_id,
                tenant_id: tenant,
                vm_id: Some(vm_id),
                // Only local volumes are tied to a host.
                host_id: local.then_some(host.id),
                pool_id: disk.pool_id,
                size_bytes: disk.size_bytes as i64,
            },
        )
        .await?;
    }
    let nic_id = NicId::new();
    let nic =
        mst_db::networks::create_nic(&mut tx, nic_id, vm_id, network.id, &mac_for(nic_id)).await?;
    mst_db::networks::set_nic_groups(&mut tx, nic_id, &groups).await?;
    spec.nics.push(NicSpec {
        id: nic_id,
        network_id: network.id,
        mac: nic.mac,
        ip: nic.ip.parse().map_err(ApiError::internal)?,
    });
    mst_db::vms::set_spec(&mut *tx, vm_id, &spec).await?;
    let task = enqueue(&mut tx, &p, tenant, "vm.create", vm_id, json!({})).await?;
    tx.commit().await?;

    tracing::info!(vm = %vm_id, name = %req.name, host = %host.name, "VM accepted");
    audit(
        &state.db,
        Some(&p),
        Some(tenant),
        "vm.create",
        Some(("vm", vm_id.0)),
        json!({"name": req.name, "vcpus": req.vcpus, "memory_mib": req.memory_mib, "disk_gib": req.disk_gib, "host": host.name,
               "restored_from": match source { Source::Backup(b) => Some(b.id), _ => None },
               "cloned_from": match source { Source::Clone(vm) => Some(vm.id), _ => None }}),
    )
    .await;
    let vm = mst_db::vms::get(&state.db, vm_id).await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(Accepted {
            task_id: task,
            item: view(vm, &context(&state).await?),
        }),
    ))
}

/// Security groups must belong to the VM's tenant.
async fn check_groups(
    tx: &mut mst_db::sqlx::PgConnection,
    tenant: TenantId,
    ids: &[SecurityGroupId],
) -> ApiResult<()> {
    for id in ids {
        let g = mst_db::netsec::get_group(&mut *tx, *id).await?;
        if g.tenant_id != tenant {
            return Err(ApiError::not_found("security group"));
        }
    }
    Ok(())
}

#[derive(Deserialize, ToSchema)]
pub struct NicGroups {
    /// Replaces the NIC's groups. An empty list blocks all traffic but DHCP.
    security_group_ids: Vec<SecurityGroupId>,
}

/// Changes which security groups apply to a NIC. Takes effect at once; the
/// VM keeps running.
#[utoipa::path(put, path = "/vms/{id}/nics/{nic}/security-groups", tag = "vms",
    params(("id" = VmId, Path), ("nic" = NicId, Path)), request_body = NicGroups,
    responses((status = 200, body = VmView), (status = 400), (status = 403), (status = 404)))]
async fn set_nic_groups(
    State(state): State<AppState>,
    p: Principal,
    Path((id, nic)): Path<(VmId, NicId)>,
    Json(req): Json<NicGroups>,
) -> ApiResult<Json<VmView>> {
    let vm = load(&state, &p, id, Perm::VmWrite).await?;
    if !vm.spec.0.nics.iter().any(|n| n.id == nic) {
        return Err(ApiError::not_found("NIC"));
    }
    let mut groups = req.security_group_ids.clone();
    groups.sort();
    groups.dedup();
    let mut tx = state.db.begin().await?;
    check_groups(&mut tx, vm.tenant_id, &groups).await?;
    mst_db::networks::set_nic_groups(&mut tx, nic, &groups).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(vm.tenant_id),
        "vm.nic.security_groups",
        Some(("vm", id.0)),
        json!({"nic": nic, "security_group_ids": groups}),
    )
    .await;
    state.net.kick();
    let vm = mst_db::vms::get(&state.db, id).await?;
    Ok(Json(view(vm, &context(&state).await?)))
}

/// Picks a host for a new VM. Hosts are locked until the transaction ends,
/// so concurrent creations see each other's allocations.
async fn place(
    state: &AppState,
    tx: &mut mst_db::sqlx::PgConnection,
    tenant: TenantId,
    req: &CreateVm,
    pool: &mst_db::storage_pools::Pool,
    pin: Option<HostId>,
) -> ApiResult<mst_scheduler::Host> {
    let mut clusters = super::storage_pools::clusters_for(
        pool,
        super::clusters::allowed_clusters(state, tenant).await?,
    );
    // A shared pool restricts placement to its clusters; say so when none
    // of them is available, rather than blaming every host.
    if pool.kind != "local" && clusters.as_ref().is_some_and(|c| c.is_empty()) {
        return Err(ApiError::unavailable(format!(
            "storage pool {} is not attached to any cluster this tenant may use",
            pool.name
        )));
    }
    if let Some(want) = req.cluster_id {
        if clusters.as_ref().is_some_and(|c| !c.contains(&want)) {
            return Err(ApiError::not_found("cluster"));
        }
        clusters = Some([want].into_iter().collect());
    }
    let hosts: Vec<mst_scheduler::Host> = mst_db::clusters::candidates_locked(tx)
        .await?
        .into_iter()
        .map(|c| mst_scheduler::Host {
            id: c.id,
            name: c.name,
            cluster: c.cluster_id,
            live: c.live,
            state: c.state,
            kvm: c.kvm,
            cpus: c.cpu_count.max(0) as u32,
            memory_mib: c.memory_mib.max(0) as u64,
            cpu_overcommit: f64::from(c.cpu_overcommit),
            memory_overcommit: f64::from(c.memory_overcommit),
            used_vcpus: c.used_vcpus.max(0) as u64,
            used_memory_mib: c.used_memory_mib.max(0) as u64,
        })
        .collect();
    let request = mst_scheduler::Request {
        vcpus: reserved_vcpus(req),
        memory_mib: reserved_memory(req),
        clusters,
        host: pin,
    };
    let id =
        mst_scheduler::place(&request, &hosts).map_err(|e| ApiError::unavailable(e.to_string()))?;
    Ok(hosts
        .into_iter()
        .find(|h| h.id == id)
        .expect("placed on a candidate"))
}

async fn enqueue(
    tx: &mut mst_db::sqlx::PgConnection,
    p: &Principal,
    tenant: TenantId,
    kind: &str,
    vm: VmId,
    params: serde_json::Value,
) -> ApiResult<TaskId> {
    let task = mst_db::tasks::enqueue(
        &mut *tx,
        mst_db::tasks::NewTask {
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
    if vm.status == "migrating" {
        return Err(ApiError::conflict(
            "VM is being migrated; wait, or cancel the migration",
        ));
    }
    let mut tx = state.db.begin().await?;
    if let Some(d) = desired {
        // Locks the VM: a revert that started meanwhile is seen below.
        mst_db::vms::set_desired_power(&mut *tx, id, d).await?;
    }
    if mst_db::snapshots::busy(&mut *tx, id).await?.as_deref() == Some("reverting") {
        return Err(ApiError::conflict(
            "the VM is being reverted to a snapshot; start it once that is done",
        ));
    }
    // Its disks change with it as it is: plugged in or out while it runs,
    // in its definition while it is stopped.
    if mst_db::disks::busy_on_vm(&mut *tx, id).await?.is_some() {
        return Err(ApiError::conflict(
            "a disk of the VM is being attached or detached; try again once that is done",
        ));
    }
    if desired == Some("running") {
        if vm.template {
            return Err(ApiError::conflict(
                "the VM is a template: clone it, or make it a VM again first",
            ));
        }
        if mst_db::vms::being_cloned(&mut *tx, id).await? {
            return Err(ApiError::conflict(
                "a clone of the VM is being made; start it once that is done",
            ));
        }
    }
    let task_id = enqueue(&mut tx, p, vm.tenant_id, kind, id, params.clone()).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(p),
        Some(vm.tenant_id),
        kind,
        Some(("vm", id.0)),
        json!({"name": vm.name, "params": params}),
    )
    .await;
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
#[into_params(parameter_in = Query)]
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

#[derive(Deserialize, IntoParams, Default)]
#[into_params(parameter_in = Query)]
pub struct DeleteQuery {
    /// Delete the disks of their own attached to it too; otherwise they are
    /// kept, attached to nothing.
    #[serde(default)]
    delete_disks: bool,
}

/// Deletes a VM with the disks made with it: its boot disk, and the copies
/// a clone or restore made (unnamed). Disks of their own attached to it
/// (named) are kept, attached to nothing, unless `delete_disks` is given.
#[utoipa::path(delete, path = "/vms/{id}", tag = "vms", params(("id" = VmId, Path), DeleteQuery),
    responses((status = 202, body = TaskRef), (status = 404)))]
async fn delete_vm(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
    Query(q): Query<DeleteQuery>,
) -> ApiResult<(StatusCode, Json<TaskRef>)> {
    let vm = load(&state, &p, id, Perm::VmWrite).await?;
    if vm.status == "migrating" {
        return Err(ApiError::conflict(
            "VM is being migrated; cancel the migration first",
        ));
    }
    // Once deleting fails, the VM is in error, and can be deleted again.
    if vm.status == "deleting" {
        return Err(ApiError::conflict("VM is being deleted"));
    }
    if let Err(e) = super::snapshots::check_idle(&mut *state.db.acquire().await?, id).await {
        return Err(ApiError::conflict(format!(
            "{}; delete the VM once that is done",
            e.message
        )));
    }
    let mut tx = state.db.begin().await?;
    if !mst_db::vms::start_deleting(&mut *tx, id).await? {
        return Err(ApiError::conflict("VM is being deleted, or migrated"));
    }
    let params = json!({"delete_disks": q.delete_disks});
    let task_id = enqueue(&mut tx, &p, vm.tenant_id, "vm.delete", id, params).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(vm.tenant_id),
        "vm.delete",
        Some(("vm", id.0)),
        json!({"name": vm.name, "delete_disks": q.delete_disks}),
    )
    .await;
    Ok((StatusCode::ACCEPTED, Json(TaskRef { task_id })))
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateVm {
    /// Allow live migration (the cluster's CPU model, no host disk cache) or
    /// not (the host's full CPU, host disk cache). Only while the VM is
    /// stopped; it applies from the next start.
    migratable: Option<bool>,
    /// Virtual CPUs (1-128). While the VM runs, within `max_vcpus`
    /// (plugged in or out); otherwise while it is stopped, its host having
    /// room for more.
    vcpus: Option<u32>,
    /// What vCPUs may grow to while the VM runs. Only while it is stopped;
    /// it counts against quotas and its host's room as its vCPUs do.
    max_vcpus: Option<u32>,
    /// Memory in MiB (128-4194304). While the VM runs, within
    /// `max_memory_mib`, through its balloon (the guest's virtio_balloon
    /// driver must be loaded); otherwise while it is stopped, its host
    /// having room for more.
    memory_mib: Option<u64>,
    /// What memory may grow to while the VM runs. Only while it is stopped;
    /// it counts against quotas and its host's room as its memory does.
    max_memory_mib: Option<u64>,
    /// The guest hands memory it frees back to its host. Only while it is
    /// stopped; it applies from the next start.
    free_page_reporting: Option<bool>,
    /// Make the stopped VM a template, kept to be cloned and never started,
    /// or a VM again.
    template: Option<bool>,
    /// Fit its CPU model to its cluster's hosts as they are now, so it can
    /// run (migrate, restart) on all of them: it may see fewer CPU features
    /// from its next start. Only while it is stopped, and may migrate.
    #[serde(default)]
    fit_cpu: bool,
}

/// Changes a VM's settings. A running VM's memory changes at once, within
/// its maximum; everything else only while it is stopped: its definition
/// on the host is updated right away, and the VM picks it up when it next
/// starts. Growing its CPUs or (maximum) memory counts against the tenant's
/// quotas.
#[utoipa::path(patch, path = "/vms/{id}", tag = "vms", params(("id" = VmId, Path)), request_body = UpdateVm,
    responses((status = 200, body = VmView), (status = 403), (status = 404), (status = 409), (status = 503)))]
async fn update_vm(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
    Json(req): Json<UpdateVm>,
) -> ApiResult<Json<VmView>> {
    let vm = load(&state, &p, id, Perm::VmWrite).await?;
    let old = &vm.spec.0;
    let migratable = req.migratable.unwrap_or(old.migratable);
    let vcpus = req.vcpus.unwrap_or(old.vcpus);
    let memory_mib = req.memory_mib.unwrap_or(old.memory_mib);
    // Set beyond the maximum (while stopped), the maximum comes along.
    let max_vcpus = req
        .max_vcpus
        .unwrap_or_else(|| old.reserved_vcpus().max(vcpus));
    let max_memory_mib = req
        .max_memory_mib
        .unwrap_or_else(|| old.reserved_memory_mib().max(memory_mib));
    let template = req.template.unwrap_or(vm.template);
    let reporting = req.free_page_reporting.unwrap_or(old.free_page_reporting);
    if req.fit_cpu && !migratable {
        return Err(ApiError::bad_request(
            "the VM does not migrate: it has its host's CPU as is",
        ));
    }
    if !req.fit_cpu
        && (
            migratable,
            vcpus,
            max_vcpus,
            memory_mib,
            max_memory_mib,
            template,
            reporting,
        ) == (
            old.migratable,
            old.vcpus,
            old.reserved_vcpus(),
            old.memory_mib,
            old.reserved_memory_mib(),
            vm.template,
            old.free_page_reporting,
        )
    {
        return Ok(Json(view(vm, &context(&state).await?)));
    }
    check_size(vcpus, memory_mib)?;
    check_max_vcpus(vcpus, Some(max_vcpus))?;
    check_max_memory(memory_mib, Some(max_memory_mib))?;
    // Locked, so no power action slips in between the checks and the change.
    let mut tx = state.db.begin().await?;
    let vm = mst_db::vms::get_for_update(&mut tx, id).await?;
    let busy = mst_db::tasks::active_by_target(&mut *tx)
        .await?
        .into_iter()
        .any(|(t, _)| t == id.0);
    let host = vm
        .host_id
        .ok_or_else(|| ApiError::conflict("the VM is on no host"))?;
    let running = vm.power == "running" && vm.desired_power == "running";
    if vm.status == "ready" && !busy && running {
        let old = &vm.spec.0;
        if req.fit_cpu
            || (migratable, max_vcpus, max_memory_mib, template, reporting)
                != (
                    old.migratable,
                    old.reserved_vcpus(),
                    old.reserved_memory_mib(),
                    vm.template,
                    old.free_page_reporting,
                )
        {
            return Err(ApiError::conflict(format!(
                "stop {} first: only its CPUs and memory change while it runs, within what they may grow to",
                vm.name
            )));
        }
        return resize_live(&state, &p, tx, vm, host, vcpus, memory_mib).await;
    }
    if vm.status != "ready" || busy || vm.power != "stopped" || vm.desired_power != "stopped" {
        return Err(ApiError::conflict(format!(
            "stop {} first; this can only change while it is stopped",
            vm.name
        )));
    }
    let mut spec = vm.spec.0.clone();
    let grow_cpu = i64::from(max_vcpus) - i64::from(spec.reserved_vcpus());
    let grow_mem = max_memory_mib as i64 - spec.reserved_memory_mib() as i64;
    if grow_cpu > 0 || grow_mem > 0 {
        let delta: mst_db::quotas::Amounts = [("vcpus", grow_cpu), ("memory_mib", grow_mem)]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        mst_db::quotas::reserve(&mut tx, vm.tenant_id, &delta).await?;
        host_has_room(&mut tx, host, grow_cpu, grow_mem).await?;
    }
    spec.vcpus = vcpus;
    spec.max_vcpus = (max_vcpus > vcpus).then_some(max_vcpus);
    spec.memory_mib = memory_mib;
    spec.max_memory_mib = (max_memory_mib > memory_mib).then_some(max_memory_mib);
    if req.free_page_reporting.is_some() {
        spec.free_page_reporting = reporting;
    }
    let migratable_changed = migratable != spec.migratable;
    spec.migratable = migratable;
    let fitted = migratable && (migratable_changed || req.fit_cpu);
    if fitted {
        // The cluster's CPU model as it is now.
        spec.cpu = Some(crate::migration::baseline_cpu(&state, host).await.ok_or_else(|| {
            ApiError::unavailable("could not determine a CPU model the VM's cluster shares; is its host connected?")
        })?);
    }
    if template != vm.template {
        mst_db::vms::set_template(&mut *tx, id, template).await?;
    }
    let generation = mst_db::vms::redefine(&mut tx, id, &spec).await?;
    if fitted {
        // Worked out anew (below, or in the background).
        mst_db::vms::set_cpu_features(&mut *tx, id, None).await?;
    }
    define_on_host(&state, &mut tx, &vm, host, &spec, generation).await?;
    tx.commit().await?;
    if let (true, Some(cpu)) = (fitted, &spec.cpu)
        && let Ok(Some(needs)) = crate::cpu::expand(&state, host, cpu).await
    {
        mst_db::vms::set_cpu_features(&state.db, id, Some(&needs.features)).await?;
    }
    audit(
        &state.db,
        Some(&p),
        Some(vm.tenant_id),
        "vm.update",
        Some(("vm", id.0)),
        json!({"name": vm.name, "migratable": req.migratable, "vcpus": req.vcpus,
               "max_vcpus": req.max_vcpus, "memory_mib": req.memory_mib,
               "max_memory_mib": req.max_memory_mib, "template": req.template,
               "free_page_reporting": req.free_page_reporting, "fit_cpu": req.fit_cpu}),
    )
    .await;
    let vm = mst_db::vms::get(&state.db, id).await?;
    Ok(Json(view(vm, &context(&state).await?)))
}

/// Updates a VM's definition on its host to `spec` (a running VM takes it
/// up when it next starts).
pub async fn define_on_host(
    state: &AppState,
    tx: &mut mst_db::sqlx::PgConnection,
    vm: &mst_db::vms::Vm,
    host: mst_core::ids::HostId,
    spec: &VmSpec,
    generation: i64,
) -> ApiResult<()> {
    let define = definition(state, tx, vm, host, spec, generation).await?;
    state
        .agents
        .call(
            host,
            mst_proto::command::Cmd::DefineVm(define),
            std::time::Duration::from_secs(60),
        )
        .await
        .map_err(|e| ApiError::unavailable(format!("updating the VM on its host: {e}")))
}

/// The VM's definition with `spec`, as its host's agent takes it.
pub async fn definition(
    state: &AppState,
    tx: &mut mst_db::sqlx::PgConnection,
    vm: &mst_db::vms::Vm,
    host: mst_core::ids::HostId,
    spec: &VmSpec,
    generation: i64,
) -> ApiResult<mst_proto::DefineVm> {
    let mut pools = std::collections::BTreeMap::new();
    for d in &spec.disks {
        if let std::collections::btree_map::Entry::Vacant(e) = pools.entry(d.pool_id) {
            e.insert(
                super::agent_pool(&mst_db::storage_pools::get(&mut *tx, d.pool_id).await?)
                    .map_err(ApiError::internal)?,
            );
        }
    }
    Ok(mst_proto::DefineVm {
        vm_id: vm.id.to_string(),
        tenant_id: vm.tenant_id.to_string(),
        name: vm.name.clone(),
        generation: generation as u64,
        spec_json: serde_json::to_string(spec).map_err(ApiError::internal)?,
        pools: pools.into_values().collect(),
        disk_limits: crate::limits::disk_limits(&state.db, host).await?,
    })
}

/// What a running VM's balloon reports.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct MemoryView {
    /// What the guest has now, in MiB: the balloon's target, once reached.
    actual_mib: u64,
    /// The guest's balloon driver reports: its memory can change while it
    /// runs, and the figures below are known.
    driver: bool,
    /// Memory the guest sees.
    available_mib: Option<u64>,
    /// What it could use without swapping: free, and caches it can drop.
    usable_mib: Option<u64>,
    /// Entirely free.
    unused_mib: Option<u64>,
    /// What the VM takes on its host.
    rss_mib: Option<u64>,
}

async fn memory_of(
    state: &AppState,
    vm: &mst_db::vms::Vm,
    host: mst_core::ids::HostId,
) -> ApiResult<MemoryView> {
    state
        .agents
        .query::<MemoryView>(
            host,
            mst_proto::command::Cmd::MemoryStats(mst_proto::MemoryStats {
                vm_id: vm.id.to_string(),
            }),
            std::time::Duration::from_secs(30),
        )
        .await
        .map_err(|e| {
            ApiError::unavailable(format!("asking {}'s host about its memory: {e}", vm.name))
        })
}

/// A running VM's memory as its guest sees and uses it, from its balloon.
#[utoipa::path(get, path = "/vms/{id}/memory", tag = "vms", params(("id" = VmId, Path)),
    responses((status = 200, body = MemoryView), (status = 404), (status = 409), (status = 503)))]
async fn vm_memory(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
) -> ApiResult<Json<MemoryView>> {
    let vm = load(&state, &p, id, Perm::VmRead).await?;
    let host = vm
        .host_id
        .filter(|_| vm.power == "running")
        .ok_or_else(|| ApiError::conflict(format!("{} is not running", vm.name)))?;
    Ok(Json(memory_of(&state, &vm, host).await?))
}

/// Gives a running VM `vcpus` (plugged in or out) and `memory_mib` (through
/// its balloon), each within its maximum: kept for it on its host and
/// counted in quotas already.
async fn resize_live(
    state: &AppState,
    p: &Principal,
    mut tx: mst_db::sqlx::Transaction<'static, mst_db::sqlx::Postgres>,
    vm: mst_db::vms::Vm,
    host: mst_core::ids::HostId,
    vcpus: u32,
    memory_mib: u64,
) -> ApiResult<Json<VmView>> {
    let old = vm.spec.0.clone();
    let (max_vcpus, max_memory) = (old.reserved_vcpus(), old.reserved_memory_mib());
    if vcpus > max_vcpus {
        return Err(ApiError::conflict(format!(
            "{} can have {max_vcpus} vCPUs while it runs; stop it to give it more",
            vm.name
        )));
    }
    if memory_mib > max_memory {
        return Err(ApiError::conflict(format!(
            "{} can grow to {max_memory} MiB while it runs; stop it to give it more",
            vm.name
        )));
    }
    if vcpus != old.vcpus && old.max_vcpus.is_none() {
        return Err(ApiError::conflict(format!(
            "{}'s CPUs are not laid out for plugging; stop it to change them (or give it room to grow)",
            vm.name
        )));
    }
    if memory_mib != old.memory_mib && !memory_of(state, &vm, host).await?.driver {
        return Err(ApiError::conflict(format!(
            "{}'s guest has no balloon driver reporting (virtio_balloon), so its memory cannot change while it runs; stop it first",
            vm.name
        )));
    }
    let call = |cmd, what: &'static str| {
        let name = vm.name.clone();
        async move {
            state
                .agents
                .call(host, cmd, std::time::Duration::from_secs(60))
                .await
                .map_err(|e| ApiError::unavailable(format!("changing {name}'s {what}: {e}")))
        }
    };
    if vcpus != old.vcpus {
        let cmd = mst_proto::command::Cmd::SetVcpus(mst_proto::SetVcpus {
            vm_id: vm.id.to_string(),
            vcpus,
        });
        call(cmd, "CPUs").await?;
    }
    if memory_mib != old.memory_mib {
        let cmd = mst_proto::command::Cmd::SetMemory(mst_proto::SetMemory {
            vm_id: vm.id.to_string(),
            memory_mib,
        });
        call(cmd, "memory").await?;
    }
    let mut spec = old.clone();
    spec.vcpus = vcpus;
    // Kept, also with all plugged in: the running VM's CPUs stay laid out
    // for plugging, and its definition (where it migrates to) with them.
    spec.max_vcpus = old.max_vcpus;
    spec.memory_mib = memory_mib;
    spec.max_memory_mib = (max_memory > memory_mib).then_some(max_memory);
    let generation = mst_db::vms::redefine(&mut tx, vm.id, &spec).await?;
    define_on_host(state, &mut tx, &vm, host, &spec, generation).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(p),
        Some(vm.tenant_id),
        "vm.update",
        Some(("vm", vm.id.0)),
        json!({"name": vm.name, "vcpus": vcpus, "memory_mib": memory_mib,
               "from": {"vcpus": old.vcpus, "memory_mib": old.memory_mib}, "live": true}),
    )
    .await;
    let vm = mst_db::vms::get(&state.db, vm.id).await?;
    Ok(Json(view(vm, &context(state).await?)))
}

#[derive(Deserialize, ToSchema)]
pub struct ResizeDisk {
    /// The new size: larger than the disk is now (disks only grow), at most
    /// 16384.
    size_gib: u64,
}

/// Grows one of a VM's disks (0 is the boot disk), running or stopped. A
/// running guest sees the larger disk at once; its partition and file
/// system grow when it next boots (cloud-init does that), or by hand. The
/// growth counts against the tenant's disk quota. Disks on local or NFS
/// storage cannot grow while the VM has snapshots.
#[utoipa::path(post, path = "/vms/{id}/disks/{index}/resize", tag = "vms",
    params(("id" = VmId, Path), ("index" = usize, Path)), request_body = ResizeDisk,
    responses((status = 200, body = VmView), (status = 400), (status = 404), (status = 409), (status = 503)))]
async fn resize_disk(
    State(state): State<AppState>,
    p: Principal,
    Path((id, index)): Path<(VmId, usize)>,
    Json(req): Json<ResizeDisk>,
) -> ApiResult<Json<VmView>> {
    grow_disk(&state, &p, id, Which::Index(index), req.size_gib).await
}

/// Which of a VM's disks.
pub(crate) enum Which {
    /// By its place (0 is the boot disk).
    Index(usize),
    /// By its volume.
    Volume(VolumeId),
}

/// Grows one of the VM's disks to `size_gib` (see `resize_disk`).
pub(crate) async fn grow_disk(
    state: &AppState,
    p: &Principal,
    id: VmId,
    which: Which,
    size_gib: u64,
) -> ApiResult<Json<VmView>> {
    load(state, p, id, Perm::VmWrite).await?;
    // Locked, so no power action or other change slips in meanwhile; the
    // disk is found (and its size checked) in what is locked.
    let mut tx = state.db.begin().await?;
    let vm = mst_db::vms::get_for_update(&mut tx, id).await?;
    let index = match which {
        Which::Index(i) => i,
        Which::Volume(v) => vm
            .spec
            .0
            .disks
            .iter()
            .position(|d| d.volume_id == v)
            .ok_or_else(|| ApiError::not_found("disk"))?,
    };
    let disk = vm
        .spec
        .0
        .disks
        .get(index)
        .ok_or_else(|| ApiError::not_found("disk"))?;
    let size = size_gib.saturating_mul(GIB);
    if size_gib > 16384 || size <= disk.size_bytes {
        return Err(ApiError::bad_request(format!(
            "the disk is {} GiB: it can grow, to at most 16384 GiB",
            disk.size_bytes.div_ceil(GIB)
        )));
    }
    if vm.status != "ready" {
        return Err(ApiError::conflict(format!(
            "the VM is {}: resize it afterwards",
            vm.status
        )));
    }
    if vm.power != vm.desired_power {
        return Err(ApiError::conflict(
            "the VM is starting or stopping: resize it once it is done",
        ));
    }
    super::check_disks_idle(&mut tx, id).await?;
    let host = vm
        .host_id
        .ok_or_else(|| ApiError::conflict("the VM is on no host"))?;
    let mut spec = vm.spec.0.clone();
    let disk = &mut spec.disks[index];
    let pool = mst_db::storage_pools::get(&mut *tx, disk.pool_id).await?;
    if pool.kind != "rbd" && mst_db::snapshots::count(&mut *tx, id).await? > 0 {
        return Err(ApiError::conflict(
            "the VM has snapshots, and qcow2 disks cannot grow with snapshots in them; delete them first",
        ));
    }
    let delta: mst_db::quotas::Amounts = [(
        "disk_gib".to_owned(),
        (size - disk.size_bytes).div_ceil(GIB) as i64,
    )]
    .into_iter()
    .collect();
    mst_db::quotas::reserve(&mut tx, vm.tenant_id, &delta).await?;
    disk.size_bytes = size;
    let volume = disk.volume_id;
    mst_db::vms::set_spec(&mut *tx, id, &spec).await?;
    mst_db::vms::set_volume_size(&mut *tx, volume, size as i64).await?;
    let cmd = mst_proto::ResizeDisk {
        vm_id: id.to_string(),
        disk: Some(mst_proto::BackupDisk {
            index: index as u32,
            volume_id: volume.to_string(),
            pool: Some(super::agent_pool(&pool).map_err(ApiError::internal)?),
            target: spec.disk_slots()[index].target.clone(),
            parent_index: None,
        }),
        size_bytes: size,
    };
    state
        .agents
        .call(
            host,
            mst_proto::command::Cmd::ResizeDisk(cmd),
            std::time::Duration::from_secs(120),
        )
        .await
        .map_err(|e| ApiError::unavailable(format!("growing the disk on its host: {e}")))?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(p),
        Some(vm.tenant_id),
        "vm.resize_disk",
        Some(("vm", id.0)),
        json!({"name": vm.name, "disk": index, "size_gib": size_gib}),
    )
    .await;
    let vm = mst_db::vms::get(&state.db, id).await?;
    Ok(Json(view(vm, &context(state).await?)))
}

fn check_max_memory(memory_mib: u64, max: Option<u64>) -> ApiResult<()> {
    match max {
        Some(m) if m < memory_mib || m > 4 * 1024 * 1024 => Err(ApiError::bad_request(
            "max_memory_mib must be at least memory_mib, and at most 4194304",
        )),
        _ => Ok(()),
    }
}

fn check_max_vcpus(vcpus: u32, max: Option<u32>) -> ApiResult<()> {
    match max {
        Some(m) if m < vcpus || m > 128 => Err(ApiError::bad_request(
            "max_vcpus must be at least vcpus, and at most 128",
        )),
        _ => Ok(()),
    }
}

/// What a new VM's host keeps for it: its CPUs, or what they may grow to.
fn reserved_vcpus(req: &CreateVm) -> u32 {
    req.max_vcpus.unwrap_or(0).max(req.vcpus)
}

/// What a new VM's host keeps for it: its memory, or what that may grow to.
fn reserved_memory(req: &CreateVm) -> u64 {
    req.max_memory_mib.unwrap_or(0).max(req.memory_mib)
}

fn check_size(vcpus: u32, memory_mib: u64) -> ApiResult<()> {
    if !(1..=128).contains(&vcpus) {
        return Err(ApiError::bad_request("vcpus must be between 1 and 128"));
    }
    if !(128..=4 * 1024 * 1024).contains(&memory_mib) {
        return Err(ApiError::bad_request(
            "memory_mib must be between 128 and 4194304",
        ));
    }
    Ok(())
}

/// Whether a VM's host can give it `vcpus` more CPUs and `memory_mib` more
/// memory, within its cluster's overcommit.
async fn host_has_room(
    tx: &mut mst_db::sqlx::PgConnection,
    host: HostId,
    vcpus: i64,
    memory_mib: i64,
) -> ApiResult<()> {
    let hosts = mst_db::clusters::candidates_locked(tx).await?;
    let h = hosts
        .iter()
        .find(|h| h.id == host)
        .ok_or_else(|| ApiError::conflict("the VM's host no longer exists"))?;
    let free_cpu = f64::from(h.cpu_count) * f64::from(h.cpu_overcommit) - h.used_vcpus as f64;
    if vcpus > 0 && free_cpu < vcpus as f64 {
        return Err(ApiError::conflict(format!(
            "{} has only {:.1} vCPUs free",
            h.name,
            free_cpu.max(0.0)
        )));
    }
    let free_mem = h.memory_mib as f64 * f64::from(h.memory_overcommit) - h.used_memory_mib as f64;
    if memory_mib > 0 && free_mem < memory_mib as f64 {
        return Err(ApiError::conflict(format!(
            "{} has only {} MiB of memory free",
            h.name,
            free_mem.max(0.0).floor()
        )));
    }
    Ok(())
}

#[derive(Deserialize, ToSchema)]
pub struct MigrateVm {
    /// Destination; the scheduler picks one in the VM's cluster otherwise.
    host_id: Option<HostId>,
}

/// Live-migrates a running VM to another host of its cluster, without
/// stopping it. Disks on shared storage stay put; local disks are copied.
/// Moving VMs between hosts is the provider's business.
#[utoipa::path(post, path = "/vms/{id}/migrate", tag = "vms", params(("id" = VmId, Path)), request_body = MigrateVm,
    responses((status = 202, body = TaskRef), (status = 403), (status = 404), (status = 409)))]
async fn migrate_vm(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
    Json(req): Json<MigrateVm>,
) -> ApiResult<(StatusCode, Json<TaskRef>)> {
    let vm = load(&state, &p, id, Perm::VmRead).await?;
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let mut tx = state.db.begin().await?;
    let task = enqueue(&mut tx, &p, vm.tenant_id, "vm.migrate", id, json!({})).await?;
    let (dest, name) = crate::migration::plan(&mut tx, &vm, req.host_id, &[], task)
        .await
        .map_err(|e| match e {
            crate::migration::Refusal::Refused(m) => ApiError::conflict(m),
            crate::migration::Refusal::Db(e) => e.into(),
        })?;
    mst_db::sqlx::query(
        "UPDATE tasks SET params = jsonb_build_object('dest_host_id', $2::uuid) WHERE id = $1",
    )
    .bind(task)
    .bind(dest)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(vm.tenant_id),
        "vm.migrate",
        Some(("vm", id.0)),
        json!({"name": vm.name, "from": vm.host_id, "to": dest, "to_name": name}),
    )
    .await;
    Ok((StatusCode::ACCEPTED, Json(TaskRef { task_id: task })))
}

/// Asks a migration in progress to stop. The VM stays where it was.
#[utoipa::path(post, path = "/vms/{id}/migrate/cancel", tag = "vms", params(("id" = VmId, Path)),
    responses((status = 204), (status = 403), (status = 404), (status = 409)))]
async fn cancel_migration(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
) -> ApiResult<StatusCode> {
    let vm = load(&state, &p, id, Perm::VmRead).await?;
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    if !mst_db::migrations::request_cancel(&state.db, id).await? {
        return Err(ApiError::conflict("the VM is not being migrated"));
    }
    audit(
        &state.db,
        Some(&p),
        Some(vm.tenant_id),
        "vm.migrate.cancel",
        Some(("vm", id.0)),
        json!({"name": vm.name}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
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
        let ud = user_data(&req).unwrap();
        assert!(ud.starts_with("#cloud-config\n"));
        assert!(!ud.contains("p'w"), "plain-text password leaked: {ud}");
        let body: serde_json::Value = serde_json::from_str(ud.lines().nth(1).unwrap()).unwrap();
        let hash = body["password"].as_str().unwrap();
        assert!(hash.starts_with("$6$"), "{hash}");
        use sha_crypt::{PasswordVerifier, ShaCrypt};
        ShaCrypt::SHA512.verify_password(b"p'w\"d", hash).unwrap();
        assert!(ShaCrypt::SHA512.verify_password(b"wrong", hash).is_err());
        assert_eq!(body["chpasswd"]["expire"], false);
        assert_eq!(body["ssh_authorized_keys"][0], "ssh-ed25519 AAAA x");
    }

    /// Reference output of `openssl passwd -6` (crypt(3)'s SHA-512 scheme) for
    /// this password and salt: guests must be able to verify what we generate.
    #[test]
    fn sha512_crypt_matches_libc() {
        assert_eq!(
            sha512_crypt("Hello world!", b"metastratum!").unwrap(),
            "$6$rounds=5000$hJ4RVB5Rm34RppK6$V5QES7BRcuwtDfYoOcQWdBCThBnMoq3.nK1ouMPE2CmugyxX3ZuH4PW4royh6WYVGz/6m6o797VVeEcNAoFSJ1"
        );
        let random = hash_console_password("x").unwrap();
        assert_eq!(random.split('$').nth(3).unwrap().len(), 16, "{random}");
    }

    #[test]
    fn names_are_validated() {
        let mk = |name: &str| -> CreateVm {
            serde_json::from_value(json!({"name": name, "vcpus": 1, "memory_mib": 512, "disk_gib": 10, "image_id": ImageId::new()})).unwrap()
        };
        assert!(validate(&mk("web-1"), true).is_ok());
        for bad in ["", "-a", "a-", "a_b", "a.b", &"x".repeat(64)] {
            assert!(validate(&mk(bad), true).is_err(), "{bad:?}");
        }
    }
}
