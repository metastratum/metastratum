use std::net::Ipv4Addr;

use chrono::{DateTime, Utc};
use hm_core::ids::{NetworkId, NicId, TenantId, VmId};
use sqlx::{PgConnection, PgExecutor};

use crate::Result;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Network {
    pub id: NetworkId,
    pub tenant_id: TenantId,
    pub name: String,
    pub cidr: String,
    pub gateway: String,
    pub created_at: DateTime<Utc>,
}

macro_rules! network_columns {
    () => {
        "id, tenant_id, name, cidr::text AS cidr, host(gateway) AS gateway, created_at"
    };
}

pub const DEFAULT_NAME: &str = "default";
const DEFAULT_CIDR: &str = "10.0.0.0/24";
const DEFAULT_GATEWAY: &str = "10.0.0.1";

pub async fn get(db: impl PgExecutor<'_>, id: NetworkId) -> Result<Network> {
    sqlx::query_as(concat!(
        "SELECT ",
        network_columns!(),
        " FROM networks WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        hm_core::Error::NotFound {
            kind: "network",
            id: id.to_string(),
        }
        .into()
    })
}

pub async fn list(db: impl PgExecutor<'_>, tenant_id: TenantId) -> Result<Vec<Network>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        network_columns!(),
        " FROM networks WHERE tenant_id = $1 ORDER BY name"
    ))
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// The tenant's default network, created on first use. Address ranges may
/// overlap between tenants: each network is its own OVN logical switch.
pub async fn default_for(db: &mut PgConnection, tenant_id: TenantId) -> Result<Network> {
    sqlx::query(
        "INSERT INTO networks (id, tenant_id, name, cidr, gateway) VALUES ($1, $2, $3, $4::cidr, $5::inet)
         ON CONFLICT (tenant_id, name) DO NOTHING",
    )
    .bind(NetworkId::new())
    .bind(tenant_id)
    .bind(DEFAULT_NAME)
    .bind(DEFAULT_CIDR)
    .bind(DEFAULT_GATEWAY)
    .execute(&mut *db)
    .await?;
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        network_columns!(),
        " FROM networks WHERE tenant_id = $1 AND name = $2"
    ))
    .bind(tenant_id)
    .bind(DEFAULT_NAME)
    .fetch_one(&mut *db)
    .await?)
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Nic {
    pub id: NicId,
    pub vm_id: VmId,
    pub network_id: NetworkId,
    pub mac: String,
    pub ip: String,
}

/// Creates a NIC with the lowest free address in the network (skipping the
/// network, gateway and broadcast addresses). Concurrent allocations that
/// pick the same address are serialised by the unique constraint; the loser
/// retries.
pub async fn create_nic(
    db: &mut PgConnection,
    id: NicId,
    vm_id: VmId,
    network_id: NetworkId,
    mac: &str,
) -> Result<Nic> {
    for _ in 0..8 {
        let res = sqlx::query_as(
            "INSERT INTO nics (id, vm_id, network_id, mac, ip)
             SELECT $1, $2, n.id, $4, set_masklen(n.cidr::inet + s, 32)
             FROM networks n, generate_series(1, (2 ^ (32 - masklen(n.cidr)))::bigint - 2) s
             WHERE n.id = $3
               AND set_masklen(n.cidr::inet + s, 32) <> set_masklen(n.gateway, 32)
               AND NOT EXISTS (SELECT 1 FROM nics x
                               WHERE x.network_id = n.id AND x.ip = set_masklen(n.cidr::inet + s, 32))
             ORDER BY s LIMIT 1
             RETURNING id, vm_id, network_id, mac, host(ip) AS ip",
        )
        .bind(id)
        .bind(vm_id)
        .bind(network_id)
        .bind(mac)
        .fetch_optional(&mut *db)
        .await;
        match res {
            Ok(Some(nic)) => return Ok(nic),
            Ok(None) => {
                return Err(
                    hm_core::Error::Conflict("network has no free addresses".into()).into(),
                );
            }
            Err(sqlx::Error::Database(e))
                if e.is_unique_violation() && e.constraint() == Some("nics_network_id_ip_key") =>
            {
                continue;
            }
            Err(e) => return Err(crate::conflict_on_unique(e, "nic")),
        }
    }
    Err(hm_core::Error::Conflict("could not allocate an address; try again".into()).into())
}

pub async fn nics_for_vm(db: impl PgExecutor<'_>, vm_id: VmId) -> Result<Vec<Nic>> {
    Ok(sqlx::query_as(
        "SELECT id, vm_id, network_id, mac, host(ip) AS ip FROM nics WHERE vm_id = $1 ORDER BY created_at",
    )
    .bind(vm_id)
    .fetch_all(db)
    .await?)
}

/// Parses a dotted-quad as returned by `host(...)`.
pub fn parse_ip(s: &str) -> Option<Ipv4Addr> {
    s.parse().ok()
}
