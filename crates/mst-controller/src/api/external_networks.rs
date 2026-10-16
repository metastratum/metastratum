//! External networks: provider networks outside OVN that VPCs route to, and
//! that floating IPs come from.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use mst_core::ids::{ExternalNetworkId, TenantId};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::authz::{Perm, Principal, audit, authorize, authorize_resource};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_external_networks, create_external_network))
        .routes(routes!(update_external_network, delete_external_network))
        .routes(routes!(usable_external_networks))
}

#[derive(Serialize, ToSchema)]
pub struct ExternalNetworkView {
    id: ExternalNetworkId,
    name: String,
    /// Name in the hosts' `ovn-bridge-mappings`.
    physical_network: String,
    cidr: String,
    gateway: String,
    /// The range metastratum hands out (VPC gateways, floating IPs).
    pool_start: String,
    pool_end: String,
    /// Addresses handed out.
    used: i64,
    /// Hosts that reach the physical network (and so can be gateways).
    hosts: Vec<String>,
    created_at: DateTime<Utc>,
}

async fn views(
    state: &AppState,
    nets: Vec<mst_db::external::ExternalNetwork>,
) -> ApiResult<Vec<ExternalNetworkView>> {
    let used: HashMap<_, _> = mst_db::external::usage(&state.db)
        .await?
        .into_iter()
        .collect();
    let hosts = mst_db::hosts::list(&state.db).await?;
    Ok(nets
        .into_iter()
        .map(|n| ExternalNetworkView {
            used: used.get(&n.id).copied().unwrap_or(0),
            hosts: hosts
                .iter()
                .filter(|h| h.physical_networks.contains(&n.physical_network))
                .map(|h| h.name.clone())
                .collect(),
            id: n.id,
            name: n.name,
            physical_network: n.physical_network,
            cidr: n.cidr,
            gateway: n.gateway,
            pool_start: n.pool_start,
            pool_end: n.pool_end,
            created_at: n.created_at,
        })
        .collect())
}

#[utoipa::path(get, path = "/external-networks", tag = "networks",
    responses((status = 200, body = Vec<ExternalNetworkView>), (status = 403)))]
async fn list_external_networks(
    State(state): State<AppState>,
    p: Principal,
) -> ApiResult<Json<Vec<ExternalNetworkView>>> {
    authorize(&state.db, &p, Perm::InfraRead, p.tenant_id).await?;
    let nets = mst_db::external::list(&state.db).await?;
    Ok(Json(views(&state, nets).await?))
}

#[derive(Deserialize, ToSchema)]
pub struct CreateExternalNetwork {
    name: String,
    physical_network: String,
    /// e.g. `203.0.113.0/24`.
    cidr: String,
    /// The upstream router; default the first address.
    gateway: Option<String>,
    pool_start: String,
    pool_end: String,
}

fn check_name(name: &str) -> ApiResult<&str> {
    let name = name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(ApiError::bad_request("name must be 1-64 characters"));
    }
    Ok(name)
}

#[utoipa::path(post, path = "/external-networks", tag = "networks", request_body = CreateExternalNetwork,
    responses((status = 201, body = ExternalNetworkView), (status = 400), (status = 403), (status = 409)))]
async fn create_external_network(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<CreateExternalNetwork>,
) -> ApiResult<(StatusCode, Json<ExternalNetworkView>)> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let name = check_name(&req.name)?;
    let physnet = req.physical_network.trim();
    if physnet.is_empty() || physnet.contains([',', ':', ' ']) {
        return Err(ApiError::bad_request(
            "physical_network must be a bridge-mapping name",
        ));
    }
    let cidr = mst_netplan::Cidr::parse(req.cidr.trim())
        .filter(|c| (8..=30).contains(&c.len))
        .ok_or_else(|| ApiError::bad_request("cidr must be an IPv4 range from /8 to /30"))?;
    let gateway = req
        .gateway
        .clone()
        .unwrap_or_else(|| mst_netplan::first_host(cidr).to_string());
    let n = mst_db::external::create(
        &state.db,
        ExternalNetworkId::new(),
        &mst_db::external::ExternalSpec {
            name,
            physical_network: physnet,
            cidr: &cidr.to_string(),
            gateway: &gateway,
            pool_start: req.pool_start.trim(),
            pool_end: req.pool_end.trim(),
        },
    )
    .await?;
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "external_network.create",
        Some(("external_network", n.id.0)),
        json!({"name": n.name, "cidr": n.cidr, "physical_network": n.physical_network}),
    )
    .await;
    state.net.kick();
    Ok((
        StatusCode::CREATED,
        Json(views(&state, vec![n]).await?.remove(0)),
    ))
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateExternalNetwork {
    name: String,
    pool_start: String,
    pool_end: String,
}

/// Renames the network or changes its pool. Addresses already handed out
/// stay, even outside a narrowed pool.
#[utoipa::path(put, path = "/external-networks/{id}", tag = "networks", params(("id" = ExternalNetworkId, Path)),
    request_body = UpdateExternalNetwork,
    responses((status = 200, body = ExternalNetworkView), (status = 400), (status = 403), (status = 404)))]
async fn update_external_network(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<ExternalNetworkId>,
    Json(req): Json<UpdateExternalNetwork>,
) -> ApiResult<Json<ExternalNetworkView>> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let name = check_name(&req.name)?;
    let n = mst_db::external::update(
        &state.db,
        id,
        name,
        req.pool_start.trim(),
        req.pool_end.trim(),
    )
    .await?;
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "external_network.update",
        Some(("external_network", id.0)),
        json!({"name": n.name, "pool_start": n.pool_start, "pool_end": n.pool_end}),
    )
    .await;
    Ok(Json(views(&state, vec![n]).await?.remove(0)))
}

#[utoipa::path(delete, path = "/external-networks/{id}", tag = "networks", params(("id" = ExternalNetworkId, Path)),
    responses((status = 204), (status = 403), (status = 404), (status = 409, description = "Still in use")))]
async fn delete_external_network(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<ExternalNetworkId>,
) -> ApiResult<StatusCode> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let n = mst_db::external::get(&state.db, id).await?;
    let mut tx = state.db.begin().await?;
    mst_db::sqlx::query(
        "DELETE FROM tenant_grants WHERE kind = 'external_network' AND ref_id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    mst_db::external::delete(&mut *tx, id).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "external_network.delete",
        Some(("external_network", id.0)),
        json!({"name": n.name}),
    )
    .await;
    state.net.kick();
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize, ToSchema)]
pub struct UsableExternalNetwork {
    id: ExternalNetworkId,
    name: String,
    cidr: String,
}

/// External networks the tenant's VPCs may route to.
#[utoipa::path(get, path = "/tenants/{id}/external-networks", tag = "tenants", params(("id" = TenantId, Path)),
    responses((status = 200, body = Vec<UsableExternalNetwork>), (status = 404)))]
async fn usable_external_networks(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TenantId>,
) -> ApiResult<Json<Vec<UsableExternalNetwork>>> {
    authorize_resource(&state.db, &p, Perm::NetworkRead, id, "tenant").await?;
    let mut out = vec![];
    for n in mst_db::external::list(&state.db).await? {
        if mst_db::grants::may_use(&state.db, id, "external_network", n.id.0).await? {
            out.push(UsableExternalNetwork {
                id: n.id,
                name: n.name,
                cidr: n.cidr,
            });
        }
    }
    Ok(Json(out))
}
