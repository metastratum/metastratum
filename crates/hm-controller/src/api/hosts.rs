use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use hm_core::ids::{ClusterId, HostId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::authz::{Perm, Principal, audit, authorize};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(update_host))
        .routes(routes!(list_hosts))
        .routes(routes!(create_join_token))
}

#[derive(Serialize, ToSchema)]
pub struct HostView {
    id: HostId,
    name: String,
    address: String,
    /// `ready`, `maintenance` or `decommissioned`.
    state: String,
    cluster_id: Option<ClusterId>,
    cluster_name: Option<String>,
    /// Whether the host's agent has a live session (and heartbeated recently).
    connected: bool,
    cpu_count: i32,
    memory_mib: i64,
    cpu_model: String,
    kvm: bool,
    libvirt_version: String,
    ovn_chassis: String,
    agent_version: String,
    vm_count: usize,
    allocated_memory_mib: u64,
    cert_expires_at: DateTime<Utc>,
    last_seen_at: Option<DateTime<Utc>>,
}

#[utoipa::path(get, path = "/hosts", tag = "hosts", responses((status = 200, body = Vec<HostView>), (status = 403)))]
async fn list_hosts(State(state): State<AppState>, p: Principal) -> ApiResult<Json<Vec<HostView>>> {
    authorize(&state.db, &p, Perm::InfraRead, p.tenant_id).await?;
    let clusters: std::collections::HashMap<_, _> = hm_db::clusters::list(&state.db)
        .await?
        .into_iter()
        .map(|c| (c.id, c.name))
        .collect();
    let mut out = vec![];
    for h in hm_db::hosts::list(&state.db).await? {
        let vms = hm_db::vms::list_on_host(&state.db, h.id).await?;
        out.push(HostView {
            cluster_name: h.cluster_id.and_then(|c| clusters.get(&c).cloned()),
            cluster_id: h.cluster_id,
            connected: h.is_live(),
            vm_count: vms.len(),
            allocated_memory_mib: vms.iter().map(|v| v.spec.0.memory_mib).sum(),
            id: h.id,
            name: h.name,
            address: h.address,
            state: h.state,
            cpu_count: h.cpu_count,
            memory_mib: h.memory_mib,
            cpu_model: h.cpu_model,
            kvm: h.kvm,
            libvirt_version: h.libvirt_version,
            ovn_chassis: h.ovn_chassis,
            agent_version: h.agent_version,
            cert_expires_at: h.cert_expires_at,
            last_seen_at: h.last_seen_at,
        });
    }
    Ok(Json(out))
}

#[derive(Deserialize, ToSchema)]
pub struct CreateJoinToken {
    /// Validity in minutes (default 60, max 1440).
    ttl_minutes: Option<u32>,
    /// Cluster the host joins on enrollment; without one the host is not
    /// scheduled on until it is added to a cluster.
    cluster_id: Option<ClusterId>,
}

#[derive(Serialize, ToSchema)]
pub struct JoinTokenView {
    /// Pass to `hm-agent --join-token`. Shown only once.
    token: String,
    expires_at: DateTime<Utc>,
}

pub async fn issue_join_token(
    state: &AppState,
    created_by: Option<hm_core::ids::UserId>,
    ttl: Duration,
    cluster: Option<ClusterId>,
) -> ApiResult<JoinTokenView> {
    if let Some(c) = cluster {
        hm_db::clusters::get(&state.db, c).await?;
    }
    let token = hm_pki::JoinToken::generate(&state.ca.fingerprint());
    let expires_at = Utc::now() + chrono::Duration::from_std(ttl).map_err(ApiError::internal)?;
    hm_db::join_tokens::create(
        &state.db,
        &token.id,
        &token.secret_hash(),
        created_by,
        expires_at,
        cluster,
    )
    .await?;
    Ok(JoinTokenView {
        token: token.to_string(),
        expires_at,
    })
}

/// Creates a single-use token for enrolling one host.
#[utoipa::path(post, path = "/hosts/join-tokens", tag = "hosts", request_body = CreateJoinToken,
    responses((status = 201, body = JoinTokenView), (status = 403)))]
async fn create_join_token(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<CreateJoinToken>,
) -> ApiResult<(StatusCode, Json<JoinTokenView>)> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let minutes = req.ttl_minutes.unwrap_or(60).clamp(1, 1440);
    let view = issue_join_token(
        &state,
        Some(p.user_id),
        Duration::from_secs(u64::from(minutes) * 60),
        req.cluster_id,
    )
    .await?;
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "host.join_token",
        None,
        serde_json::json!({"expires_at": view.expires_at, "cluster_id": req.cluster_id}),
    )
    .await;
    Ok((StatusCode::CREATED, Json(view)))
}

#[derive(Deserialize, ToSchema)]
pub struct UpdateHost {
    /// Move the host to this cluster; `null` removes it from any cluster.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>, format = Uuid)]
    cluster_id: Option<Option<ClusterId>>,
    /// `ready`, or `maintenance` to stop placing new VMs on the host.
    state: Option<String>,
}

fn double_option<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<ClusterId>>, D::Error> {
    Ok(Some(Option::deserialize(d)?))
}

#[utoipa::path(patch, path = "/hosts/{id}", tag = "hosts", params(("id" = HostId, Path)), request_body = UpdateHost,
    responses((status = 204), (status = 400), (status = 403), (status = 404)))]
async fn update_host(
    State(state): State<AppState>,
    p: Principal,
    axum::extract::Path(id): axum::extract::Path<HostId>,
    Json(req): Json<UpdateHost>,
) -> ApiResult<StatusCode> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let host = hm_db::hosts::get(&state.db, id).await?;
    if let Some(cluster) = req.cluster_id {
        if let Some(c) = cluster {
            hm_db::clusters::get(&state.db, c).await?;
        }
        if cluster != host.cluster_id && !hm_db::vms::list_on_host(&state.db, id).await?.is_empty()
        {
            return Err(ApiError::conflict(
                "move or delete the host's VMs before changing its cluster",
            ));
        }
        hm_db::hosts::set_cluster(&state.db, id, cluster).await?;
    }
    if let Some(s) = &req.state {
        if !matches!(s.as_str(), "ready" | "maintenance") {
            return Err(ApiError::bad_request("state must be ready or maintenance"));
        }
        hm_db::hosts::set_state(&state.db, id, s).await?;
    }
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "host.update",
        Some(("host", id.0)),
        serde_json::json!({"name": host.name, "cluster_id": req.cluster_id, "state": req.state}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}
