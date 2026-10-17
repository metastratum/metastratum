//! VPCs (isolated routing domains) and their networks.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use mst_core::ids::{ExternalNetworkId, NetworkId, TenantId, VpcId};
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
        .routes(routes!(list_vpcs, create_vpc))
        .routes(routes!(get_vpc, update_vpc, delete_vpc))
        .routes(routes!(list_networks, create_network))
        .routes(routes!(get_network, delete_network))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Defaults to the caller's tenant.
    tenant_id: Option<TenantId>,
    /// Include those of all sub-tenants.
    #[serde(default)]
    descendants: bool,
}

#[derive(Serialize, ToSchema)]
pub struct NetworkView {
    id: NetworkId,
    tenant_id: TenantId,
    vpc_id: VpcId,
    name: String,
    cidr: String,
    gateway: String,
    /// Connected NICs.
    ports: i64,
    created_at: DateTime<Utc>,
}

#[derive(Serialize, ToSchema)]
pub struct VpcView {
    id: VpcId,
    tenant_id: TenantId,
    name: String,
    /// Where traffic leaves the VPC, with source NAT; none means VMs reach
    /// only each other.
    external_network_id: Option<ExternalNetworkId>,
    external_network_name: Option<String>,
    /// The VPC's address on the external network (its SNAT address).
    gateway_ip: Option<String>,
    networks: Vec<NetworkView>,
    created_at: DateTime<Utc>,
}

pub(super) fn check_name(name: &str) -> ApiResult<&str> {
    let name = name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(ApiError::bad_request("name must be 1-64 characters"));
    }
    Ok(name)
}

fn network_views(
    nets: Vec<mst_db::networks::Network>,
    counts: &HashMap<NetworkId, i64>,
) -> Vec<NetworkView> {
    nets.into_iter()
        .map(|n| NetworkView {
            ports: counts.get(&n.id).copied().unwrap_or(0),
            id: n.id,
            tenant_id: n.tenant_id,
            vpc_id: n.vpc_id,
            name: n.name,
            cidr: n.cidr,
            gateway: n.gateway,
            created_at: n.created_at,
        })
        .collect()
}

async fn vpc_views(state: &AppState, vpcs: Vec<mst_db::networks::Vpc>) -> ApiResult<Vec<VpcView>> {
    let counts: HashMap<_, _> = mst_db::networks::port_counts(&state.db)
        .await?
        .into_iter()
        .collect();
    let externals: HashMap<_, _> = mst_db::external::list(&state.db)
        .await?
        .into_iter()
        .map(|e| (e.id, e.name))
        .collect();
    let mut out = vec![];
    for v in vpcs {
        let nets = mst_db::networks::list_all(&state.db, v.tenant_id, false)
            .await?
            .into_iter()
            .filter(|n| n.vpc_id == v.id)
            .collect();
        let gateway = mst_db::external::gateway_of(&state.db, v.id).await?;
        out.push(VpcView {
            external_network_name: v
                .external_network_id
                .and_then(|e| externals.get(&e).cloned()),
            gateway_ip: gateway.map(|g| g.ip),
            networks: network_views(nets, &counts),
            id: v.id,
            tenant_id: v.tenant_id,
            name: v.name,
            external_network_id: v.external_network_id,
            created_at: v.created_at,
        });
    }
    Ok(out)
}

#[utoipa::path(get, path = "/vpcs/{id}", tag = "networks", params(("id" = VpcId, Path)),
    responses((status = 200, body = VpcView), (status = 404)))]
async fn get_vpc(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VpcId>,
) -> ApiResult<Json<VpcView>> {
    let vpc = mst_db::networks::get_vpc(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::NetworkRead, vpc.tenant_id, "VPC").await?;
    Ok(Json(vpc_views(&state, vec![vpc]).await?.remove(0)))
}

#[utoipa::path(get, path = "/networks/{id}", tag = "networks", params(("id" = NetworkId, Path)),
    responses((status = 200, body = NetworkView), (status = 404)))]
async fn get_network(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<NetworkId>,
) -> ApiResult<Json<NetworkView>> {
    let n = mst_db::networks::get(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::NetworkRead, n.tenant_id, "network").await?;
    let counts: HashMap<_, _> = mst_db::networks::port_counts(&state.db)
        .await?
        .into_iter()
        .collect();
    Ok(Json(network_views(vec![n], &counts).remove(0)))
}

#[utoipa::path(get, path = "/vpcs", tag = "networks", params(ListQuery), responses((status = 200, body = Vec<VpcView>)))]
async fn list_vpcs(
    State(state): State<AppState>,
    p: Principal,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<VpcView>>> {
    let tenant = q.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::NetworkRead, tenant).await?;
    let vpcs = mst_db::networks::list_vpcs(&state.db, tenant, q.descendants).await?;
    Ok(Json(vpc_views(&state, vpcs).await?))
}

#[derive(Deserialize, ToSchema)]
pub struct CreateVpc {
    /// Defaults to the caller's tenant.
    tenant_id: Option<TenantId>,
    name: String,
    /// Route to this external network (it must be granted to the tenant).
    external_network_id: Option<ExternalNetworkId>,
}

/// Gives a VPC a way out through `ext`, or takes it away.
async fn set_external(
    state: &AppState,
    tx: &mut mst_db::sqlx::PgConnection,
    vpc: &mst_db::networks::Vpc,
    ext: Option<ExternalNetworkId>,
) -> ApiResult<()> {
    if vpc.external_network_id == ext {
        return Ok(());
    }
    if let Some(old) = vpc.external_network_id {
        let fips: i64 = mst_db::sqlx::query_scalar(
            "SELECT count(*) FROM external_ips f JOIN nics n ON n.id = f.nic_id JOIN networks w ON w.id = n.network_id
             WHERE w.vpc_id = $1 AND f.external_network_id = $2",
        )
        .bind(vpc.id)
        .bind(old)
        .fetch_one(&mut *tx)
        .await?;
        if fips > 0 {
            return Err(ApiError::conflict(format!(
                "{fips} floating IP(s) point at VMs in this VPC; detach them first"
            )));
        }
        if mst_db::load_balancers::any_external_in_vpc(&mut *tx, vpc.id).await? {
            return Err(ApiError::conflict(
                "load balancers in this VPC have addresses on its external network; delete them first",
            ));
        }
        mst_db::sqlx::query("DELETE FROM external_ips WHERE vpc_id = $1")
            .bind(vpc.id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(e) = ext {
        mst_db::external::get(&state.db, e).await?;
        if !mst_db::grants::may_use(&state.db, vpc.tenant_id, "external_network", e.0).await? {
            return Err(ApiError::forbidden_msg(
                "that external network is not available to this tenant",
            ));
        }
        mst_db::external::allocate(&mut *tx, e, vpc.tenant_id, Some(vpc.id)).await?;
    }
    mst_db::networks::set_vpc_external(&mut *tx, vpc.id, ext).await?;
    Ok(())
}

#[utoipa::path(post, path = "/vpcs", tag = "networks", request_body = CreateVpc,
    responses((status = 201, body = VpcView), (status = 400), (status = 403), (status = 409)))]
async fn create_vpc(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<CreateVpc>,
) -> ApiResult<(StatusCode, Json<VpcView>)> {
    let tenant = req.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::NetworkWrite, tenant).await?;
    let name = check_name(&req.name)?;
    let mut tx = state.db.begin().await?;
    let delta = [("vpcs".to_owned(), 1)].into_iter().collect();
    mst_db::quotas::reserve(&mut tx, tenant, &delta).await?;
    let vpc = mst_db::networks::create_vpc(&mut *tx, VpcId::new(), tenant, name).await?;
    set_external(&state, &mut tx, &vpc, req.external_network_id).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(tenant),
        "vpc.create",
        Some(("vpc", vpc.id.0)),
        json!({"name": name, "external_network_id": req.external_network_id}),
    )
    .await;
    state.net.kick();
    let vpc = mst_db::networks::get_vpc(&state.db, vpc.id).await?;
    Ok((
        StatusCode::CREATED,
        Json(vpc_views(&state, vec![vpc]).await?.remove(0)),
    ))
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateVpc {
    name: Option<String>,
    /// Route to this external network; `null` disconnects the VPC.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>, format = Uuid)]
    external_network_id: Option<Option<ExternalNetworkId>>,
}

fn double_option<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<ExternalNetworkId>>, D::Error> {
    Ok(Some(Option::deserialize(d)?))
}

#[utoipa::path(patch, path = "/vpcs/{id}", tag = "networks", params(("id" = VpcId, Path)), request_body = UpdateVpc,
    responses((status = 200, body = VpcView), (status = 400), (status = 403), (status = 404), (status = 409)))]
async fn update_vpc(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VpcId>,
    Json(req): Json<UpdateVpc>,
) -> ApiResult<Json<VpcView>> {
    let vpc = mst_db::networks::get_vpc(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::NetworkWrite, vpc.tenant_id, "VPC").await?;
    let mut tx = state.db.begin().await?;
    if let Some(name) = &req.name {
        mst_db::networks::rename_vpc(&mut *tx, id, check_name(name)?).await?;
    }
    if let Some(ext) = req.external_network_id {
        set_external(&state, &mut tx, &vpc, ext).await?;
    }
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(vpc.tenant_id),
        "vpc.update",
        Some(("vpc", id.0)),
        json!({"name": req.name, "external_network_id": req.external_network_id}),
    )
    .await;
    state.net.kick();
    let vpc = mst_db::networks::get_vpc(&state.db, id).await?;
    Ok(Json(vpc_views(&state, vec![vpc]).await?.remove(0)))
}

#[utoipa::path(delete, path = "/vpcs/{id}", tag = "networks", params(("id" = VpcId, Path)),
    responses((status = 204), (status = 403), (status = 404), (status = 409, description = "Still has networks")))]
async fn delete_vpc(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<VpcId>,
) -> ApiResult<StatusCode> {
    let vpc = mst_db::networks::get_vpc(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::NetworkWrite, vpc.tenant_id, "VPC").await?;
    if mst_db::load_balancers::any_in_vpc(&state.db, id).await? {
        return Err(ApiError::conflict(
            "the VPC has load balancers; delete them first",
        ));
    }
    // Its gateway address goes with it.
    mst_db::networks::delete_vpc(&state.db, id).await?;
    audit(
        &state.db,
        Some(&p),
        Some(vpc.tenant_id),
        "vpc.delete",
        Some(("vpc", id.0)),
        json!({"name": vpc.name}),
    )
    .await;
    state.net.kick();
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(get, path = "/networks", tag = "networks", params(ListQuery),
    responses((status = 200, body = Vec<NetworkView>)))]
async fn list_networks(
    State(state): State<AppState>,
    p: Principal,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<NetworkView>>> {
    let tenant = q.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::NetworkRead, tenant).await?;
    let counts: HashMap<_, _> = mst_db::networks::port_counts(&state.db)
        .await?
        .into_iter()
        .collect();
    let nets = mst_db::networks::list_all(&state.db, tenant, q.descendants).await?;
    Ok(Json(network_views(nets, &counts)))
}

#[derive(Deserialize, ToSchema)]
pub struct CreateNetwork {
    vpc_id: VpcId,
    name: String,
    /// e.g. `10.1.0.0/24`; /16 to /29, not overlapping the VPC's others.
    cidr: String,
    /// The router's address in the network; default the first one.
    gateway: Option<String>,
}

#[utoipa::path(post, path = "/networks", tag = "networks", request_body = CreateNetwork,
    responses((status = 201, body = NetworkView), (status = 400), (status = 403), (status = 409)))]
async fn create_network(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<CreateNetwork>,
) -> ApiResult<(StatusCode, Json<NetworkView>)> {
    let vpc = mst_db::networks::get_vpc(&state.db, req.vpc_id).await?;
    authorize_resource(&state.db, &p, Perm::NetworkWrite, vpc.tenant_id, "VPC").await?;
    let name = check_name(&req.name)?;
    let cidr = mst_netplan::Cidr::parse(req.cidr.trim())
        .filter(|c| (16..=29).contains(&c.len))
        .ok_or_else(|| {
            ApiError::bad_request("cidr must be an IPv4 range from /16 to /29, e.g. 10.1.0.0/24")
        })?;
    let gateway = match &req.gateway {
        Some(g) => g.trim().to_owned(),
        None => mst_netplan::first_host(cidr).to_string(),
    };
    let n = mst_db::networks::create(
        &state.db,
        NetworkId::new(),
        &vpc,
        name,
        &cidr.to_string(),
        &gateway,
    )
    .await?;
    audit(
        &state.db,
        Some(&p),
        Some(vpc.tenant_id),
        "network.create",
        Some(("network", n.id.0)),
        json!({"name": n.name, "cidr": n.cidr, "vpc_id": vpc.id}),
    )
    .await;
    state.net.kick();
    Ok((
        StatusCode::CREATED,
        Json(network_views(vec![n], &HashMap::new()).remove(0)),
    ))
}

#[utoipa::path(delete, path = "/networks/{id}", tag = "networks", params(("id" = NetworkId, Path)),
    responses((status = 204), (status = 403), (status = 404), (status = 409, description = "VMs still connected")))]
async fn delete_network(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<NetworkId>,
) -> ApiResult<StatusCode> {
    let n = mst_db::networks::get(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::NetworkWrite, n.tenant_id, "network").await?;
    if mst_db::load_balancers::any_in_network(&state.db, id).await? {
        return Err(ApiError::conflict(
            "load balancers have their address in this network; delete them first",
        ));
    }
    mst_db::networks::delete(&state.db, id).await?;
    audit(
        &state.db,
        Some(&p),
        Some(n.tenant_id),
        "network.delete",
        Some(("network", id.0)),
        json!({"name": n.name}),
    )
    .await;
    state.net.kick();
    Ok(StatusCode::NO_CONTENT)
}
