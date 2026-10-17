//! Load balancers: a virtual IP in a VPC whose listeners spread connections
//! over member VMs, done by OVN in the VPC's router and switches.

use std::collections::HashSet;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use mst_core::ids::{LoadBalancerId, NetworkId, NicId, TenantId, VmId, VpcId};
use mst_db::sqlx::PgConnection;
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
        .routes(routes!(list_load_balancers, create_load_balancer))
        .routes(routes!(
            get_load_balancer,
            update_load_balancer,
            delete_load_balancer
        ))
}

/// How many listeners a load balancer may have.
const MAX_LISTENERS: usize = 32;

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Defaults to the caller's tenant.
    tenant_id: Option<TenantId>,
    /// Include sub-tenants' load balancers.
    #[serde(default)]
    descendants: bool,
}

#[derive(Clone, Serialize, Deserialize, ToSchema)]
pub struct ListenerSpec {
    /// `tcp` or `udp`.
    protocol: String,
    /// The port on the virtual IP.
    port: u16,
    /// The members' port; the same as `port` by default.
    target_port: Option<u16>,
}

#[derive(Serialize, ToSchema)]
pub struct MemberView {
    vm_id: VmId,
    vm_name: String,
    nic_id: NicId,
    ip: String,
}

#[derive(Serialize, ToSchema)]
pub struct LoadBalancerView {
    id: LoadBalancerId,
    tenant_id: TenantId,
    vpc_id: VpcId,
    vpc_name: String,
    name: String,
    /// The virtual IP clients connect to.
    ip: String,
    /// The virtual IP is on the VPC's external network, reachable from
    /// outside it; otherwise it is in `network_id`, for the VPC's VMs.
    external: bool,
    network_id: Option<NetworkId>,
    /// OVN checks members and leaves out those that do not answer.
    health_check: bool,
    listeners: Vec<ListenerSpec>,
    members: Vec<MemberView>,
    created_at: DateTime<Utc>,
}

async fn view(
    db: &mut PgConnection,
    lb: mst_db::load_balancers::LoadBalancer,
) -> ApiResult<LoadBalancerView> {
    let vpc = mst_db::networks::get_vpc(&mut *db, lb.vpc_id).await?;
    let listeners = mst_db::load_balancers::listeners(&mut *db, lb.id)
        .await?
        .into_iter()
        .map(|l| ListenerSpec {
            protocol: l.protocol,
            port: l.port as u16,
            target_port: Some(l.target_port as u16),
        })
        .collect();
    let mut members = vec![];
    for nic in mst_db::load_balancers::members(&mut *db, lb.id).await? {
        let n = mst_db::networks::get_nic(&mut *db, nic).await?;
        let vm = mst_db::vms::get(&mut *db, n.vm_id).await?;
        members.push(MemberView {
            vm_id: vm.id,
            vm_name: vm.name,
            nic_id: n.id,
            ip: n.ip,
        });
    }
    members.sort_by(|a, b| a.vm_name.cmp(&b.vm_name));
    Ok(LoadBalancerView {
        id: lb.id,
        tenant_id: lb.tenant_id,
        vpc_id: lb.vpc_id,
        vpc_name: vpc.name,
        name: lb.name,
        ip: lb.ip,
        external: lb.external_ip_id.is_some(),
        network_id: lb.network_id,
        health_check: lb.health_check,
        listeners,
        members,
        created_at: lb.created_at,
    })
}

#[utoipa::path(get, path = "/load-balancers", tag = "networks", params(ListQuery),
    responses((status = 200, body = Vec<LoadBalancerView>)))]
async fn list_load_balancers(
    State(state): State<AppState>,
    p: Principal,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<LoadBalancerView>>> {
    let tenant = q.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::NetworkRead, tenant).await?;
    let mut db = state.db.acquire().await?;
    let mut out = vec![];
    for lb in mst_db::load_balancers::in_subtrees(&mut *db, &[tenant]).await? {
        if q.descendants || lb.tenant_id == tenant {
            out.push(view(&mut db, lb).await?);
        }
    }
    Ok(Json(out))
}

#[utoipa::path(get, path = "/load-balancers/{id}", tag = "networks", params(("id" = LoadBalancerId, Path)),
    responses((status = 200, body = LoadBalancerView), (status = 404)))]
async fn get_load_balancer(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<LoadBalancerId>,
) -> ApiResult<Json<LoadBalancerView>> {
    let lb = mst_db::load_balancers::get(&state.db, id).await?;
    authorize_resource(
        &state.db,
        &p,
        Perm::NetworkRead,
        lb.tenant_id,
        "load balancer",
    )
    .await?;
    let mut db = state.db.acquire().await?;
    Ok(Json(view(&mut db, lb).await?))
}

fn check_name(name: &str) -> ApiResult<&str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 64 || name.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "a load balancer's name is 1-64 characters, on one line",
        ));
    }
    Ok(name)
}

fn check_listeners(specs: &[ListenerSpec]) -> ApiResult<Vec<mst_db::load_balancers::Listener>> {
    if specs.len() > MAX_LISTENERS {
        return Err(ApiError::bad_request(format!(
            "a load balancer has at most {MAX_LISTENERS} listeners"
        )));
    }
    let mut seen = HashSet::new();
    let mut out = vec![];
    for l in specs {
        if l.protocol != "tcp" && l.protocol != "udp" {
            return Err(ApiError::bad_request("a listener's protocol is tcp or udp"));
        }
        let target = l.target_port.unwrap_or(l.port);
        if l.port == 0 || target == 0 {
            return Err(ApiError::bad_request("ports are 1-65535"));
        }
        if !seen.insert((l.protocol.clone(), l.port)) {
            return Err(ApiError::bad_request(format!(
                "two listeners on {} port {}",
                l.protocol, l.port
            )));
        }
        out.push(mst_db::load_balancers::Listener {
            protocol: l.protocol.clone(),
            port: i32::from(l.port),
            target_port: i32::from(target),
        });
    }
    Ok(out)
}

/// The members' NICs in the VPC, and the networks they are in. Each VM must
/// be the tenant's and have a NIC in the VPC.
async fn member_nics(
    tx: &mut PgConnection,
    tenant: TenantId,
    vpc: VpcId,
    vms: &[VmId],
) -> ApiResult<Vec<(NicId, NetworkId)>> {
    let mut out = vec![];
    for id in vms.iter().collect::<HashSet<_>>() {
        let vm = mst_db::vms::get(&mut *tx, *id).await?;
        if vm.tenant_id != tenant {
            return Err(ApiError::not_found("VM"));
        }
        let mut found = None;
        for nic in mst_db::networks::nics_for_vm(&mut *tx, vm.id).await? {
            let net = mst_db::networks::get(&mut *tx, nic.network_id).await?;
            if net.vpc_id == vpc {
                found = Some((nic.id, net.id));
                break;
            }
        }
        out.push(found.ok_or_else(|| {
            ApiError::bad_request(format!(
                "{} has no network interface in the load balancer's VPC",
                vm.name
            ))
        })?);
    }
    Ok(out)
}

/// Gives each members' network an address for health checks to come from.
async fn monitor_addresses(tx: &mut PgConnection, members: &[(NicId, NetworkId)]) -> ApiResult<()> {
    for net in members.iter().map(|m| m.1).collect::<HashSet<_>>() {
        mst_db::networks::monitor_address(&mut *tx, net).await?;
    }
    Ok(())
}

#[derive(Deserialize, ToSchema)]
pub struct CreateLoadBalancer {
    /// Defaults to the caller's tenant.
    tenant_id: Option<TenantId>,
    name: String,
    vpc_id: VpcId,
    /// Put the virtual IP in this network of the VPC, for its VMs only. By
    /// default it is on the VPC's external network, reachable from outside.
    network_id: Option<NetworkId>,
    /// Check members and leave out those that do not answer (default true).
    health_check: Option<bool>,
    #[serde(default)]
    listeners: Vec<ListenerSpec>,
    /// VMs to spread connections over, each with a NIC in the VPC.
    #[serde(default)]
    member_vm_ids: Vec<VmId>,
}

/// Creates a load balancer. Connections to a listener's port on its virtual
/// IP go to one of the members, at the listener's target port. Members'
/// security groups must let the clients in; OVN keeps the clients' address.
#[utoipa::path(post, path = "/load-balancers", tag = "networks", request_body = CreateLoadBalancer,
    responses((status = 201, body = LoadBalancerView), (status = 400), (status = 403), (status = 404),
        (status = 409, description = "Over quota, or no address left")))]
async fn create_load_balancer(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<CreateLoadBalancer>,
) -> ApiResult<(StatusCode, Json<LoadBalancerView>)> {
    let tenant = req.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::NetworkWrite, tenant).await?;
    let name = check_name(&req.name)?;
    let listeners = check_listeners(&req.listeners)?;
    let health_check = req.health_check.unwrap_or(true);
    let vpc = mst_db::networks::get_vpc(&state.db, req.vpc_id).await?;
    if vpc.tenant_id != tenant {
        return Err(ApiError::not_found("VPC"));
    }

    let mut tx = state.db.begin().await?;
    let delta = [("load_balancers".to_owned(), 1)].into_iter().collect();
    mst_db::quotas::reserve(&mut tx, tenant, &delta).await?;
    let (external_ip_id, network_id, ip) = match req.network_id {
        Some(net) => {
            let n = mst_db::networks::get(&mut *tx, net).await?;
            if n.vpc_id != vpc.id {
                return Err(ApiError::bad_request("the network is not in the VPC"));
            }
            let ip = mst_db::networks::reserve_address(&mut tx, net, "lb-vip").await?;
            (None, Some(net), ip)
        }
        None => {
            let ext = vpc.external_network_id.ok_or_else(|| {
                ApiError::bad_request(format!(
                    "VPC {} has no external network: connect it to one, or put the address in one of its networks",
                    vpc.name
                ))
            })?;
            let ip = mst_db::external::allocate_for_load_balancer(&mut tx, ext, tenant).await?;
            let addr = ip.ip.parse().map_err(ApiError::internal)?;
            (Some(ip.id), None, addr)
        }
    };
    let id = LoadBalancerId::new();
    mst_db::load_balancers::create(
        &mut *tx,
        &mst_db::load_balancers::NewLoadBalancer {
            id,
            tenant_id: tenant,
            vpc_id: vpc.id,
            name,
            external_ip_id,
            network_id,
            ip,
            health_check,
        },
    )
    .await?;
    let members = member_nics(&mut tx, tenant, vpc.id, &req.member_vm_ids).await?;
    if health_check {
        monitor_addresses(&mut tx, &members).await?;
    }
    let nics: Vec<NicId> = members.iter().map(|m| m.0).collect();
    mst_db::load_balancers::update(&mut tx, id, name, health_check, &listeners, &nics).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(tenant),
        "load_balancer.create",
        Some(("load_balancer", id.0)),
        json!({"name": name, "ip": ip.to_string(), "vpc": vpc.name}),
    )
    .await;
    state.net.kick();
    let lb = mst_db::load_balancers::get(&state.db, id).await?;
    let mut db = state.db.acquire().await?;
    Ok((StatusCode::CREATED, Json(view(&mut db, lb).await?)))
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateLoadBalancer {
    name: String,
    health_check: bool,
    /// Replaces the listeners.
    listeners: Vec<ListenerSpec>,
    /// Replaces the members.
    member_vm_ids: Vec<VmId>,
}

/// Changes a load balancer's name, health checks, listeners and members.
/// Its virtual IP stays.
#[utoipa::path(put, path = "/load-balancers/{id}", tag = "networks", params(("id" = LoadBalancerId, Path)),
    request_body = UpdateLoadBalancer,
    responses((status = 200, body = LoadBalancerView), (status = 400), (status = 403), (status = 404), (status = 409)))]
async fn update_load_balancer(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<LoadBalancerId>,
    Json(req): Json<UpdateLoadBalancer>,
) -> ApiResult<Json<LoadBalancerView>> {
    let lb = mst_db::load_balancers::get(&state.db, id).await?;
    authorize_resource(
        &state.db,
        &p,
        Perm::NetworkWrite,
        lb.tenant_id,
        "load balancer",
    )
    .await?;
    let name = check_name(&req.name)?;
    let listeners = check_listeners(&req.listeners)?;
    let mut tx = state.db.begin().await?;
    let members = member_nics(&mut tx, lb.tenant_id, lb.vpc_id, &req.member_vm_ids).await?;
    if req.health_check {
        monitor_addresses(&mut tx, &members).await?;
    }
    let nics: Vec<NicId> = members.iter().map(|m| m.0).collect();
    mst_db::load_balancers::update(&mut tx, id, name, req.health_check, &listeners, &nics).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(lb.tenant_id),
        "load_balancer.update",
        Some(("load_balancer", id.0)),
        json!({"name": name, "listeners": req.listeners, "members": req.member_vm_ids}),
    )
    .await;
    state.net.kick();
    let lb = mst_db::load_balancers::get(&state.db, id).await?;
    let mut db = state.db.acquire().await?;
    Ok(Json(view(&mut db, lb).await?))
}

/// Deletes a load balancer and gives back its virtual IP.
#[utoipa::path(delete, path = "/load-balancers/{id}", tag = "networks", params(("id" = LoadBalancerId, Path)),
    responses((status = 204), (status = 403), (status = 404)))]
async fn delete_load_balancer(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<LoadBalancerId>,
) -> ApiResult<StatusCode> {
    let lb = mst_db::load_balancers::get(&state.db, id).await?;
    authorize_resource(
        &state.db,
        &p,
        Perm::NetworkWrite,
        lb.tenant_id,
        "load balancer",
    )
    .await?;
    let mut tx = state.db.begin().await?;
    mst_db::load_balancers::delete(&mut tx, &lb).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(lb.tenant_id),
        "load_balancer.delete",
        Some(("load_balancer", id.0)),
        json!({"name": lb.name, "ip": lb.ip}),
    )
    .await;
    state.net.kick();
    Ok(StatusCode::NO_CONTENT)
}
