//! Clusters: groups of hosts VMs are placed within.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use hm_core::ids::{ClusterId, TenantId};
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
        .routes(routes!(list_clusters, create_cluster))
        .routes(routes!(update_cluster, delete_cluster))
        .routes(routes!(usable_clusters))
}

#[derive(Serialize, ToSchema)]
pub struct ClusterView {
    id: ClusterId,
    name: String,
    /// vCPUs the scheduler may allocate per physical CPU.
    cpu_overcommit: f32,
    /// Memory the scheduler may allocate per MiB of host memory.
    memory_overcommit: f32,
    /// When a host fails, it is fenced (powered off) and its VMs on shared
    /// storage restart on the cluster's other hosts.
    ha_enabled: bool,
    /// Limits for each VM in the cluster, so VMs sharing hosts and storage
    /// cannot starve each other. Absent: no limit.
    #[serde(flatten)]
    limits: VmLimits,
    created_at: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct VmLimits {
    /// Network traffic per NIC, each way, in Mbit/s.
    vm_net_mbps: Option<u32>,
    /// Operations per second, per disk.
    vm_disk_iops: Option<u32>,
    /// Throughput per disk, in MB/s.
    vm_disk_mbps: Option<u32>,
}

impl VmLimits {
    fn validate(self) -> ApiResult<Self> {
        let ok = |v: Option<u32>, max: u32| v.is_none_or(|v| (1..=max).contains(&v));
        if !(ok(self.vm_net_mbps, 400_000)
            && ok(self.vm_disk_iops, 10_000_000)
            && ok(self.vm_disk_mbps, 100_000))
        {
            return Err(ApiError::bad_request(
                "limits must be positive: network up to 400000 Mbit/s, disks up to 10000000 IOPS and 100000 MB/s",
            ));
        }
        Ok(self)
    }

    fn db(self) -> (Option<i32>, Option<i32>, Option<i32>) {
        let i = |v: Option<u32>| v.map(|v| v as i32);
        (
            i(self.vm_net_mbps),
            i(self.vm_disk_iops),
            i(self.vm_disk_mbps),
        )
    }
}

impl From<hm_db::clusters::Cluster> for ClusterView {
    fn from(c: hm_db::clusters::Cluster) -> Self {
        Self {
            id: c.id,
            name: c.name,
            cpu_overcommit: c.cpu_overcommit,
            memory_overcommit: c.memory_overcommit,
            ha_enabled: c.ha_enabled,
            limits: VmLimits {
                vm_net_mbps: c.vm_net_mbps.map(|v| v as u32),
                vm_disk_iops: c.vm_disk_iops.map(|v| v as u32),
                vm_disk_mbps: c.vm_disk_mbps.map(|v| v as u32),
            },
            created_at: c.created_at,
        }
    }
}

#[utoipa::path(get, path = "/clusters", tag = "hosts", responses((status = 200, body = Vec<ClusterView>), (status = 403)))]
async fn list_clusters(
    State(state): State<AppState>,
    p: Principal,
) -> ApiResult<Json<Vec<ClusterView>>> {
    authorize(&state.db, &p, Perm::InfraRead, p.tenant_id).await?;
    Ok(Json(
        hm_db::clusters::list(&state.db)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

#[derive(Deserialize, ToSchema)]
pub struct ClusterSpec {
    name: String,
    /// Default 4.
    cpu_overcommit: Option<f32>,
    /// Default 1 (no memory overcommit).
    memory_overcommit: Option<f32>,
    /// Restart a failed host's VMs elsewhere (default off). Hosts need
    /// fencing configured.
    #[serde(default)]
    ha_enabled: bool,
    #[serde(flatten)]
    limits: VmLimits,
}

fn validate(req: &ClusterSpec) -> ApiResult<(String, f32, f32)> {
    let name = req.name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(ApiError::bad_request("name must be 1-64 characters"));
    }
    let cpu = req.cpu_overcommit.unwrap_or(4.0);
    let mem = req.memory_overcommit.unwrap_or(1.0);
    if !(0.1..=64.0).contains(&cpu) || !(0.1..=4.0).contains(&mem) {
        return Err(ApiError::bad_request(
            "overcommit ratios must be 0.1-64 for CPU and 0.1-4 for memory",
        ));
    }
    Ok((name.to_owned(), cpu, mem))
}

#[utoipa::path(post, path = "/clusters", tag = "hosts", request_body = ClusterSpec,
    responses((status = 201, body = ClusterView), (status = 400), (status = 403), (status = 409)))]
async fn create_cluster(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<ClusterSpec>,
) -> ApiResult<(StatusCode, Json<ClusterView>)> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let (name, cpu, mem) = validate(&req)?;
    let limits = req.limits.validate()?;
    let c = hm_db::clusters::create(&state.db, &name, cpu, mem, req.ha_enabled).await?;
    let (net, iops, mbps) = limits.db();
    hm_db::clusters::set_limits(&state.db, c.id, net, iops, mbps).await?;
    let c = hm_db::clusters::get(&state.db, c.id).await?;
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "cluster.create",
        Some(("cluster", c.id.0)),
        json!({"name": name}),
    )
    .await;
    Ok((StatusCode::CREATED, Json(c.into())))
}

#[utoipa::path(put, path = "/clusters/{id}", tag = "hosts", params(("id" = ClusterId, Path)), request_body = ClusterSpec,
    responses((status = 200, body = ClusterView), (status = 400), (status = 403), (status = 404)))]
async fn update_cluster(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<ClusterId>,
    Json(req): Json<ClusterSpec>,
) -> ApiResult<Json<ClusterView>> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let before = ClusterView::from(hm_db::clusters::get(&state.db, id).await?).limits;
    let (name, cpu, mem) = validate(&req)?;
    let limits = req.limits.validate()?;
    hm_db::clusters::update(&state.db, id, &name, cpu, mem, req.ha_enabled).await?;
    let (net, iops, mbps) = limits.db();
    hm_db::clusters::set_limits(&state.db, id, net, iops, mbps).await?;
    if limits.vm_net_mbps != before.vm_net_mbps {
        state.net.kick();
    }
    if (limits.vm_disk_iops, limits.vm_disk_mbps) != (before.vm_disk_iops, before.vm_disk_mbps) {
        tokio::spawn(crate::limits::apply_disk_limits(state.clone(), id));
    }
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "cluster.update",
        Some(("cluster", id.0)),
        json!({"name": name, "cpu_overcommit": cpu, "memory_overcommit": mem, "limits": limits}),
    )
    .await;
    Ok(Json(hm_db::clusters::get(&state.db, id).await?.into()))
}

#[utoipa::path(delete, path = "/clusters/{id}", tag = "hosts", params(("id" = ClusterId, Path)),
    responses((status = 204), (status = 403), (status = 404), (status = 409, description = "Still has hosts")))]
async fn delete_cluster(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<ClusterId>,
) -> ApiResult<StatusCode> {
    authorize(&state.db, &p, Perm::InfraWrite, p.tenant_id).await?;
    let c = hm_db::clusters::get(&state.db, id).await?;
    let mut tx = state.db.begin().await?;
    hm_db::clusters::delete(&mut tx, id).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "cluster.delete",
        Some(("cluster", id.0)),
        json!({"name": c.name}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize, ToSchema)]
pub struct UsableCluster {
    id: ClusterId,
    name: String,
}

/// Clusters the tenant may place VMs in.
#[utoipa::path(get, path = "/tenants/{id}/clusters", tag = "tenants", params(("id" = TenantId, Path)),
    responses((status = 200, body = Vec<UsableCluster>), (status = 404)))]
async fn usable_clusters(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TenantId>,
) -> ApiResult<Json<Vec<UsableCluster>>> {
    authorize_resource(&state.db, &p, Perm::VmRead, id, "tenant").await?;
    let allowed = allowed_clusters(&state, id).await?;
    Ok(Json(
        hm_db::clusters::list(&state.db)
            .await?
            .into_iter()
            .filter(|c| allowed.as_ref().is_none_or(|a| a.contains(&c.id)))
            .map(|c| UsableCluster {
                id: c.id,
                name: c.name,
            })
            .collect(),
    ))
}

/// Clusters a tenant may use; `None` for any (the root tenant).
pub async fn allowed_clusters(
    state: &AppState,
    tenant: TenantId,
) -> ApiResult<Option<std::collections::HashSet<ClusterId>>> {
    let t = hm_db::tenants::get(&state.db, tenant).await?;
    if t.parent_id.is_none() {
        return Ok(None);
    }
    Ok(Some(
        hm_db::grants::list(&state.db, tenant, "cluster")
            .await?
            .into_iter()
            .map(ClusterId)
            .collect(),
    ))
}
