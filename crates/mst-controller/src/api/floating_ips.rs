//! Floating IPs: addresses on an external network that forward to a VM.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use mst_core::ids::{ExternalIpId, ExternalNetworkId, NicId, TenantId, VmId};
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
        .routes(routes!(list_floating_ips, allocate_floating_ip))
        .routes(routes!(update_floating_ip, release_floating_ip))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Defaults to the caller's tenant.
    tenant_id: Option<TenantId>,
    #[serde(default)]
    descendants: bool,
}

#[derive(Serialize, ToSchema)]
pub struct FloatingIpView {
    id: ExternalIpId,
    tenant_id: TenantId,
    external_network_id: ExternalNetworkId,
    external_network_name: String,
    ip: String,
    /// The NIC it forwards to.
    nic_id: Option<NicId>,
    vm_id: Option<VmId>,
    vm_name: Option<String>,
    created_at: DateTime<Utc>,
}

async fn views(
    state: &AppState,
    ips: Vec<mst_db::external::ExternalIp>,
) -> ApiResult<Vec<FloatingIpView>> {
    let nets: HashMap<_, _> = mst_db::external::list(&state.db)
        .await?
        .into_iter()
        .map(|n| (n.id, n.name))
        .collect();
    let mut out = vec![];
    for f in ips {
        let vm = match f.nic_id {
            Some(nic) => {
                let n = mst_db::networks::get_nic(&state.db, nic).await?;
                Some(mst_db::vms::get(&state.db, n.vm_id).await?)
            }
            None => None,
        };
        out.push(FloatingIpView {
            external_network_name: nets
                .get(&f.external_network_id)
                .cloned()
                .unwrap_or_default(),
            vm_id: vm.as_ref().map(|v| v.id),
            vm_name: vm.map(|v| v.name),
            id: f.id,
            tenant_id: f.tenant_id,
            external_network_id: f.external_network_id,
            ip: f.ip,
            nic_id: f.nic_id,
            created_at: f.created_at,
        });
    }
    Ok(out)
}

#[utoipa::path(get, path = "/floating-ips", tag = "networks", params(ListQuery),
    responses((status = 200, body = Vec<FloatingIpView>)))]
async fn list_floating_ips(
    State(state): State<AppState>,
    p: Principal,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<FloatingIpView>>> {
    let tenant = q.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::NetworkRead, tenant).await?;
    let ips = mst_db::external::floating_ips(&state.db, tenant, q.descendants).await?;
    Ok(Json(views(&state, ips).await?))
}

#[derive(Deserialize, ToSchema)]
pub struct AllocateFloatingIp {
    tenant_id: Option<TenantId>,
    external_network_id: ExternalNetworkId,
    /// Forward to this NIC right away.
    nic_id: Option<NicId>,
}

/// Points a floating IP at a NIC (or nowhere). The NIC's VM must belong to
/// the floating IP's tenant, and its VPC must route to the floating IP's
/// external network.
async fn check_target(
    state: &AppState,
    f_tenant: TenantId,
    f_network: ExternalNetworkId,
    nic: NicId,
) -> ApiResult<()> {
    let n = mst_db::networks::get_nic(&state.db, nic).await?;
    let vm = mst_db::vms::get(&state.db, n.vm_id).await?;
    if vm.tenant_id != f_tenant {
        return Err(ApiError::not_found("NIC"));
    }
    let net = mst_db::networks::get(&state.db, n.network_id).await?;
    let vpc = mst_db::networks::get_vpc(&state.db, net.vpc_id).await?;
    if vpc.external_network_id != Some(f_network) {
        return Err(ApiError::bad_request(format!(
            "VPC {} does not route to this floating IP's external network; connect it first",
            vpc.name
        )));
    }
    Ok(())
}

#[utoipa::path(post, path = "/floating-ips", tag = "networks", request_body = AllocateFloatingIp,
    responses((status = 201, body = FloatingIpView), (status = 400), (status = 403), (status = 409)))]
async fn allocate_floating_ip(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<AllocateFloatingIp>,
) -> ApiResult<(StatusCode, Json<FloatingIpView>)> {
    let tenant = req.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::NetworkWrite, tenant).await?;
    mst_db::external::get(&state.db, req.external_network_id).await?;
    if !mst_db::grants::may_use(
        &state.db,
        tenant,
        "external_network",
        req.external_network_id.0,
    )
    .await?
    {
        return Err(ApiError::forbidden_msg(
            "that external network is not available to this tenant",
        ));
    }
    if let Some(nic) = req.nic_id {
        check_target(&state, tenant, req.external_network_id, nic).await?;
    }
    let mut tx = state.db.begin().await?;
    let delta = [("floating_ips".to_owned(), 1)].into_iter().collect();
    mst_db::quotas::reserve(&mut tx, tenant, &delta).await?;
    let ip = mst_db::external::allocate(&mut tx, req.external_network_id, tenant, None).await?;
    if let Some(nic) = req.nic_id {
        mst_db::external::associate(&mut *tx, ip.id, Some(nic)).await?;
    }
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(tenant),
        "floating_ip.allocate",
        Some(("floating_ip", ip.id.0)),
        json!({"ip": ip.ip, "nic_id": req.nic_id}),
    )
    .await;
    state.net.kick();
    let ip = mst_db::external::get_ip(&state.db, ip.id).await?;
    Ok((
        StatusCode::CREATED,
        Json(views(&state, vec![ip]).await?.remove(0)),
    ))
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateFloatingIp {
    /// Forward to this NIC; `null` to stop forwarding.
    nic_id: Option<NicId>,
}

#[utoipa::path(patch, path = "/floating-ips/{id}", tag = "networks", params(("id" = ExternalIpId, Path)),
    request_body = UpdateFloatingIp,
    responses((status = 200, body = FloatingIpView), (status = 400), (status = 403), (status = 404), (status = 409)))]
async fn update_floating_ip(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<ExternalIpId>,
    Json(req): Json<UpdateFloatingIp>,
) -> ApiResult<Json<FloatingIpView>> {
    let ip = mst_db::external::get_ip(&state.db, id).await?;
    if ip.kind != "floating" {
        return Err(ApiError::not_found("floating IP"));
    }
    authorize_resource(
        &state.db,
        &p,
        Perm::NetworkWrite,
        ip.tenant_id,
        "floating IP",
    )
    .await?;
    if let Some(nic) = req.nic_id {
        check_target(&state, ip.tenant_id, ip.external_network_id, nic).await?;
    }
    mst_db::external::associate(&state.db, id, req.nic_id).await?;
    audit(
        &state.db,
        Some(&p),
        Some(ip.tenant_id),
        "floating_ip.update",
        Some(("floating_ip", id.0)),
        json!({"ip": ip.ip, "nic_id": req.nic_id}),
    )
    .await;
    state.net.kick();
    let ip = mst_db::external::get_ip(&state.db, id).await?;
    Ok(Json(views(&state, vec![ip]).await?.remove(0)))
}

#[utoipa::path(delete, path = "/floating-ips/{id}", tag = "networks", params(("id" = ExternalIpId, Path)),
    responses((status = 204), (status = 403), (status = 404)))]
async fn release_floating_ip(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<ExternalIpId>,
) -> ApiResult<StatusCode> {
    let ip = mst_db::external::get_ip(&state.db, id).await?;
    if ip.kind != "floating" {
        return Err(ApiError::not_found("floating IP"));
    }
    authorize_resource(
        &state.db,
        &p,
        Perm::NetworkWrite,
        ip.tenant_id,
        "floating IP",
    )
    .await?;
    mst_db::external::release(&state.db, id).await?;
    audit(
        &state.db,
        Some(&p),
        Some(ip.tenant_id),
        "floating_ip.release",
        Some(("floating_ip", id.0)),
        json!({"ip": ip.ip}),
    )
    .await;
    state.net.kick();
    Ok(StatusCode::NO_CONTENT)
}
