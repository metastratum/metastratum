use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use mst_core::ids::{ClusterId, HostId};
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
        .routes(routes!(accept_facts))
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
    /// What the host reports about itself that would give it more than it
    /// was accepted with (`name`, `address`, `ovn_chassis`, `cpu_count`,
    /// `memory_mib`, `physical_networks`), waiting for an administrator to
    /// accept it. Until then the accepted values hold.
    #[schema(value_type = Option<Object>)]
    pending_facts: Option<serde_json::Value>,
}

#[utoipa::path(get, path = "/hosts", tag = "hosts", responses((status = 200, body = Vec<HostView>), (status = 403)))]
async fn list_hosts(State(state): State<AppState>, p: Principal) -> ApiResult<Json<Vec<HostView>>> {
    authorize(&state.db, &p, Perm::InfraRead, p.tenant_id).await?;
    let clusters: std::collections::HashMap<_, _> = mst_db::clusters::list(&state.db)
        .await?
        .into_iter()
        .map(|c| (c.id, c.name))
        .collect();
    let mut out = vec![];
    for h in mst_db::hosts::list(&state.db).await? {
        let vms = mst_db::vms::list_on_host(&state.db, h.id).await?;
        out.push(HostView {
            cluster_name: h.cluster_id.and_then(|c| clusters.get(&c).cloned()),
            cluster_id: h.cluster_id,
            connected: h.is_live(),
            migration_ready: h.migration_ready,
            controller: h.connected_to.clone(),
            connected_since: h.connected_since,
            fence_kind: h.fence_kind.clone(),
            vm_count: vms.len(),
            // What is kept for its VMs: as much as each may grow to.
            allocated_memory_mib: vms.iter().map(|v| v.spec.0.reserved_memory_mib()).sum(),
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
            pending_facts: h.pending_facts,
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
    /// For the agent's join_token setting. Shown only once.
    token: String,
    expires_at: DateTime<Utc>,
    /// The controllers' agent endpoints, for the agent's controllers
    /// setting: where the controllers reach each other, which hosts on the
    /// management network can reach too.
    controllers: Vec<String>,
}

pub async fn issue_join_token(
    state: &AppState,
    created_by: Option<mst_core::ids::UserId>,
    ttl: Duration,
    cluster: Option<ClusterId>,
) -> ApiResult<JoinTokenView> {
    if let Some(c) = cluster {
        mst_db::clusters::get(&state.db, c).await?;
    }
    let token = mst_pki::JoinToken::generate(&state.ca.fingerprint());
    let expires_at = Utc::now() + chrono::Duration::from_std(ttl).map_err(ApiError::internal)?;
    mst_db::join_tokens::create(
        &state.db,
        &token.id,
        &token.secret_hash(),
        created_by,
        expires_at,
        cluster,
    )
    .await?;
    let controllers = mst_db::replicas::live(&state.db)
        .await?
        .into_iter()
        .map(|(_, address)| format!("https://{address}"))
        .collect();
    Ok(JoinTokenView {
        token: token.to_string(),
        expires_at,
        controllers,
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
    let host = mst_db::hosts::get(&state.db, id).await?;
    if let Some(cluster) = req.cluster_id {
        if let Some(c) = cluster {
            mst_db::clusters::get(&state.db, c).await?;
        }
        if cluster != host.cluster_id && !mst_db::vms::list_on_host(&state.db, id).await?.is_empty()
        {
            return Err(ApiError::conflict(
                "move or delete the host's VMs before changing its cluster",
            ));
        }
        mst_db::hosts::set_cluster(&state.db, id, cluster).await?;
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
        mst_db::hosts::set_state(&state.db, id, s).await?;
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
    task_id: mst_core::ids::TaskId,
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
    let host = mst_db::hosts::get(&state.db, id).await?;
    if host.cluster_id.is_none() {
        return Err(ApiError::conflict(
            "the host is in no cluster, so its VMs have nowhere to go",
        ));
    }
    let mut tx = state.db.begin().await?;
    mst_db::hosts::set_state(&mut *tx, id, "maintenance").await?;
    let task = mst_db::tasks::enqueue(
        &mut *tx,
        mst_db::tasks::NewTask {
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
    mst_db::hosts::get(&state.db, id).await?;
    Ok(Json(match mst_db::hosts::fencing(&state.db, id).await? {
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
    let host = mst_db::hosts::get(&state.db, id).await?;
    match &req.kind {
        None => mst_db::hosts::set_fencing(&state.db, id, None).await?,
        Some(kind) => {
            let mut config = req.config.clone();
            // Write-only secrets: keep the stored ones unless replaced.
            if let Some((old_kind, old)) = mst_db::hosts::fencing(&state.db, id).await?
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
                    && !mst_pki::is_sealed(v)
                {
                    config[*f] = crate::secrets::seal(&crate::fence::secret_context(id), v).into();
                }
            }
            mst_db::hosts::set_fencing(&state.db, id, Some((kind, &config))).await?;
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
    let (kind, config) = mst_db::hosts::fencing(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::conflict("the host has no fencing configured"))?;
    let config = crate::ha::open_config(id, &config).map_err(ApiError::internal)?;
    let power = crate::fence::power_state(&kind, &config)
        .await
        .map_err(|e| ApiError::unavailable(format!("{e:#}")))?;
    Ok(Json(FencingTest { power }))
}

/// What decommissioning a host did.
#[derive(Serialize, ToSchema)]
pub struct DecommissionReport {
    /// What the host's agent removed from it, when wiped (nothing if it
    /// could not be reached).
    host_cleanup: Vec<String>,
    /// Cleanup steps that failed on the host.
    host_failures: Vec<String>,
    certificates_revoked: usize,
    /// What is left to do by hand.
    notes: Vec<String>,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct DecommissionQuery {
    /// Also remove metastratum from the machine (its agent does, if
    /// connected). Without it the host is only unlinked: retired in
    /// metastratum, the machine left as it is.
    #[serde(default)]
    wipe: bool,
}

/// Takes a host out of service for good; it must have no VMs, nor disks on
/// its storage. It leaves its
/// cluster and its certificates are revoked, so its agent cannot connect
/// again. With `wipe`, its agent first removes metastratum from the machine:
/// lets go of storage pools, deletes cached images, local volumes, seeds,
/// metastratum's TLS files and libvirt secrets, puts back the OVN southbound
/// settings metastratum changed (OVN itself, which the admin set up, stays),
/// removes its own identity and stops. To use the machine again, enroll it
/// anew.
#[utoipa::path(post, path = "/hosts/{id}/decommission", tag = "hosts", params(("id" = HostId, Path), DecommissionQuery),
    responses((status = 200, body = DecommissionReport), (status = 403), (status = 404), (status = 409)))]
async fn decommission_host(
    State(state): State<AppState>,
    p: Principal,
    axum::extract::Path(id): axum::extract::Path<HostId>,
    axum::extract::Query(q): axum::extract::Query<DecommissionQuery>,
) -> ApiResult<Json<DecommissionReport>> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let host = mst_db::hosts::get(&state.db, id).await?;
    if host.state == "decommissioned" {
        return Err(ApiError::conflict("the host is already decommissioned"));
    }
    if !mst_db::vms::list_on_host(&state.db, id).await?.is_empty() {
        return Err(ApiError::conflict(
            "move or delete the host's VMs before decommissioning it",
        ));
    }
    // Disks attached to no VM, on its own storage: wiping it would take them.
    let disks = mst_db::disks::count_on_host(&state.db, id).await?
        + mst_db::disks::moving_to_host(&state.db, id).await?;
    if disks > 0 {
        return Err(ApiError::conflict(format!(
            "{disks} disks are on the host's storage: move or delete them before decommissioning it"
        )));
    }
    let mut report = DecommissionReport {
        host_cleanup: vec![],
        host_failures: vec![],
        certificates_revoked: 0,
        notes: vec![],
    };

    if !q.wipe {
        report.notes.push(
            "The machine was left as it is: metastratum's files, mounts, TLS settings and agent stay on it. Its agent can no longer connect; before enrolling it again, remove its identity (/var/lib/metastratum-agent)."
                .into(),
        );
    } else if host.is_live() {
        // The host wipes itself while its certificate still opens a session.
        let cmd = mst_proto::command::Cmd::Decommission(mst_proto::Decommission {});
        match state.agents.send(id, cmd, Duration::from_secs(300)).await {
            Ok(payload) => {
                #[derive(serde::Deserialize, Default)]
                struct Steps {
                    done: Vec<String>,
                    failed: Vec<String>,
                }
                let steps: Steps = serde_json::from_str(&payload).unwrap_or_default();
                report.host_cleanup = steps.done;
                report.host_failures = steps.failed;
            }
            // VMs defined on the host that the controller does not know of.
            Err(crate::agents::AgentError::Failed(e)) if e.contains("still defined") => {
                return Err(ApiError::conflict(format!("the host refused: {e}")));
            }
            Err(e) => report.notes.push(format!(
                "The host could not remove metastratum ({e}): its files, mounts and settings stay on it."
            )),
        }
    } else {
        report.notes.push(
            "The host's agent is not connected, so nothing on the machine was removed; reinstall it before reuse."
                .into(),
        );
    }

    let revoked = mst_db::hosts::decommission(&state.db, id).await?;
    report.certificates_revoked = revoked.len();
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
        serde_json::json!({"name": host.name, "wipe": q.wipe, "revoked": report.certificates_revoked,
            "host_cleanup": report.host_cleanup, "host_failures": report.host_failures,
            "notes": report.notes}),
    )
    .await;
    Ok(Json(report))
}

/// Has the host's agent renew its certificate now (it also does so on its
/// own before it expires). The agent reconnects with the new one.
/// Accepts what a host reported about itself that waited for an
/// administrator (`pending_facts`): a new name, address or OVN chassis,
/// physical networks, more CPUs or memory.
#[utoipa::path(post, path = "/hosts/{id}/accept-facts", tag = "hosts", params(("id" = HostId, Path)),
    responses((status = 204), (status = 403), (status = 404), (status = 409)))]
async fn accept_facts(
    State(state): State<AppState>,
    p: Principal,
    axum::extract::Path(id): axum::extract::Path<HostId>,
) -> ApiResult<StatusCode> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let host = mst_db::hosts::get(&state.db, id).await?;
    let Some(pending) = host.pending_facts else {
        return Err(ApiError::conflict("the host has no changes waiting"));
    };
    mst_db::hosts::accept_pending_facts(&state.db, id).await?;
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "host.accept_facts",
        Some(("host", id.0)),
        serde_json::json!({"name": host.name, "accepted": pending}),
    )
    .await;
    // Gateways may move, and ports follow a renamed chassis.
    state.net.kick();
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(post, path = "/hosts/{id}/renew-certificate", tag = "hosts", params(("id" = HostId, Path)),
    responses((status = 204), (status = 403), (status = 404), (status = 503)))]
async fn renew_certificate(
    State(state): State<AppState>,
    p: Principal,
    axum::extract::Path(id): axum::extract::Path<HostId>,
) -> ApiResult<StatusCode> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let host = mst_db::hosts::get(&state.db, id).await?;
    if host.state == "decommissioned" {
        return Err(ApiError::conflict("the host is decommissioned"));
    }
    state
        .agents
        .call(
            id,
            mst_proto::command::Cmd::RenewCertificate(mst_proto::RenewCertificate {}),
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
