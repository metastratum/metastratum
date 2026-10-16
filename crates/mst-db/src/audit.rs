//! Audit log: who did what, where.

use chrono::{DateTime, Utc};
use mst_core::ids::{TenantId, UserId};
use serde_json::Value;
use sqlx::PgExecutor;
use uuid::Uuid;

use crate::Result;

pub struct Entry<'a> {
    pub user_id: Option<UserId>,
    pub username: Option<&'a str>,
    pub tenant_id: Option<TenantId>,
    pub action: &'a str,
    pub target: Option<(&'a str, Uuid)>,
    pub success: bool,
    pub details: Value,
}

pub async fn record(db: impl PgExecutor<'_>, e: Entry<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO audit_log (user_id, username, tenant_id, tenant_path, action, target_kind, target_id, outcome, details)
         VALUES ($1, $2, $3, (SELECT path FROM tenants WHERE id = $3), $4, $5, $6, $7, $8)",
    )
    .bind(e.user_id)
    .bind(e.username)
    .bind(e.tenant_id)
    .bind(e.action)
    .bind(e.target.map(|t| t.0))
    .bind(e.target.map(|t| t.1))
    .bind(if e.success { "success" } else { "failure" })
    .bind(e.details)
    .execute(db)
    .await?;
    Ok(())
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Record {
    pub id: i64,
    pub at: DateTime<Utc>,
    pub user_id: Option<UserId>,
    pub username: Option<String>,
    pub tenant_id: Option<TenantId>,
    pub action: String,
    pub target_kind: Option<String>,
    pub target_id: Option<Uuid>,
    pub outcome: String,
    pub details: Value,
}

/// Entries for a tenant's subtree, newest first; `before` pages backwards.
pub async fn list(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    before: Option<i64>,
    limit: i64,
) -> Result<Vec<Record>> {
    Ok(sqlx::query_as(
        "SELECT id, at, user_id, username, tenant_id, action, target_kind, target_id, outcome, details
         FROM audit_log
         WHERE tenant_path <@ (SELECT path FROM tenants WHERE id = $1) AND ($2::bigint IS NULL OR id < $2)
         ORDER BY id DESC LIMIT $3",
    )
    .bind(tenant_id)
    .bind(before)
    .bind(limit)
    .fetch_all(db)
    .await?)
}
