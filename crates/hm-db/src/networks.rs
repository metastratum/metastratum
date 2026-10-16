use std::net::Ipv4Addr;

use chrono::{DateTime, Utc};
use hm_core::ids::{ExternalNetworkId, NetworkId, NicId, SecurityGroupId, TenantId, VmId, VpcId};
use sqlx::{PgConnection, PgExecutor};

use crate::{Result, conflict_on_unique};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Vpc {
    pub id: VpcId,
    pub tenant_id: TenantId,
    pub name: String,
    pub external_network_id: Option<ExternalNetworkId>,
    pub created_at: DateTime<Utc>,
}

macro_rules! vpc_columns {
    () => {
        "id, tenant_id, name, external_network_id, created_at"
    };
}

pub async fn create_vpc(
    db: impl PgExecutor<'_>,
    id: VpcId,
    tenant: TenantId,
    name: &str,
) -> Result<Vpc> {
    sqlx::query_as(concat!(
        "INSERT INTO vpcs (id, tenant_id, name) VALUES ($1, $2, $3) RETURNING ",
        vpc_columns!()
    ))
    .bind(id)
    .bind(tenant)
    .bind(name)
    .fetch_one(db)
    .await
    .map_err(|e| conflict_on_unique(e, "a VPC with that name"))
}

pub async fn get_vpc(db: impl PgExecutor<'_>, id: VpcId) -> Result<Vpc> {
    sqlx::query_as(concat!(
        "SELECT ",
        vpc_columns!(),
        " FROM vpcs WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        hm_core::Error::NotFound {
            kind: "VPC",
            id: id.to_string(),
        }
        .into()
    })
}

/// VPCs of a tenant (and optionally its subtree).
pub async fn list_vpcs(
    db: impl PgExecutor<'_>,
    tenant: TenantId,
    descendants: bool,
) -> Result<Vec<Vpc>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        vpc_columns!(),
        " FROM vpcs WHERE tenant_id = $1 OR ($2 AND tenant_id IN (
             SELECT d.id FROM tenants d, tenants t WHERE t.id = $1 AND d.path <@ t.path))
         ORDER BY name"
    ))
    .bind(tenant)
    .bind(descendants)
    .fetch_all(db)
    .await?)
}

pub async fn rename_vpc(db: impl PgExecutor<'_>, id: VpcId, name: &str) -> Result<()> {
    sqlx::query("UPDATE vpcs SET name = $2 WHERE id = $1")
        .bind(id)
        .bind(name)
        .execute(db)
        .await
        .map_err(|e| conflict_on_unique(e, "a VPC with that name"))?;
    Ok(())
}

pub async fn set_vpc_external(
    db: impl PgExecutor<'_>,
    id: VpcId,
    ext: Option<ExternalNetworkId>,
) -> Result<()> {
    sqlx::query("UPDATE vpcs SET external_network_id = $2 WHERE id = $1")
        .bind(id)
        .bind(ext)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn delete_vpc(db: impl PgExecutor<'_>, id: VpcId) -> Result<()> {
    sqlx::query("DELETE FROM vpcs WHERE id = $1")
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(d) if d.is_foreign_key_violation() => {
                hm_core::Error::Conflict("delete the VPC's networks first".into()).into()
            }
            _ => crate::Error::from(e),
        })?;
    Ok(())
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Network {
    pub id: NetworkId,
    pub tenant_id: TenantId,
    pub vpc_id: VpcId,
    pub name: String,
    pub cidr: String,
    pub gateway: String,
    pub created_at: DateTime<Utc>,
}

macro_rules! network_columns {
    () => {
        "id, tenant_id, vpc_id, name, cidr::text AS cidr, host(gateway) AS gateway, created_at"
    };
}

/// Creates a network. `cidr` is normalised (host bits cleared).
pub async fn create(
    db: impl PgExecutor<'_>,
    id: NetworkId,
    vpc: &Vpc,
    name: &str,
    cidr: &str,
    gateway: &str,
) -> Result<Network> {
    sqlx::query_as(concat!(
        "INSERT INTO networks (id, tenant_id, vpc_id, name, cidr, gateway)
         VALUES ($1, $2, $3, $4, network($5::inet), $6::inet) RETURNING ",
        network_columns!()
    ))
    .bind(id)
    .bind(vpc.tenant_id)
    .bind(vpc.id)
    .bind(name)
    .bind(cidr)
    .bind(gateway)
    .fetch_one(db)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(d) if d.constraint() == Some("networks_no_overlap") => {
            hm_core::Error::Conflict("the range overlaps another network in this VPC".into()).into()
        }
        sqlx::Error::Database(d) if d.is_check_violation() => hm_core::Error::InvalidArgument(
            "the gateway must lie within the network's range".into(),
        )
        .into(),
        sqlx::Error::Database(d) if d.code().as_deref() == Some("22P02") => {
            hm_core::Error::InvalidArgument("invalid address or range".into()).into()
        }
        _ => conflict_on_unique(e, "a network with that name"),
    })
}

pub async fn delete(db: impl PgExecutor<'_>, id: NetworkId) -> Result<()> {
    sqlx::query("DELETE FROM networks WHERE id = $1")
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(d) if d.is_foreign_key_violation() => {
                hm_core::Error::Conflict("VMs are still connected to this network".into()).into()
            }
            _ => crate::Error::from(e),
        })?;
    Ok(())
}

/// Networks of a tenant (and optionally its subtree).
pub async fn list_all(
    db: impl PgExecutor<'_>,
    tenant: TenantId,
    descendants: bool,
) -> Result<Vec<Network>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        network_columns!(),
        " FROM networks WHERE tenant_id = $1 OR ($2 AND tenant_id IN (
             SELECT d.id FROM tenants d, tenants t WHERE t.id = $1 AND d.path <@ t.path))
         ORDER BY name"
    ))
    .bind(tenant)
    .bind(descendants)
    .fetch_all(db)
    .await?)
}

/// Addresses in use per network.
pub async fn port_counts(db: impl PgExecutor<'_>) -> Result<Vec<(NetworkId, i64)>> {
    Ok(
        sqlx::query_as("SELECT network_id, count(*) FROM nics GROUP BY network_id")
            .fetch_all(db)
            .await?,
    )
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

/// The tenant's default network (in its default VPC), created on first use.
/// Address ranges may overlap between VPCs: each is its own OVN router.
pub async fn default_for(db: &mut PgConnection, tenant_id: TenantId) -> Result<Network> {
    sqlx::query(
        "INSERT INTO vpcs (id, tenant_id, name) VALUES ($1, $2, $3) ON CONFLICT (tenant_id, name) DO NOTHING",
    )
    .bind(VpcId::new())
    .bind(tenant_id)
    .bind(DEFAULT_NAME)
    .execute(&mut *db)
    .await?;
    sqlx::query(
        "INSERT INTO networks (id, tenant_id, vpc_id, name, cidr, gateway)
         SELECT $1, $2, v.id, $3, $4::cidr, $5::inet FROM vpcs v WHERE v.tenant_id = $2 AND v.name = $3
         ON CONFLICT DO NOTHING",
    )
    .bind(NetworkId::new())
    .bind(tenant_id)
    .bind(DEFAULT_NAME)
    .bind(DEFAULT_CIDR)
    .bind(DEFAULT_GATEWAY)
    .execute(&mut *db)
    .await?;
    sqlx::query_as(concat!(
        "SELECT ",
        network_columns!(),
        " FROM networks WHERE tenant_id = $1 AND name = $2"
    ))
    .bind(tenant_id)
    .bind(DEFAULT_NAME)
    .fetch_optional(&mut *db)
    .await?
    .ok_or_else(|| {
        hm_core::Error::Conflict(
            "the default VPC already has a network overlapping 10.0.0.0/24; choose a network for the VM".into(),
        )
        .into()
    })
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

/// Puts a NIC in exactly these security groups.
pub async fn set_nic_groups(
    db: &mut PgConnection,
    nic: NicId,
    groups: &[SecurityGroupId],
) -> Result<()> {
    sqlx::query("DELETE FROM nic_security_groups WHERE nic_id = $1")
        .bind(nic)
        .execute(&mut *db)
        .await?;
    sqlx::query(
        "INSERT INTO nic_security_groups (nic_id, security_group_id) SELECT $1, unnest($2::uuid[])",
    )
    .bind(nic)
    .bind(groups)
    .execute(&mut *db)
    .await?;
    Ok(())
}

pub async fn nic_groups(db: impl PgExecutor<'_>, nic: NicId) -> Result<Vec<SecurityGroupId>> {
    Ok(sqlx::query_scalar(
        "SELECT security_group_id FROM nic_security_groups WHERE nic_id = $1 ORDER BY 1",
    )
    .bind(nic)
    .fetch_all(db)
    .await?)
}

pub async fn get_nic(db: impl PgExecutor<'_>, id: NicId) -> Result<Nic> {
    sqlx::query_as("SELECT id, vm_id, network_id, mac, host(ip) AS ip FROM nics WHERE id = $1")
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| {
            hm_core::Error::NotFound {
                kind: "NIC",
                id: id.to_string(),
            }
            .into()
        })
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
