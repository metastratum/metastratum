//! Security groups and guardrail policies.

use chrono::{DateTime, Utc};
use mst_core::ids::{PolicyId, SecurityGroupId, SecurityRuleId, TenantId, UserId};
use sqlx::{PgConnection, PgExecutor};

use crate::{Result, conflict_on_unique};

pub const DEFAULT_GROUP: &str = "default";

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct SecurityGroup {
    pub id: SecurityGroupId,
    pub tenant_id: TenantId,
    pub name: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
}

/// The traffic a rule or policy is about.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Rule {
    pub id: SecurityRuleId,
    pub security_group_id: SecurityGroupId,
    pub direction: String,
    pub protocol: String,
    pub port_min: Option<i32>,
    pub port_max: Option<i32>,
    pub remote_cidr: Option<String>,
    pub remote_group_id: Option<SecurityGroupId>,
    pub description: String,
    pub created_at: DateTime<Utc>,
}

pub struct RuleSpec<'a> {
    pub direction: &'a str,
    pub protocol: &'a str,
    pub ports: Option<(i32, i32)>,
    pub remote_cidr: Option<&'a str>,
    pub remote_group_id: Option<SecurityGroupId>,
    pub description: &'a str,
}

macro_rules! group_columns {
    () => {
        "id, tenant_id, name, description, created_at"
    };
}
macro_rules! rule_columns {
    () => {
        "id, security_group_id, direction, protocol, port_min, port_max, \
    remote_cidr::text AS remote_cidr, remote_group_id, description, created_at"
    };
}

fn rule_error(e: sqlx::Error) -> crate::Error {
    match &e {
        sqlx::Error::Database(d) if d.is_check_violation() => mst_core::Error::InvalidArgument(
            "a rule needs either an address range or a group, and ports only with tcp or udp, lowest first".into(),
        )
        .into(),
        sqlx::Error::Database(d) if d.code().as_deref() == Some("22P02") => {
            mst_core::Error::InvalidArgument("invalid address range".into()).into()
        }
        _ => e.into(),
    }
}

pub async fn create_group(
    db: impl PgExecutor<'_>,
    id: SecurityGroupId,
    tenant: TenantId,
    name: &str,
    description: &str,
) -> Result<SecurityGroup> {
    sqlx::query_as(concat!("INSERT INTO security_groups (id, tenant_id, name, description) VALUES ($1, $2, $3, $4) RETURNING ", group_columns!()))
    .bind(id)
    .bind(tenant)
    .bind(name)
    .bind(description)
    .fetch_one(db)
    .await
    .map_err(|e| conflict_on_unique(e, "a security group with that name"))
}

/// The tenant's default group (everything out, in from members), created on
/// first use.
pub async fn default_group(db: &mut PgConnection, tenant: TenantId) -> Result<SecurityGroup> {
    let id = SecurityGroupId::new();
    let created: Option<SecurityGroupId> = sqlx::query_scalar(
        "INSERT INTO security_groups (id, tenant_id, name, description) VALUES ($1, $2, $3, $4)
         ON CONFLICT (tenant_id, name) DO NOTHING RETURNING id",
    )
    .bind(id)
    .bind(tenant)
    .bind(DEFAULT_GROUP)
    .bind("Allows all outgoing traffic, and incoming traffic from other members")
    .fetch_optional(&mut *db)
    .await?;
    if created.is_some() {
        for spec in [
            RuleSpec {
                direction: "egress",
                protocol: "any",
                ports: None,
                remote_cidr: Some("0.0.0.0/0"),
                remote_group_id: None,
                description: "",
            },
            RuleSpec {
                direction: "ingress",
                protocol: "any",
                ports: None,
                remote_cidr: None,
                remote_group_id: Some(id),
                description: "",
            },
        ] {
            add_rule(&mut *db, SecurityRuleId::new(), id, &spec).await?;
        }
    }
    sqlx::query_as(concat!(
        "SELECT ",
        group_columns!(),
        " FROM security_groups WHERE tenant_id = $1 AND name = $2"
    ))
    .bind(tenant)
    .bind(DEFAULT_GROUP)
    .fetch_one(&mut *db)
    .await
    .map_err(Into::into)
}

pub async fn get_group(db: impl PgExecutor<'_>, id: SecurityGroupId) -> Result<SecurityGroup> {
    sqlx::query_as(concat!(
        "SELECT ",
        group_columns!(),
        " FROM security_groups WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        mst_core::Error::NotFound {
            kind: "security group",
            id: id.to_string(),
        }
        .into()
    })
}

pub async fn list_groups(
    db: impl PgExecutor<'_>,
    tenant: TenantId,
    descendants: bool,
) -> Result<Vec<SecurityGroup>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        group_columns!(),
        " FROM security_groups WHERE tenant_id = $1 OR ($2 AND tenant_id IN (
             SELECT d.id FROM tenants d, tenants t WHERE t.id = $1 AND d.path <@ t.path))
         ORDER BY name"
    ))
    .bind(tenant)
    .bind(descendants)
    .fetch_all(db)
    .await?)
}

pub async fn update_group(
    db: impl PgExecutor<'_>,
    id: SecurityGroupId,
    name: &str,
    description: &str,
) -> Result<()> {
    sqlx::query("UPDATE security_groups SET name = $2, description = $3 WHERE id = $1")
        .bind(id)
        .bind(name)
        .bind(description)
        .execute(db)
        .await
        .map_err(|e| conflict_on_unique(e, "a security group with that name"))?;
    Ok(())
}

pub async fn delete_group(db: impl PgExecutor<'_>, id: SecurityGroupId) -> Result<()> {
    sqlx::query("DELETE FROM security_groups WHERE id = $1")
        .bind(id)
        .execute(db)
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(d) if d.is_foreign_key_violation() => {
                mst_core::Error::Conflict("VMs still use this security group".into()).into()
            }
            _ => crate::Error::from(e),
        })?;
    Ok(())
}

/// How many NICs use each group.
pub async fn member_counts(db: impl PgExecutor<'_>) -> Result<Vec<(SecurityGroupId, i64)>> {
    Ok(
        sqlx::query_as("SELECT security_group_id, count(*) FROM nic_security_groups GROUP BY 1")
            .fetch_all(db)
            .await?,
    )
}

pub async fn add_rule(
    db: impl PgExecutor<'_>,
    id: SecurityRuleId,
    group: SecurityGroupId,
    r: &RuleSpec<'_>,
) -> Result<Rule> {
    sqlx::query_as(concat!("INSERT INTO security_group_rules
             (id, security_group_id, direction, protocol, port_min, port_max, remote_cidr, remote_group_id, description)
         VALUES ($1, $2, $3, $4, $5, $6, network($7::inet), $8, $9) RETURNING ", rule_columns!()))
    .bind(id)
    .bind(group)
    .bind(r.direction)
    .bind(r.protocol)
    .bind(r.ports.map(|p| p.0))
    .bind(r.ports.map(|p| p.1))
    .bind(r.remote_cidr)
    .bind(r.remote_group_id)
    .bind(r.description)
    .fetch_one(db)
    .await
    .map_err(rule_error)
}

pub async fn get_rule(db: impl PgExecutor<'_>, id: SecurityRuleId) -> Result<Rule> {
    sqlx::query_as(concat!(
        "SELECT ",
        rule_columns!(),
        " FROM security_group_rules WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        mst_core::Error::NotFound {
            kind: "rule",
            id: id.to_string(),
        }
        .into()
    })
}

pub async fn delete_rule(db: impl PgExecutor<'_>, id: SecurityRuleId) -> Result<()> {
    sqlx::query("DELETE FROM security_group_rules WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn rules_of(db: impl PgExecutor<'_>, groups: &[SecurityGroupId]) -> Result<Vec<Rule>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        rule_columns!(),
        " FROM security_group_rules WHERE security_group_id = ANY($1) ORDER BY created_at"
    ))
    .bind(groups)
    .fetch_all(db)
    .await?)
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Policy {
    pub id: PolicyId,
    /// It applies to this tenant's VMs, and its sub-tenants'.
    pub tenant_id: TenantId,
    /// Whose administrators set it, and alone may remove it: the tenant it
    /// applies to, or one above.
    pub owner_tenant_id: TenantId,
    pub direction: String,
    pub protocol: String,
    pub port_min: Option<i32>,
    pub port_max: Option<i32>,
    pub remote_cidr: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
}

macro_rules! policy_columns {
    () => {
        "id, tenant_id, owner_tenant_id, direction, protocol, port_min, port_max, \
    remote_cidr::text AS remote_cidr, description, created_at"
    };
}

pub async fn create_policy(
    db: impl PgExecutor<'_>,
    id: PolicyId,
    tenant: TenantId,
    owner: TenantId,
    r: &RuleSpec<'_>,
    by: UserId,
) -> Result<Policy> {
    sqlx::query_as(concat!("INSERT INTO network_policies
             (id, tenant_id, direction, protocol, port_min, port_max, remote_cidr, description, created_by,
              owner_tenant_id)
         VALUES ($1, $2, $3, $4, $5, $6, network(coalesce($7, '0.0.0.0/0')::inet), $8, $9, $10) RETURNING ", policy_columns!()))
    .bind(id)
    .bind(tenant)
    .bind(r.direction)
    .bind(r.protocol)
    .bind(r.ports.map(|p| p.0))
    .bind(r.ports.map(|p| p.1))
    .bind(r.remote_cidr)
    .bind(r.description)
    .bind(by)
    .bind(owner)
    .fetch_one(db)
    .await
    .map_err(rule_error)
}

pub async fn get_policy(db: impl PgExecutor<'_>, id: PolicyId) -> Result<Policy> {
    sqlx::query_as(concat!(
        "SELECT ",
        policy_columns!(),
        " FROM network_policies WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        mst_core::Error::NotFound {
            kind: "policy",
            id: id.to_string(),
        }
        .into()
    })
}

/// Policies that apply to a tenant: its own and its ancestors', outermost
/// first.
pub async fn policies_for(db: impl PgExecutor<'_>, tenant: TenantId) -> Result<Vec<Policy>> {
    Ok(sqlx::query_as(
        "SELECT p.id, p.tenant_id, p.owner_tenant_id, p.direction, p.protocol, p.port_min, p.port_max,
                p.remote_cidr::text AS remote_cidr, p.description, p.created_at
         FROM network_policies p JOIN tenants o ON o.id = p.tenant_id, tenants t
         WHERE t.id = $1 AND o.path @> t.path ORDER BY nlevel(o.path), p.created_at",
    )
    .bind(tenant)
    .fetch_all(db)
    .await?)
}

pub async fn delete_policy(db: impl PgExecutor<'_>, id: PolicyId) -> Result<()> {
    sqlx::query("DELETE FROM network_policies WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}
