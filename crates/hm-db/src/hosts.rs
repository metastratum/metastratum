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
    pub agent_version: String,
    pub cert_fingerprint: String,
    pub cert_expires_at: DateTime<Utc>,
    pub connected_to: Option<String>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

macro_rules! host_columns {
    () => {
        "id, name, host(address) AS address, state, cpu_count, memory_mib, cpu_model, kvm, \
         libvirt_version, ovn_chassis, agent_version, cert_fingerprint, cert_expires_at, \
         connected_to, last_seen_at, created_at"
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
                            ovn_chassis, agent_version, cert_fingerprint, cert_expires_at)
         VALUES ($1, $2, $3::inet, $4, $5, $6, $7, $8, $9, $10, $11, $12) RETURNING ",
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
    .fetch_one(db)
    .await
    .map_err(|e| conflict_on_unique(e, "host"))
}

pub async fn update_facts(db: impl PgExecutor<'_>, id: HostId, f: &Facts<'_>) -> Result<()> {
    sqlx::query(
        "UPDATE hosts SET name = $2, address = $3::inet, cpu_count = $4, memory_mib = $5, cpu_model = $6,
                kvm = $7, libvirt_version = $8, ovn_chassis = $9, agent_version = $10, updated_at = now()
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
    sqlx::query("UPDATE hosts SET connected_to = $2, last_seen_at = now() WHERE id = $1")
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
