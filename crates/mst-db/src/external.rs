//! External networks, and the addresses metastratum hands out on them.

use chrono::{DateTime, Utc};
use mst_core::ids::{ExternalIpId, ExternalNetworkId, NicId, TenantId, VpcId};
use sqlx::{PgConnection, PgExecutor};

use crate::{Result, conflict_on_unique};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ExternalNetwork {
    pub id: ExternalNetworkId,
    pub name: String,
    pub physical_network: String,
    pub cidr: String,
    pub gateway: String,
    pub pool_start: String,
    pub pool_end: String,
    pub created_at: DateTime<Utc>,
}

macro_rules! ext_columns {
    () => {
        "id, name, physical_network, cidr::text AS cidr, host(gateway) AS gateway, \
         host(pool_start) AS pool_start, host(pool_end) AS pool_end, created_at"
    };
}

pub struct ExternalSpec<'a> {
    pub name: &'a str,
    pub physical_network: &'a str,
    pub cidr: &'a str,
    pub gateway: &'a str,
    pub pool_start: &'a str,
    pub pool_end: &'a str,
}

fn invalid(e: sqlx::Error) -> crate::Error {
    match &e {
        sqlx::Error::Database(d) if d.constraint() == Some("networks_outside_external") => {
            mst_core::Error::Conflict("the range overlaps tenants' networks".into()).into()
        }
        sqlx::Error::Database(d) if d.is_check_violation() => mst_core::Error::InvalidArgument(
            "the gateway and address pool must lie within the network's range, the pool in order"
                .into(),
        )
        .into(),
        sqlx::Error::Database(d) if d.code().as_deref() == Some("22P02") => {
            mst_core::Error::InvalidArgument("invalid address or range".into()).into()
        }
        _ => conflict_on_unique(e, "an external network with that name"),
    }
}

pub async fn create(
    db: impl PgExecutor<'_>,
    id: ExternalNetworkId,
    s: &ExternalSpec<'_>,
) -> Result<ExternalNetwork> {
    sqlx::query_as(concat!(
        "INSERT INTO external_networks (id, name, physical_network, cidr, gateway, pool_start, pool_end)
         VALUES ($1, $2, $3, $4::cidr, $5::inet, $6::inet, $7::inet) RETURNING ",
        ext_columns!()
    ))
    .bind(id)
    .bind(s.name)
    .bind(s.physical_network)
    .bind(s.cidr)
    .bind(s.gateway)
    .bind(s.pool_start)
    .bind(s.pool_end)
    .fetch_one(db)
    .await
    .map_err(invalid)
}

/// Changes name and pool. The range and gateway stay: addresses are in use.
pub async fn update(
    db: impl PgExecutor<'_>,
    id: ExternalNetworkId,
    name: &str,
    pool_start: &str,
    pool_end: &str,
) -> Result<ExternalNetwork> {
    sqlx::query_as(concat!(
        "UPDATE external_networks SET name = $2, pool_start = $3::inet, pool_end = $4::inet WHERE id = $1 RETURNING ",
        ext_columns!()
    ))
    .bind(id)
    .bind(name)
    .bind(pool_start)
    .bind(pool_end)
    .fetch_optional(db)
    .await
    .map_err(invalid)?
    .ok_or_else(|| not_found(id))
}

fn not_found(id: ExternalNetworkId) -> crate::Error {
    mst_core::Error::NotFound {
        kind: "external network",
        id: id.to_string(),
    }
    .into()
}

pub async fn get(db: impl PgExecutor<'_>, id: ExternalNetworkId) -> Result<ExternalNetwork> {
    sqlx::query_as(concat!(
        "SELECT ",
        ext_columns!(),
        " FROM external_networks WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| not_found(id))
}

pub async fn list(db: impl PgExecutor<'_>) -> Result<Vec<ExternalNetwork>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        ext_columns!(),
        " FROM external_networks ORDER BY name"
    ))
    .fetch_all(db)
    .await?)
}

/// Addresses in use on each network.
pub async fn usage(db: impl PgExecutor<'_>) -> Result<Vec<(ExternalNetworkId, i64)>> {
    Ok(sqlx::query_as(
        "SELECT external_network_id, count(*) FROM external_ips GROUP BY external_network_id",
    )
    .fetch_all(db)
    .await?)
}

pub async fn delete(db: impl PgExecutor<'_>, id: ExternalNetworkId) -> Result<()> {
    let res = sqlx::query("DELETE FROM external_networks WHERE id = $1")
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(d) if d.is_foreign_key_violation() => mst_core::Error::Conflict(
                "VPCs or floating IPs still use this external network".into(),
            )
            .into(),
            _ => crate::Error::from(e),
        })?;
    if res.rows_affected() == 0 {
        return Err(not_found(id));
    }
    Ok(())
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ExternalIp {
    pub id: ExternalIpId,
    pub external_network_id: ExternalNetworkId,
    pub ip: String,
    pub kind: String,
    pub tenant_id: TenantId,
    pub vpc_id: Option<VpcId>,
    pub nic_id: Option<NicId>,
    pub created_at: DateTime<Utc>,
}

macro_rules! ip_columns {
    () => {
        "id, external_network_id, host(ip) AS ip, kind, tenant_id, vpc_id, nic_id, created_at"
    };
}

/// Takes the lowest free address in the network's pool.
pub async fn allocate(
    db: &mut PgConnection,
    network: ExternalNetworkId,
    tenant: TenantId,
    vpc: Option<VpcId>,
) -> Result<ExternalIp> {
    let kind = if vpc.is_some() { "gateway" } else { "floating" };
    allocate_as(db, network, tenant, vpc, kind).await
}

/// An address for a load balancer's virtual IP.
pub async fn allocate_for_load_balancer(
    db: &mut PgConnection,
    network: ExternalNetworkId,
    tenant: TenantId,
) -> Result<ExternalIp> {
    allocate_as(db, network, tenant, None, "load_balancer").await
}

async fn allocate_as(
    db: &mut PgConnection,
    network: ExternalNetworkId,
    tenant: TenantId,
    vpc: Option<VpcId>,
    kind: &str,
) -> Result<ExternalIp> {
    for _ in 0..8 {
        let res = sqlx::query_as(concat!(
            "INSERT INTO external_ips (id, external_network_id, ip, kind, tenant_id, vpc_id)
             SELECT $1, e.id, e.pool_start + s, $3, $4, $5
             FROM external_networks e, generate_series(0, e.pool_end - e.pool_start) s
             WHERE e.id = $2 AND e.pool_start + s <> e.gateway
               AND NOT EXISTS (SELECT 1 FROM external_ips x
                               WHERE x.external_network_id = e.id AND x.ip = e.pool_start + s)
             ORDER BY s LIMIT 1
             RETURNING ",
            ip_columns!()
        ))
        .bind(ExternalIpId::new())
        .bind(network)
        .bind(kind)
        .bind(tenant)
        .bind(vpc)
        .fetch_optional(&mut *db)
        .await;
        match res {
            Ok(Some(ip)) => return Ok(ip),
            Ok(None) => {
                return Err(mst_core::Error::Conflict(
                    "the external network has no free addresses".into(),
                )
                .into());
            }
            Err(sqlx::Error::Database(e))
                if e.is_unique_violation()
                    && e.constraint() == Some("external_ips_external_network_id_ip_key") =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(mst_core::Error::Conflict("could not allocate an address; try again".into()).into())
}

pub async fn get_ip(db: impl PgExecutor<'_>, id: ExternalIpId) -> Result<ExternalIp> {
    sqlx::query_as(concat!(
        "SELECT ",
        ip_columns!(),
        " FROM external_ips WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        mst_core::Error::NotFound {
            kind: "floating IP",
            id: id.to_string(),
        }
        .into()
    })
}

/// Floating IPs of a tenant (and optionally its subtree).
pub async fn floating_ips(
    db: impl PgExecutor<'_>,
    tenant: TenantId,
    descendants: bool,
) -> Result<Vec<ExternalIp>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        ip_columns!(),
        " FROM external_ips WHERE kind = 'floating' AND (tenant_id = $1 OR ($2 AND tenant_id IN (
             SELECT d.id FROM tenants d, tenants t WHERE t.id = $1 AND d.path <@ t.path)))
         ORDER BY created_at"
    ))
    .bind(tenant)
    .bind(descendants)
    .fetch_all(db)
    .await?)
}

/// The floating IP pointing at a NIC, if any.
pub async fn floating_ip_of(db: impl PgExecutor<'_>, nic: NicId) -> Result<Option<ExternalIp>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        ip_columns!(),
        " FROM external_ips WHERE nic_id = $1"
    ))
    .bind(nic)
    .fetch_optional(db)
    .await?)
}

pub async fn gateway_of(db: impl PgExecutor<'_>, vpc: VpcId) -> Result<Option<ExternalIp>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        ip_columns!(),
        " FROM external_ips WHERE vpc_id = $1"
    ))
    .bind(vpc)
    .fetch_optional(db)
    .await?)
}

pub async fn associate(
    db: impl PgExecutor<'_>,
    id: ExternalIpId,
    nic: Option<NicId>,
) -> Result<()> {
    sqlx::query("UPDATE external_ips SET nic_id = $2 WHERE id = $1 AND kind = 'floating'")
        .bind(id)
        .bind(nic)
        .execute(db)
        .await
        .map_err(|e| conflict_on_unique(e, "a floating IP for that NIC"))?;
    Ok(())
}

pub async fn release(db: impl PgExecutor<'_>, id: ExternalIpId) -> Result<()> {
    sqlx::query("DELETE FROM external_ips WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}
