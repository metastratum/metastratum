use chrono::{DateTime, Utc};
use mst_core::ids::HostId;
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
    /// The protocol revision its agent speaks.
    pub agent_protocol: i32,
    pub agent_version: String,
    pub cert_fingerprint: String,
    pub cert_expires_at: DateTime<Utc>,
    pub cluster_id: Option<mst_core::ids::ClusterId>,
    pub connected_to: Option<String>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// Facts the host reported that wait for an administrator to accept
    /// them (see [`update_facts`]): the reported values, by field.
    pub pending_facts: Option<serde_json::Value>,
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
         cluster_id, connected_to, last_seen_at, created_at, physical_networks, cpu_json, migration_ready, fence_kind, connected_since, agent_protocol, pending_facts"
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
    /// The host-model CPU as `mst_core::vm::CpuSpec` JSON; empty if unknown.
    pub cpu_json: &'a str,
    pub migration_ready: bool,
}

pub async fn create(
    db: impl PgExecutor<'_>,
    id: HostId,
    f: &Facts<'_>,
    cert_fingerprint: &str,
    cert_expires_at: DateTime<Utc>,
    cert_serial: &str,
) -> Result<Host> {
    sqlx::query_as(concat!(
        "INSERT INTO hosts (id, name, address, cpu_count, memory_mib, cpu_model, kvm, libvirt_version,
                            ovn_chassis, agent_version, cert_fingerprint, cert_expires_at, physical_networks,
                            cpu_json, migration_ready, cert_serial)
         VALUES ($1, $2, $3::inet, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16) RETURNING ",
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
    .bind(cert_serial)
    .fetch_one(db)
    .await
    .map_err(|e| conflict_on_unique(e, "host"))
}

/// Records what a host reports about itself. `f` holds what is accepted of
/// it; `pending`, what waits for an administrator (or none).
pub async fn update_facts(
    db: impl PgExecutor<'_>,
    id: HostId,
    f: &Facts<'_>,
    pending: Option<&serde_json::Value>,
) -> Result<()> {
    sqlx::query(
        "UPDATE hosts SET name = $2, address = $3::inet, cpu_count = $4, memory_mib = $5, cpu_model = $6,
                kvm = $7, libvirt_version = $8, ovn_chassis = $9, agent_version = $10,
                physical_networks = $11, cpu_json = $12, migration_ready = $13,
                pending_facts = $14, updated_at = now()
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
    .bind(pending)
    .execute(db)
    .await
    .map_err(|e| conflict_on_unique(e, "host"))?;
    Ok(())
}

/// Applies the facts a host reported that waited for an administrator.
pub async fn accept_pending_facts(db: impl PgExecutor<'_>, id: HostId) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE hosts SET
             name = coalesce(pending_facts->>'name', name),
             address = coalesce((pending_facts->>'address')::inet, address),
             ovn_chassis = coalesce(pending_facts->>'ovn_chassis', ovn_chassis),
             cpu_count = coalesce((pending_facts->>'cpu_count')::int, cpu_count),
             memory_mib = coalesce((pending_facts->>'memory_mib')::bigint, memory_mib),
             physical_networks = CASE WHEN pending_facts ? 'physical_networks'
                 THEN ARRAY(SELECT jsonb_array_elements_text(pending_facts->'physical_networks'))
                 ELSE physical_networks END,
             pending_facts = NULL, updated_at = now()
         WHERE id = $1 AND pending_facts IS NOT NULL",
    )
    .bind(id)
    .execute(db)
    .await
    .map_err(|e| conflict_on_unique(e, "host"))?
    .rows_affected()
        > 0)
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
        mst_core::Error::NotFound {
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

pub async fn set_agent_protocol(db: impl PgExecutor<'_>, id: HostId, version: u32) -> Result<()> {
    sqlx::query("UPDATE hosts SET agent_protocol = $2 WHERE id = $1")
        .bind(id)
        .bind(version as i32)
        .execute(db)
        .await?;
    Ok(())
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
    cluster: Option<mst_core::ids::ClusterId>,
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

/// Records a renewed certificate. The one the host renewed with stays
/// valid as well, until the host shows it has the new one
/// ([`retire_previous_cert`]); any other it held (a certificate renewed
/// before but never used) is returned for revoking, so however often a
/// host renews it holds two valid certificates at most.
pub async fn renew_cert(
    db: &mut sqlx::PgConnection,
    id: HostId,
    presented_fingerprint: &str,
    fingerprint: &str,
    expires_at: DateTime<Utc>,
    serial: &str,
) -> Result<Vec<Revocable>> {
    #[allow(clippy::type_complexity)]
    let (cur_fp, cur_serial, cur_exp, prev_fp, prev_serial, prev_exp): (
        String,
        Option<String>,
        DateTime<Utc>,
        Option<String>,
        Option<String>,
        Option<DateTime<Utc>>,
    ) = sqlx::query_as(
        "SELECT cert_fingerprint, cert_serial, cert_expires_at,
                previous_cert_fingerprint, previous_cert_serial, previous_cert_expires_at
         FROM hosts WHERE id = $1 FOR UPDATE",
    )
    .bind(id)
    .fetch_one(&mut *db)
    .await?;
    let current = (Some(cur_fp), cur_serial, Some(cur_exp));
    let previous = (prev_fp, prev_serial, prev_exp);
    let (keep, drop) = if previous.0.as_deref() == Some(presented_fingerprint) {
        (previous, current)
    } else {
        (current, previous)
    };
    sqlx::query(
        "UPDATE hosts SET previous_cert_fingerprint = $5, previous_cert_serial = $6,
                previous_cert_expires_at = $7,
                cert_fingerprint = $2, cert_expires_at = $3, cert_serial = $4, updated_at = now()
         WHERE id = $1",
    )
    .bind(id)
    .bind(fingerprint)
    .bind(expires_at)
    .bind(serial)
    .bind(keep.0)
    .bind(keep.1)
    .bind(keep.2)
    .execute(&mut *db)
    .await?;
    Ok(match drop {
        (Some(_), Some(serial), Some(expires)) => vec![(serial, expires)],
        _ => vec![],
    })
}

/// Records the serial of a host's current certificate, for certificates
/// issued before serials were kept (so they can be revoked later).
pub async fn note_cert_serial(
    db: impl PgExecutor<'_>,
    id: HostId,
    fingerprint: &str,
    serial: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE hosts SET cert_serial = $3
         WHERE id = $1 AND cert_fingerprint = $2 AND cert_serial IS NULL",
    )
    .bind(id)
    .bind(fingerprint)
    .bind(serial)
    .execute(db)
    .await?;
    Ok(())
}

/// A certificate to revoke: serial (hex) and when it expires.
pub type Revocable = (String, DateTime<Utc>);

/// Once the host connects with its current certificate, the one it
/// replaced is no longer needed: forgets it, and returns it for revoking
/// (if its serial is known).
pub async fn retire_previous_cert(
    db: impl PgExecutor<'_>,
    id: HostId,
    current_fingerprint: &str,
) -> Result<Option<Revocable>> {
    let row: Option<(Option<String>, Option<DateTime<Utc>>)> = sqlx::query_as(
        "WITH old AS (
             SELECT previous_cert_serial AS serial, previous_cert_expires_at AS expires
             FROM hosts WHERE id = $1 AND cert_fingerprint = $2
               AND previous_cert_fingerprint IS NOT NULL FOR UPDATE)
         UPDATE hosts SET previous_cert_fingerprint = NULL, previous_cert_serial = NULL,
                previous_cert_expires_at = NULL
         FROM old WHERE id = $1 RETURNING old.serial, old.expires",
    )
    .bind(id)
    .bind(current_fingerprint)
    .fetch_optional(db)
    .await?;
    Ok(row.and_then(|(s, e)| Some((s?, e?))))
}

/// Takes a host out of service for good, and returns its certificates for
/// revoking (those whose serial is known).
pub async fn decommission(db: impl PgExecutor<'_>, id: HostId) -> Result<Vec<Revocable>> {
    let row: (Option<String>, DateTime<Utc>, Option<String>, Option<DateTime<Utc>>) =
        sqlx::query_as(
            "UPDATE hosts SET state = 'decommissioned', cluster_id = NULL, connected_to = NULL,
                    previous_cert_fingerprint = NULL, updated_at = now()
             WHERE id = $1
             RETURNING cert_serial, cert_expires_at, previous_cert_serial, previous_cert_expires_at",
        )
        .bind(id)
        .fetch_one(db)
        .await?;
    let mut out = vec![];
    if let Some(s) = row.0 {
        out.push((s, row.1));
    }
    if let (Some(s), Some(e)) = (row.2, row.3) {
        out.push((s, e));
    }
    Ok(out)
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
