use chrono::{DateTime, Utc};
use hm_core::ids::HostId;
use sqlx::PgExecutor;

use crate::{Result, conflict_on_unique};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Host {
    pub id: HostId,
    pub name: String,
    pub address: String,
    pub state: String,
    pub cpu_count: i32,
    pub memory_mib: i64,
    pub cpu_model: String,
    pub kvm: bool,
    pub libvirt_version: String,
    pub ovn_chassis: String,
    pub physical_networks: Vec<String>,
    pub cpu_json: String,
    pub migration_ready: bool,
    /// `redfish` or `libvirt` when the host can be fenced.
    pub fence_kind: Option<String>,
    pub connected_since: Option<DateTime<Utc>>,
    pub agent_version: String,
    pub cert_fingerprint: String,
    pub cert_expires_at: DateTime<Utc>,
    pub cluster_id: Option<hm_core::ids::ClusterId>,
    pub connected_to: Option<String>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl Host {
    /// Whether the host's agent holds a session and has heartbeated recently,
    /// i.e. what we know about its VMs is current.
    pub fn is_live(&self) -> bool {
        self.connected_to.is_some()
            && self
                .last_seen_at
                .is_some_and(|t| Utc::now() - t < chrono::Duration::seconds(30))
    }
}

macro_rules! host_columns {
    () => {
        "id, name, host(address) AS address, state, cpu_count, memory_mib, cpu_model, kvm, \
         libvirt_version, ovn_chassis, agent_version, cert_fingerprint, cert_expires_at, \
         cluster_id, connected_to, last_seen_at, created_at, physical_networks, cpu_json, migration_ready, fence_kind, connected_since"
    };
}

/// Facts an agent reports about its host.
#[derive(Clone, Debug)]
pub struct Facts<'a> {
    pub name: &'a str,
    pub address: &'a str,
    pub cpu_count: i32,
    pub memory_mib: i64,
    pub cpu_model: &'a str,
    pub kvm: bool,
    pub libvirt_version: &'a str,
    pub ovn_chassis: &'a str,
    pub agent_version: &'a str,
    pub physical_networks: &'a [String],
    /// The host-model CPU as `hm_core::vm::CpuSpec` JSON; empty if unknown.
    pub cpu_json: &'a str,
    pub migration_ready: bool,
}

pub async fn create(
    db: impl PgExecutor<'_>,
    id: HostId,
    f: &Facts<'_>,
    cert_fingerprint: &str,
    cert_expires_at: DateTime<Utc>,
) -> Result<Host> {
    sqlx::query_as(concat!(
        "INSERT INTO hosts (id, name, address, cpu_count, memory_mib, cpu_model, kvm, libvirt_version,
                            ovn_chassis, agent_version, cert_fingerprint, cert_expires_at, physical_networks,
                            cpu_json, migration_ready)
         VALUES ($1, $2, $3::inet, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) RETURNING ",
        host_columns!()
    ))
    .bind(id)
    .bind(f.name)
    .bind(f.address)
    .bind(f.cpu_count)
    .bind(f.memory_mib)
    .bind(f.cpu_model)
    .bind(f.kvm)
    .bind(f.libvirt_version)
    .bind(f.ovn_chassis)
    .bind(f.agent_version)
    .bind(cert_fingerprint)
    .bind(cert_expires_at)
    .bind(f.physical_networks)
    .bind(f.cpu_json)
    .bind(f.migration_ready)
    .fetch_one(db)
    .await
    .map_err(|e| conflict_on_unique(e, "host"))
}

pub async fn update_facts(db: impl PgExecutor<'_>, id: HostId, f: &Facts<'_>) -> Result<()> {
    sqlx::query(
        "UPDATE hosts SET name = $2, address = $3::inet, cpu_count = $4, memory_mib = $5, cpu_model = $6,
                kvm = $7, libvirt_version = $8, ovn_chassis = $9, agent_version = $10,
                physical_networks = $11, cpu_json = $12, migration_ready = $13, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(f.name)
    .bind(f.address)
    .bind(f.cpu_count)
    .bind(f.memory_mib)
    .bind(f.cpu_model)
    .bind(f.kvm)
    .bind(f.libvirt_version)
    .bind(f.ovn_chassis)
    .bind(f.agent_version)
    .bind(f.physical_networks)
    .bind(f.cpu_json)
    .bind(f.migration_ready)
    .execute(db)
    .await
    .map_err(|e| conflict_on_unique(e, "host"))?;
    Ok(())
}

pub async fn get(db: impl PgExecutor<'_>, id: HostId) -> Result<Host> {
    sqlx::query_as(concat!(
        "SELECT ",
        host_columns!(),
        " FROM hosts WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        hm_core::Error::NotFound {
            kind: "host",
            id: id.to_string(),
        }
        .into()
    })
}

pub async fn list(db: impl PgExecutor<'_>) -> Result<Vec<Host>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        host_columns!(),
        " FROM hosts ORDER BY name"
    ))
    .fetch_all(db)
    .await?)
}

/// Records which replica holds the agent session (or that none does).
pub async fn set_connected(
    db: impl PgExecutor<'_>,
    id: HostId,
    replica: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE hosts SET connected_to = $2, last_seen_at = now(),
                connected_since = CASE WHEN $2::text IS NULL THEN NULL ELSE now() END WHERE id = $1",
    )
        .bind(id)
        .bind(replica)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn touch(db: impl PgExecutor<'_>, id: HostId) -> Result<()> {
    sqlx::query("UPDATE hosts SET last_seen_at = now() WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Clears session ownership for every host held by a replica, e.g. after it
/// restarts and its sessions are gone.
pub async fn disconnect_all(db: impl PgExecutor<'_>, replica: &str) -> Result<()> {
    sqlx::query("UPDATE hosts SET connected_to = NULL WHERE connected_to = $1")
        .bind(replica)
        .execute(db)
        .await?;
    Ok(())
}

/// Moves a host into a cluster (or out of any, with `None`).
pub async fn set_cluster(
    db: impl PgExecutor<'_>,
    id: HostId,
    cluster: Option<hm_core::ids::ClusterId>,
) -> Result<()> {
    sqlx::query("UPDATE hosts SET cluster_id = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(cluster)
        .execute(db)
        .await?;
    Ok(())
}

/// `ready` or `maintenance` (no new VMs are placed on hosts in maintenance).
pub async fn set_state(db: impl PgExecutor<'_>, id: HostId, state: &str) -> Result<()> {
    sqlx::query("UPDATE hosts SET state = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(state)
        .execute(db)
        .await?;
    Ok(())
}

/// How to fence a host: its kind and configuration (secrets sealed).
pub async fn fencing(
    db: impl PgExecutor<'_>,
    id: HostId,
) -> Result<Option<(String, serde_json::Value)>> {
    let row: Option<(Option<String>, sqlx::types::Json<serde_json::Value>)> =
        sqlx::query_as("SELECT fence_kind, fence_config FROM hosts WHERE id = $1")
            .bind(id)
            .fetch_optional(db)
            .await?;
    Ok(row.and_then(|(k, c)| k.map(|k| (k, c.0))))
}

pub async fn set_fencing(
    db: impl PgExecutor<'_>,
    id: HostId,
    fencing: Option<(&str, &serde_json::Value)>,
) -> Result<()> {
    sqlx::query("UPDATE hosts SET fence_kind = $2, fence_config = $3 WHERE id = $1")
        .bind(id)
        .bind(fencing.map(|f| f.0))
        .bind(sqlx::types::Json(
            fencing
                .map(|f| f.1.clone())
                .unwrap_or_else(|| serde_json::json!({})),
        ))
        .execute(db)
        .await?;
    Ok(())
}

/// Records a renewed certificate; the previous one stays valid as well.
pub async fn renew_cert(
    db: impl PgExecutor<'_>,
    id: HostId,
    fingerprint: &str,
    expires_at: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        "UPDATE hosts SET previous_cert_fingerprint = cert_fingerprint, cert_fingerprint = $2,
                cert_expires_at = $3, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(fingerprint)
    .bind(expires_at)
    .execute(db)
    .await?;
    Ok(())
}

/// Whether `fingerprint` is the host's certificate (or the one it replaced).
pub async fn accepts_cert(db: impl PgExecutor<'_>, id: HostId, fingerprint: &str) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM hosts WHERE id = $1 AND $2 IN (cert_fingerprint, previous_cert_fingerprint))",
    )
    .bind(id)
    .bind(fingerprint)
    .fetch_one(db)
    .await?)
}
