use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use hm_core::ids::{
    ClusterId, ExternalIpId, HostId, ImageId, LOCAL_POOL, NetworkId, NicId, SecurityGroupId,
    StoragePoolId, TaskId, TenantId, VmId, VolumeId,
};
use hm_core::vm::{CloudInit, DiskSpec, Firmware, NicSpec, VmSpec, mac_for};
use hm_db::vms::Vm;
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
    vcpus: u32,
    memory_mib: u64,
    firmware: Firmware,
    /// Can live-migrate; otherwise it has its host's CPU and disk caching.
    migratable: bool,
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
    migrations: HashMap<VmId, hm_db::migrations::Migration>,
}

async fn context(state: &AppState) -> ApiResult<Context> {
    type NicRow = (
        NicId,
        Option<SecurityGroupId>,
        Option<ExternalIpId>,
        Option<String>,
    );
    let nic_rows: Vec<NicRow> = hm_db::sqlx::query_as(
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
    let networks: Vec<(NetworkId, String)> = hm_db::sqlx::query_as("SELECT id, name FROM networks")
        .fetch_all(&state.db)
        .await?;
    let migrations: Vec<hm_db::migrations::Migration> = hm_db::sqlx::query_as(
        "SELECT vm_id, task_id, source_host_id, dest_host_id, state, cancel_requested, started_at FROM migrations",
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Context {
        migrations: migrations.into_iter().map(|m| (m.vm_id, m)).collect(),
        networks: networks.into_iter().collect(),
        nic_groups,
        floating,
        hosts: hm_db::hosts::list(&state.db)
            .await?
            .into_iter()
            .map(|h| (h.id, (h.name.clone(), h.is_live())))
            .collect(),
        tasks: hm_db::tasks::active_by_target(&state.db)
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
        desired_power: vm.desired_power,
        power: vm.power,
        power_reason: vm.power_reason,
        power_observed_at: vm.power_observed_at,
        active_task: ctx.tasks.get(&vm.id.0).cloned(),
        error: vm.error,
        vcpus: spec.vcpus,
        memory_mib: spec.memory_mib,
        firmware: spec.firmware,
        migratable: spec.migratable,
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
    let vm = hm_db::vms::get(&state.db, id).await?;
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
    let vms = hm_db::vms::list(&state.db, tenant, q.descendants).await?;
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
fn user_data(req: &CreateVm) -> ApiResult<String> {
    let mut doc = serde_json::Map::new();
    if !req.ssh_authorized_keys.is_empty() {
        doc.insert("ssh_authorized_keys".into(), json!(req.ssh_authorized_keys));
    }
    if let Some(pw) = &req.password {
        doc.insert("password".into(), json!(hash_console_password(pw)?));
        doc.insert("chpasswd".into(), json!({"expire": false}));
    }
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
    validate(&req)?;
    let tenant = req.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::VmWrite, tenant).await?;
    if !hm_db::images::is_usable_by(&state.db, req.image_id, tenant).await? {
        return Err(ApiError::not_found("image"));
    }
    let pool =
        hm_db::storage_pools::get(&state.db, req.storage_pool_id.unwrap_or(LOCAL_POOL)).await?;
    if !super::storage_pools::may_use(&state, tenant, &pool).await? {
        return Err(ApiError::not_found("storage pool"));
    }

    let mut tx = state.db.begin().await?;
    let host = place(&state, &mut tx, tenant, &req, &pool).await?;
    let delta: hm_db::quotas::Amounts = [
        ("vms", 1),
        ("vcpus", i64::from(req.vcpus)),
        ("memory_mib", req.memory_mib as i64),
        ("disk_gib", req.disk_gib as i64),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    hm_db::quotas::reserve(&mut tx, tenant, &delta).await?;
    let network = match req.network_id {
        Some(id) => {
            let n = hm_db::networks::get(&mut *tx, id).await?;
            if n.tenant_id != tenant {
                return Err(ApiError::not_found("network"));
            }
            n
        }
        None => hm_db::networks::default_for(&mut tx, tenant).await?,
    };
    let groups = match &req.security_group_ids {
        Some(ids) => {
            check_groups(&mut tx, tenant, ids).await?;
            ids.clone()
        }
        None => vec![hm_db::netsec::default_group(&mut tx, tenant).await?.id],
    };

    let vm_id = VmId::new();
    let volume_id = VolumeId::new();
    let disk = DiskSpec {
        volume_id,
        pool_id: pool.id,
        size_bytes: req.disk_gib * GIB,
        image_id: Some(req.image_id),
    };
    // A VM whose cluster is a single host has nowhere to migrate to; let it
    // use everything the host offers instead.
    let migratable = match req.migratable {
        Some(m) => m,
        None => match host.cluster {
            Some(c) => hm_db::sqlx::query_scalar::<_, i64>(
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
        vcpus: req.vcpus,
        memory_mib: req.memory_mib,
        firmware: req.firmware,
        disks: vec![disk.clone()],
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
            // Only local volumes are tied to a host.
            host_id: (pool.kind == "local").then_some(host.id),
            pool_id: pool.id,
            size_bytes: disk.size_bytes as i64,
            image_id: Some(req.image_id),
        },
    )
    .await?;
    let nic_id = NicId::new();
    let nic =
        hm_db::networks::create_nic(&mut tx, nic_id, vm_id, network.id, &mac_for(nic_id)).await?;
    hm_db::networks::set_nic_groups(&mut tx, nic_id, &groups).await?;
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
    audit(
        &state.db,
        Some(&p),
        Some(tenant),
        "vm.create",
        Some(("vm", vm_id.0)),
        json!({"name": req.name, "vcpus": req.vcpus, "memory_mib": req.memory_mib, "disk_gib": req.disk_gib, "host": host.name}),
    )
    .await;
    let vm = hm_db::vms::get(&state.db, vm_id).await?;
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
    tx: &mut hm_db::sqlx::PgConnection,
    tenant: TenantId,
    ids: &[SecurityGroupId],
) -> ApiResult<()> {
    for id in ids {
        let g = hm_db::netsec::get_group(&mut *tx, *id).await?;
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
    hm_db::networks::set_nic_groups(&mut tx, nic, &groups).await?;
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
    let vm = hm_db::vms::get(&state.db, id).await?;
    Ok(Json(view(vm, &context(&state).await?)))
}

/// Picks a host for a new VM. Hosts are locked until the transaction ends,
/// so concurrent creations see each other's allocations.
async fn place(
    state: &AppState,
    tx: &mut hm_db::sqlx::PgConnection,
    tenant: TenantId,
    req: &CreateVm,
    pool: &hm_db::storage_pools::Pool,
) -> ApiResult<hm_scheduler::Host> {
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
    let hosts: Vec<hm_scheduler::Host> = hm_db::clusters::candidates_locked(tx)
        .await?
        .into_iter()
        .map(|c| hm_scheduler::Host {
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
    let request = hm_scheduler::Request {
        vcpus: req.vcpus,
        memory_mib: req.memory_mib,
        clusters,
        host: None,
    };
    let id =
        hm_scheduler::place(&request, &hosts).map_err(|e| ApiError::unavailable(e.to_string()))?;
    Ok(hosts
        .into_iter()
        .find(|h| h.id == id)
        .expect("placed on a candidate"))
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
    if vm.status == "migrating" {
        return Err(ApiError::conflict(
            "VM is being migrated; wait, or cancel the migration",
        ));
    }
    let mut tx = state.db.begin().await?;
    if let Some(d) = desired {
        hm_db::vms::set_desired_power(&mut *tx, id, d).await?;
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

#[utoipa::path(delete, path = "/vms/{id}", tag = "vms", params(("id" = VmId, Path)),
    responses((status = 202, body = TaskRef), (status = 404)))]
async fn delete_vm(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
) -> ApiResult<(StatusCode, Json<TaskRef>)> {
    let vm = load(&state, &p, id, Perm::VmWrite).await?;
    if vm.status == "migrating" {
        return Err(ApiError::conflict(
            "VM is being migrated; cancel the migration first",
        ));
    }
    let mut tx = state.db.begin().await?;
    hm_db::vms::set_status(&mut *tx, id, "deleting", None).await?;
    let task_id = enqueue(&mut tx, &p, vm.tenant_id, "vm.delete", id, json!({})).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(vm.tenant_id),
        "vm.delete",
        Some(("vm", id.0)),
        json!({"name": vm.name}),
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
}

/// Changes a stopped VM's settings. Its definition on the host is updated
/// right away; the VM picks it up when it next starts.
#[utoipa::path(patch, path = "/vms/{id}", tag = "vms", params(("id" = VmId, Path)), request_body = UpdateVm,
    responses((status = 200, body = VmView), (status = 403), (status = 404), (status = 409), (status = 503)))]
async fn update_vm(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VmId>,
    Json(req): Json<UpdateVm>,
) -> ApiResult<Json<VmView>> {
    let vm = load(&state, &p, id, Perm::VmWrite).await?;
    let Some(migratable) = req.migratable else {
        return Ok(Json(view(vm, &context(&state).await?)));
    };
    if migratable == vm.spec.0.migratable {
        return Ok(Json(view(vm, &context(&state).await?)));
    }
    // Locked, so no power action slips in between the checks and the change.
    let mut tx = state.db.begin().await?;
    let vm = hm_db::vms::get_for_update(&mut tx, id).await?;
    let busy = hm_db::tasks::active_by_target(&mut *tx)
        .await?
        .into_iter()
        .any(|(t, _)| t == id.0);
    if vm.status != "ready" || busy || vm.power != "stopped" || vm.desired_power != "stopped" {
        return Err(ApiError::conflict(format!(
            "stop {} first; this can only change while it is stopped",
            vm.name
        )));
    }
    let host = vm
        .host_id
        .ok_or_else(|| ApiError::conflict("the VM is on no host"))?;
    let mut spec = vm.spec.0.clone();
    spec.migratable = migratable;
    if migratable {
        // The cluster's CPU model as it is now.
        spec.cpu = Some(crate::migration::baseline_cpu(&state, host).await.ok_or_else(|| {
            ApiError::unavailable("could not determine a CPU model the VM's cluster shares; is its host connected?")
        })?);
    }
    let generation = hm_db::vms::redefine(&mut tx, id, &spec).await?;
    let mut pools = std::collections::BTreeMap::new();
    for d in &spec.disks {
        if let std::collections::btree_map::Entry::Vacant(e) = pools.entry(d.pool_id) {
            e.insert(super::agent_pool(
                &hm_db::storage_pools::get(&mut *tx, d.pool_id).await?,
            ).map_err(ApiError::internal)?);
        }
    }
    let define = hm_proto::DefineVm {
        vm_id: id.to_string(),
        tenant_id: vm.tenant_id.to_string(),
        name: vm.name.clone(),
        generation: generation as u64,
        spec_json: serde_json::to_string(&spec).map_err(ApiError::internal)?,
        pools: pools.into_values().collect(),
    };
    state
        .agents
        .call(
            host,
            hm_proto::command::Cmd::DefineVm(define),
            std::time::Duration::from_secs(60),
        )
        .await
        .map_err(|e| ApiError::unavailable(format!("updating the VM on its host: {e}")))?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(vm.tenant_id),
        "vm.update",
        Some(("vm", id.0)),
        json!({"name": vm.name, "migratable": migratable}),
    )
    .await;
    let vm = hm_db::vms::get(&state.db, id).await?;
    Ok(Json(view(vm, &context(&state).await?)))
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
    hm_db::sqlx::query(
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
    if !hm_db::migrations::request_cancel(&state.db, id).await? {
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

    /// Reference output of libxcrypt's crypt(3) for this password and salt:
    /// guests must be able to verify what we generate.
    #[test]
    fn sha512_crypt_matches_libc() {
        assert_eq!(
            sha512_crypt("Hello world!", b"hypermon-tst").unwrap(),
            "$6$rounds=5000$cZ5QZ7LPjtK9oB5R$UAT4RtNALKwwMv9mrWoeiJ9W.foUnFZ2.6CPksejgVrFtT8PPnb.0XpYumaWmlz25PQm5bfMLlL3ISB.T.c6o0"
        );
        let random = hash_console_password("x").unwrap();
        assert_eq!(random.split('$').nth(3).unwrap().len(), 16, "{random}");
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
