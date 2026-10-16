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
        .routes(routes!(evacuate_host))
        .routes(routes!(get_fencing, set_fencing))
        .routes(routes!(test_fencing))
        .routes(routes!(renew_certificate))
        .routes(routes!(decommission_host))
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
    /// Accepts live migrations (libvirt TLS is set up).
    migration_ready: bool,
    /// The controller replica holding the agent's session, and since when.
    controller: Option<String>,
    connected_since: Option<DateTime<Utc>>,
    /// `redfish` or `libvirt` when the host can be fenced (powered off when
    /// it fails, so its VMs can restart elsewhere).
    fence_kind: Option<String>,
    cpu_count: i32,
    memory_mib: i64,
    cpu_model: String,
    kvm: bool,
    libvirt_version: String,
    ovn_chassis: String,
    /// Physical networks the host reaches; external networks on them can
    /// have their gateways here.
    physical_networks: Vec<String>,
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
            migration_ready: h.migration_ready,
            controller: h.connected_to.clone(),
            connected_since: h.connected_since,
            fence_kind: h.fence_kind.clone(),
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
            physical_networks: h.physical_networks,
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
        if cluster != host.cluster_id {
            // Whether libvirt must listen depends on the cluster's hosts:
            // both the one it left and the one it joined may change.
            let tls = crate::agents::libvirt_tls_policy;
            tokio::spawn(tls(state.clone(), host.cluster_id, id));
            tokio::spawn(tls(state.clone(), cluster, id));
        }
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

#[derive(Serialize, ToSchema)]
pub struct EvacuateRef {
    task_id: hm_core::ids::TaskId,
}

/// Puts a host in maintenance and live-migrates its running VMs to other
/// hosts of its cluster, one at a time. Stopped VMs stay.
#[utoipa::path(post, path = "/hosts/{id}/evacuate", tag = "hosts", params(("id" = HostId, Path)),
    responses((status = 202, body = EvacuateRef), (status = 403), (status = 404), (status = 409)))]
async fn evacuate_host(
    State(state): State<AppState>,
    p: Principal,
    axum::extract::Path(id): axum::extract::Path<HostId>,
) -> ApiResult<(StatusCode, Json<EvacuateRef>)> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let host = hm_db::hosts::get(&state.db, id).await?;
    if host.cluster_id.is_none() {
        return Err(ApiError::conflict(
            "the host is in no cluster, so its VMs have nowhere to go",
        ));
    }
    let mut tx = state.db.begin().await?;
    hm_db::hosts::set_state(&mut *tx, id, "maintenance").await?;
    let task = hm_db::tasks::enqueue(
        &mut *tx,
        hm_db::tasks::NewTask {
            tenant_id: p.tenant_id,
            kind: "host.evacuate",
            target: Some(("host", id.0)),
            params: serde_json::json!({}),
            created_by: Some(p.user_id),
        },
    )
    .await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "host.evacuate",
        Some(("host", id.0)),
        serde_json::json!({"name": host.name}),
    )
    .await;
    Ok((StatusCode::ACCEPTED, Json(EvacuateRef { task_id: task.id })))
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct Fencing {
    /// `redfish` or `libvirt`; null removes fencing.
    kind: Option<String>,
    /// Redfish: `url`, `username`, `password`, optional `system` and
    /// `insecure`. libvirt: `domain`, optional `socket`. The password is
    /// write-only; omit it to keep the stored one.
    #[serde(default)]
    config: serde_json::Value,
}

#[utoipa::path(get, path = "/hosts/{id}/fencing", tag = "hosts", params(("id" = HostId, Path)),
    responses((status = 200, body = Fencing), (status = 403), (status = 404)))]
async fn get_fencing(
    State(state): State<AppState>,
    p: Principal,
    axum::extract::Path(id): axum::extract::Path<HostId>,
) -> ApiResult<Json<Fencing>> {
    authorize(&state.db, &p, Perm::InfraRead, p.tenant_id).await?;
    hm_db::hosts::get(&state.db, id).await?;
    Ok(Json(match hm_db::hosts::fencing(&state.db, id).await? {
        Some((kind, mut config)) => {
            for f in crate::fence::SECRET_FIELDS {
                if config.get(*f).is_some() {
                    config[*f] = "(set)".into();
                }
            }
            Fencing {
                kind: Some(kind),
                config,
            }
        }
        None => Fencing {
            kind: None,
            config: serde_json::json!({}),
        },
    }))
}

#[utoipa::path(put, path = "/hosts/{id}/fencing", tag = "hosts", params(("id" = HostId, Path)), request_body = Fencing,
    responses((status = 200, body = Fencing), (status = 400), (status = 403), (status = 404)))]
async fn set_fencing(
    State(state): State<AppState>,
    p: Principal,
    axum::extract::Path(id): axum::extract::Path<HostId>,
    Json(req): Json<Fencing>,
) -> ApiResult<Json<Fencing>> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let host = hm_db::hosts::get(&state.db, id).await?;
    match &req.kind {
        None => hm_db::hosts::set_fencing(&state.db, id, None).await?,
        Some(kind) => {
            let mut config = req.config.clone();
            // Write-only secrets: keep the stored ones unless replaced.
            if let Some((old_kind, old)) = hm_db::hosts::fencing(&state.db, id).await?
                && &old_kind == kind
            {
                for f in crate::fence::SECRET_FIELDS {
                    if config.get(*f).is_none_or(|v| v.as_str() == Some("(set)"))
                        && let Some(v) = old.get(*f)
                    {
                        config[*f] = v.clone();
                    }
                }
            }
            let opened = crate::ha::open_config(id, &config).map_err(ApiError::internal)?;
            crate::fence::validate(kind, &opened).map_err(ApiError::bad_request)?;
            for f in crate::fence::SECRET_FIELDS {
                if let Some(v) = config.get(*f).and_then(|v| v.as_str())
                    && !hm_pki::is_sealed(v)
                {
                    config[*f] = crate::secrets::seal(&crate::fence::secret_context(id), v).into();
                }
            }
            hm_db::hosts::set_fencing(&state.db, id, Some((kind, &config))).await?;
        }
    }
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "host.fencing",
        Some(("host", id.0)),
        serde_json::json!({"name": host.name, "kind": req.kind}),
    )
    .await;
    get_fencing(State(state), p, axum::extract::Path(id)).await
}

#[derive(Serialize, ToSchema)]
pub struct FencingTest {
    /// The host's power state through the fencing channel: `on` or `off`.
    power: String,
}

/// Checks the fencing channel works, by reading the host's power state. It
/// does not power anything off.
#[utoipa::path(post, path = "/hosts/{id}/fencing/test", tag = "hosts", params(("id" = HostId, Path)),
    responses((status = 200, body = FencingTest), (status = 403), (status = 404), (status = 409), (status = 503)))]
async fn test_fencing(
    State(state): State<AppState>,
    p: Principal,
    axum::extract::Path(id): axum::extract::Path<HostId>,
) -> ApiResult<Json<FencingTest>> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let (kind, config) = hm_db::hosts::fencing(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::conflict("the host has no fencing configured"))?;
    let config = crate::ha::open_config(id, &config).map_err(ApiError::internal)?;
    let power = crate::fence::power_state(&kind, &config)
        .await
        .map_err(|e| ApiError::unavailable(format!("{e:#}")))?;
    Ok(Json(FencingTest { power }))
}

/// Takes a host out of service for good: it leaves its cluster, its
/// certificates are revoked (its agent can no longer connect, and other
/// hosts refuse it), and it stays listed as decommissioned. It must have no
/// VMs. To use the machine again, enroll it anew.
#[utoipa::path(post, path = "/hosts/{id}/decommission", tag = "hosts", params(("id" = HostId, Path)),
    responses((status = 204), (status = 403), (status = 404), (status = 409)))]
async fn decommission_host(
    State(state): State<AppState>,
    p: Principal,
    axum::extract::Path(id): axum::extract::Path<HostId>,
) -> ApiResult<StatusCode> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let host = hm_db::hosts::get(&state.db, id).await?;
    if host.state == "decommissioned" {
        return Ok(StatusCode::NO_CONTENT);
    }
    if !hm_db::vms::list_on_host(&state.db, id).await?.is_empty() {
        return Err(ApiError::conflict(
            "move or delete the host's VMs before decommissioning it",
        ));
    }
    let revoked = hm_db::hosts::decommission(&state.db, id).await?;
    crate::crl::revoke(&state, id, &revoked, "decommissioned").await;
    state.agents.close(id);
    tokio::spawn(crate::agents::libvirt_tls_policy(
        state.clone(),
        host.cluster_id,
        id,
    ));
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "host.decommission",
        Some(("host", id.0)),
        serde_json::json!({"name": host.name, "revoked": revoked.len()}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Has the host's agent renew its certificate now (it also does so on its
/// own before it expires). The agent reconnects with the new one.
#[utoipa::path(post, path = "/hosts/{id}/renew-certificate", tag = "hosts", params(("id" = HostId, Path)),
    responses((status = 204), (status = 403), (status = 404), (status = 503)))]
async fn renew_certificate(
    State(state): State<AppState>,
    p: Principal,
    axum::extract::Path(id): axum::extract::Path<HostId>,
) -> ApiResult<StatusCode> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let host = hm_db::hosts::get(&state.db, id).await?;
    state
        .agents
        .call(
            id,
            hm_proto::command::Cmd::RenewCertificate(hm_proto::RenewCertificate {}),
            Duration::from_secs(60),
        )
        .await
        .map_err(|e| ApiError::unavailable(format!("renewing {}'s certificate: {e}", host.name)))?;
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "host.renew_certificate",
        Some(("host", id.0)),
        serde_json::json!({"name": host.name}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}
