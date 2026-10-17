//! Load balancers: a virtual IP in a VPC whose listeners spread connections
//! over member NICs.

use chrono::{DateTime, Utc};
use mst_core::ids::{ExternalIpId, LoadBalancerId, NetworkId, NicId, TenantId, VpcId};
use sqlx::{PgConnection, PgExecutor};

use crate::{Result, conflict_on_unique};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct LoadBalancer {
    pub id: LoadBalancerId,
    pub tenant_id: TenantId,
    pub vpc_id: VpcId,
    pub name: String,
    /// Set for a virtual IP on the VPC's external network.
    pub external_ip_id: Option<ExternalIpId>,
    /// Set for a virtual IP in one of the VPC's networks.
    pub network_id: Option<NetworkId>,
    pub ip: String,
    pub health_check: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Listener {
    /// `tcp` or `udp`.
    pub protocol: String,
    pub port: i32,
    pub target_port: i32,
}

macro_rules! columns {
    () => {
        "id, tenant_id, vpc_id, name, external_ip_id, network_id, host(ip) AS ip, health_check, created_at"
    };
}

pub struct NewLoadBalancer<'a> {
    pub id: LoadBalancerId,
    pub tenant_id: TenantId,
    pub vpc_id: VpcId,
    pub name: &'a str,
    pub external_ip_id: Option<ExternalIpId>,
    pub network_id: Option<NetworkId>,
    pub ip: std::net::Ipv4Addr,
    pub health_check: bool,
}

pub async fn create(db: impl PgExecutor<'_>, lb: &NewLoadBalancer<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO load_balancers
             (id, tenant_id, vpc_id, name, external_ip_id, network_id, ip, health_check)
         VALUES ($1, $2, $3, $4, $5, $6, $7::inet, $8)",
    )
    .bind(lb.id)
    .bind(lb.tenant_id)
    .bind(lb.vpc_id)
    .bind(lb.name)
    .bind(lb.external_ip_id)
    .bind(lb.network_id)
    .bind(lb.ip.to_string())
    .bind(lb.health_check)
    .execute(db)
    .await
    .map_err(|e| conflict_on_unique(e, "a load balancer with this name"))?;
    Ok(())
}

pub async fn get(db: impl PgExecutor<'_>, id: LoadBalancerId) -> Result<LoadBalancer> {
    sqlx::query_as(concat!(
        "SELECT ",
        columns!(),
        " FROM load_balancers WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        mst_core::Error::NotFound {
            kind: "load balancer",
            id: id.to_string(),
        }
        .into()
    })
}

/// The load balancers of tenants (and their subtrees), by name.
pub async fn in_subtrees(db: impl PgExecutor<'_>, roots: &[TenantId]) -> Result<Vec<LoadBalancer>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        columns!(),
        " FROM load_balancers WHERE tenant_id IN (SELECT id FROM tenants
             WHERE path <@ ANY (SELECT path FROM tenants WHERE id = ANY($1)))
         ORDER BY name"
    ))
    .bind(roots.iter().map(|t| t.0).collect::<Vec<_>>())
    .fetch_all(db)
    .await?)
}

pub async fn listeners(db: impl PgExecutor<'_>, id: LoadBalancerId) -> Result<Vec<Listener>> {
    Ok(sqlx::query_as(
        "SELECT protocol, port, target_port FROM load_balancer_listeners
         WHERE load_balancer_id = $1 ORDER BY protocol, port",
    )
    .bind(id)
    .fetch_all(db)
    .await?)
}

pub async fn members(db: impl PgExecutor<'_>, id: LoadBalancerId) -> Result<Vec<NicId>> {
    Ok(sqlx::query_scalar(
        "SELECT nic_id FROM load_balancer_members WHERE load_balancer_id = $1 ORDER BY nic_id",
    )
    .bind(id)
    .fetch_all(db)
    .await?)
}

/// Changes a load balancer's name and health checks, and replaces its
/// listeners and members.
pub async fn update(
    db: &mut PgConnection,
    id: LoadBalancerId,
    name: &str,
    health_check: bool,
    listeners: &[Listener],
    members: &[NicId],
) -> Result<()> {
    // Touched last: its change notifies, once the rest is in place.
    sqlx::query("DELETE FROM load_balancer_listeners WHERE load_balancer_id = $1")
        .bind(id)
        .execute(&mut *db)
        .await?;
    for l in listeners {
        sqlx::query(
            "INSERT INTO load_balancer_listeners (load_balancer_id, protocol, port, target_port)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(&l.protocol)
        .bind(l.port)
        .bind(l.target_port)
        .execute(&mut *db)
        .await
        .map_err(|e| conflict_on_unique(e, "a listener on this protocol and port"))?;
    }
    sqlx::query("DELETE FROM load_balancer_members WHERE load_balancer_id = $1")
        .bind(id)
        .execute(&mut *db)
        .await?;
    sqlx::query(
        "INSERT INTO load_balancer_members (load_balancer_id, nic_id)
         SELECT $1, unnest($2::uuid[]) ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(members.iter().map(|n| n.0).collect::<Vec<_>>())
    .execute(&mut *db)
    .await?;
    sqlx::query(
        "UPDATE load_balancers SET name = $2, health_check = $3, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(name)
    .bind(health_check)
    .execute(&mut *db)
    .await
    .map_err(|e| conflict_on_unique(e, "a load balancer with this name"))?;
    Ok(())
}

/// Deletes a load balancer and gives back its virtual IP.
pub async fn delete(db: &mut PgConnection, lb: &LoadBalancer) -> Result<()> {
    sqlx::query("DELETE FROM load_balancers WHERE id = $1")
        .bind(lb.id)
        .execute(&mut *db)
        .await?;
    if let Some(ext) = lb.external_ip_id {
        crate::external::release(&mut *db, ext).await?;
    }
    if let (Some(net), Ok(ip)) = (lb.network_id, lb.ip.parse()) {
        crate::networks::release_address(&mut *db, net, ip).await?;
    }
    Ok(())
}

/// Whether a VPC still has load balancers (it cannot go before them).
pub async fn any_in_vpc(db: impl PgExecutor<'_>, vpc: VpcId) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM load_balancers WHERE vpc_id = $1)")
            .bind(vpc)
            .fetch_one(db)
            .await?,
    )
}

/// Whether load balancers have their virtual IP in a network.
pub async fn any_in_network(db: impl PgExecutor<'_>, network: NetworkId) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM load_balancers WHERE network_id = $1)")
            .bind(network)
            .fetch_one(db)
            .await?,
    )
}

/// Whether a VPC's load balancers have virtual IPs on its external network.
pub async fn any_external_in_vpc(db: impl PgExecutor<'_>, vpc: VpcId) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM load_balancers WHERE vpc_id = $1 AND external_ip_id IS NOT NULL)",
    )
    .bind(vpc)
    .fetch_one(db)
    .await?)
}
