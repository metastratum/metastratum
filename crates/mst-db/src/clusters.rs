//! Clusters, and the host snapshot the scheduler places VMs with.

use chrono::{DateTime, Utc};
use mst_core::ids::{ClusterId, HostId};
use sqlx::{PgConnection, PgExecutor};

use crate::{Result, conflict_on_unique};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Cluster {
    pub id: ClusterId,
    pub name: String,
    pub cpu_overcommit: f32,
    pub memory_overcommit: f32,
    /// Restart failed hosts' VMs elsewhere (after fencing the host).
    pub ha_enabled: bool,
    /// Per-VM limits (none when unset): network Mbit/s each way per NIC,
    /// disk operations and MB/s per disk.
    pub vm_net_mbps: Option<i32>,
    pub vm_disk_iops: Option<i32>,
    pub vm_disk_mbps: Option<i32>,
    pub created_at: DateTime<Utc>,
}

pub async fn create(
    db: impl PgExecutor<'_>,
    name: &str,
    cpu: f32,
    memory: f32,
    ha: bool,
) -> Result<Cluster> {
    sqlx::query_as(
        "INSERT INTO clusters (id, name, cpu_overcommit, memory_overcommit, ha_enabled) VALUES ($1, $2, $3, $4, $5)
         RETURNING id, name, cpu_overcommit, memory_overcommit, ha_enabled, vm_net_mbps, vm_disk_iops, vm_disk_mbps, created_at",
    )
    .bind(ClusterId::new())
    .bind(name)
    .bind(cpu)
    .bind(memory)
    .bind(ha)
    .fetch_one(db)
    .await
    .map_err(|e| conflict_on_unique(e, "a cluster with this name"))
}

pub async fn get(db: impl PgExecutor<'_>, id: ClusterId) -> Result<Cluster> {
    sqlx::query_as("SELECT id, name, cpu_overcommit, memory_overcommit, ha_enabled, vm_net_mbps, vm_disk_iops, vm_disk_mbps, created_at FROM clusters WHERE id = $1")
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| mst_core::Error::NotFound { kind: "cluster", id: id.to_string() }.into())
}

pub async fn list(db: impl PgExecutor<'_>) -> Result<Vec<Cluster>> {
    Ok(sqlx::query_as("SELECT id, name, cpu_overcommit, memory_overcommit, ha_enabled, vm_net_mbps, vm_disk_iops, vm_disk_mbps, created_at FROM clusters ORDER BY name")
        .fetch_all(db)
        .await?)
}

pub async fn update(
    db: impl PgExecutor<'_>,
    id: ClusterId,
    name: &str,
    cpu: f32,
    memory: f32,
    ha: bool,
) -> Result<()> {
    sqlx::query(
        "UPDATE clusters SET name = $2, cpu_overcommit = $3, memory_overcommit = $4, ha_enabled = $5 WHERE id = $1",
    )
    .bind(id)
    .bind(name)
    .bind(cpu)
    .bind(memory)
    .bind(ha)
    .execute(db)
    .await
    .map_err(|e| conflict_on_unique(e, "a cluster with this name"))?;
    Ok(())
}

/// Sets the per-VM limits (`None`: no limit).
pub async fn set_limits(
    db: impl PgExecutor<'_>,
    id: ClusterId,
    net_mbps: Option<i32>,
    disk_iops: Option<i32>,
    disk_mbps: Option<i32>,
) -> Result<()> {
    sqlx::query(
        "UPDATE clusters SET vm_net_mbps = $2, vm_disk_iops = $3, vm_disk_mbps = $4 WHERE id = $1",
    )
    .bind(id)
    .bind(net_mbps)
    .bind(disk_iops)
    .bind(disk_mbps)
    .execute(db)
    .await?;
    Ok(())
}

/// The disk limits of the cluster a host is in: (IOPS, MB/s) per disk.
pub async fn disk_limits_of_host(
    db: impl PgExecutor<'_>,
    host: HostId,
) -> Result<(Option<i32>, Option<i32>)> {
    Ok(sqlx::query_as(
        "SELECT c.vm_disk_iops, c.vm_disk_mbps FROM hosts h JOIN clusters c ON c.id = h.cluster_id
         WHERE h.id = $1",
    )
    .bind(host)
    .fetch_optional(db)
    .await?
    .unwrap_or((None, None)))
}

/// Deletes an empty cluster, and grants of it. Hosts must be moved out
/// first.
pub async fn delete(db: &mut PgConnection, id: ClusterId) -> Result<()> {
    let hosts: i64 = sqlx::query_scalar("SELECT count(*) FROM hosts WHERE cluster_id = $1")
        .bind(id)
        .fetch_one(&mut *db)
        .await?;
    if hosts > 0 {
        return Err(
            mst_core::Error::Conflict(format!("the cluster still has {hosts} hosts")).into(),
        );
    }
    sqlx::query("DELETE FROM tenant_grants WHERE kind = 'cluster' AND ref_id = $1")
        .bind(id)
        .execute(&mut *db)
        .await?;
    sqlx::query("DELETE FROM clusters WHERE id = $1")
        .bind(id)
        .execute(&mut *db)
        .await?;
    Ok(())
}

/// What the scheduler needs to know about a host.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Candidate {
    pub id: HostId,
    pub name: String,
    pub cluster_id: Option<ClusterId>,
    pub state: String,
    pub kvm: bool,
    pub cpu_count: i32,
    pub memory_mib: i64,
    pub live: bool,
    pub cpu_overcommit: f32,
    pub memory_overcommit: f32,
    pub used_vcpus: i64,
    pub used_memory_mib: i64,
    /// Accepts live migrations (libvirt TLS is set up).
    pub migration_ready: bool,
}

/// All hosts with their allocations, locked until the transaction ends so
/// concurrent placements see each other's allocations.
pub async fn candidates_locked(db: &mut PgConnection) -> Result<Vec<Candidate>> {
    Ok(sqlx::query_as(
        "SELECT h.id, h.name, h.cluster_id, h.state, h.kvm, h.cpu_count, h.memory_mib, h.migration_ready,
                (h.connected_to IS NOT NULL AND h.last_seen_at > now() - interval '30 seconds') AS live,
                coalesce(c.cpu_overcommit, 1) AS cpu_overcommit,
                coalesce(c.memory_overcommit, 1) AS memory_overcommit,
                -- VMs on the host, plus those migrating to it.
                (SELECT coalesce(sum((v.spec->>'vcpus')::bigint), 0)::bigint FROM vms v WHERE v.host_id = h.id)
                  + (SELECT coalesce(sum(m.vcpus), 0)::bigint FROM migrations m WHERE m.dest_host_id = h.id)
                    AS used_vcpus,
                (SELECT coalesce(sum(greatest((v.spec->>'memory_mib')::bigint, coalesce((v.spec->>'max_memory_mib')::bigint, 0))), 0)::bigint FROM vms v WHERE v.host_id = h.id)
                  + (SELECT coalesce(sum(m.memory_mib), 0)::bigint FROM migrations m WHERE m.dest_host_id = h.id)
                    AS used_memory_mib
         FROM hosts h LEFT JOIN clusters c ON c.id = h.cluster_id
         ORDER BY h.id
         FOR UPDATE OF h",
    )
    .fetch_all(db)
    .await?)
}
