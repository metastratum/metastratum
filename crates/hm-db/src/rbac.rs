//! Role bindings and API tokens.

use chrono::{DateTime, Utc};
use hm_core::ids::{TenantId, UserId};
use hm_core::tenant::TenantPath;
use sqlx::PgExecutor;
use uuid::Uuid;

use crate::{Result, conflict_on_unique};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct RoleBinding {
    pub id: Uuid,
    pub user_id: UserId,
    pub username: String,
    pub tenant_id: TenantId,
    pub tenant_name: String,
    pub role: String,
    /// `manual`, or `oidc` when derived from identity-provider groups.
    pub source: String,
    pub created_at: DateTime<Utc>,
}

macro_rules! binding_query {
    ($where:literal) => {
        concat!(
            "SELECT b.id, b.user_id, u.username, b.tenant_id, t.name AS tenant_name, b.role, b.source, b.created_at
             FROM role_bindings b JOIN users u ON u.id = b.user_id JOIN tenants t ON t.id = b.tenant_id ",
            $where,
            " ORDER BY t.path, u.username, b.role"
        )
    };
}

pub async fn create_binding(
    db: impl PgExecutor<'_>,
    user_id: UserId,
    tenant_id: TenantId,
    role: &str,
    created_by: Option<UserId>,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO role_bindings (id, user_id, tenant_id, role, created_by) VALUES ($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(user_id)
        .bind(tenant_id)
        .bind(role)
        .bind(created_by)
        .execute(db)
        .await
        .map_err(|e| conflict_on_unique(e, "this role binding"))?;
    Ok(id)
}

pub async fn get_binding(db: impl PgExecutor<'_>, id: Uuid) -> Result<RoleBinding> {
    sqlx::query_as(binding_query!("WHERE b.id = $1"))
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| {
            hm_core::Error::NotFound {
                kind: "role binding",
                id: id.to_string(),
            }
            .into()
        })
}

pub async fn delete_binding(db: impl PgExecutor<'_>, id: Uuid) -> Result<()> {
    sqlx::query("DELETE FROM role_bindings WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Bindings held at a tenant.
pub async fn bindings_at(db: impl PgExecutor<'_>, tenant_id: TenantId) -> Result<Vec<RoleBinding>> {
    Ok(sqlx::query_as(binding_query!("WHERE b.tenant_id = $1"))
        .bind(tenant_id)
        .fetch_all(db)
        .await?)
}

/// Bindings a user holds anywhere.
pub async fn bindings_of(db: impl PgExecutor<'_>, user_id: UserId) -> Result<Vec<RoleBinding>> {
    Ok(sqlx::query_as(binding_query!("WHERE b.user_id = $1"))
        .bind(user_id)
        .fetch_all(db)
        .await?)
}

/// A user's roles with the paths of the tenants they are bound at, for
/// permission checks.
pub async fn grants_of(
    db: impl PgExecutor<'_>,
    user_id: UserId,
) -> Result<Vec<(TenantPath, String)>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT t.path::text, b.role FROM role_bindings b JOIN tenants t ON t.id = b.tenant_id WHERE b.user_id = $1",
    )
    .bind(user_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(p, r)| Some((TenantPath::parse(&p)?, r)))
        .collect())
}

/// The paths of tenants that still exist, by ID.
pub async fn tenant_paths(
    db: impl PgExecutor<'_>,
    ids: &[TenantId],
) -> Result<Vec<(TenantId, TenantPath)>> {
    let rows: Vec<(TenantId, String)> =
        sqlx::query_as("SELECT id, path::text FROM tenants WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(db)
            .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, p)| Some((id, TenantPath::parse(&p)?)))
        .collect())
}

/// Number of enabled users holding `admin` at the root tenant. Used to refuse
/// changes that would leave nobody able to administer the provider.
pub async fn root_admin_count(db: impl PgExecutor<'_>) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(DISTINCT b.user_id) FROM role_bindings b
         JOIN tenants t ON t.id = b.tenant_id JOIN users u ON u.id = b.user_id
         WHERE t.parent_id IS NULL AND b.role = 'admin' AND NOT u.disabled",
    )
    .fetch_one(db)
    .await?)
}

/// A role an API token is limited to, at a tenant (and its sub-tenants).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenScope {
    pub tenant_id: TenantId,
    pub role: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ApiToken {
    pub id: Uuid,
    pub user_id: UserId,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    /// `None`: all its user's rights.
    pub scopes: Option<sqlx::types::Json<Vec<TokenScope>>>,
}

/// A token in use: its user, and what it is limited to.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct LiveToken {
    pub user_id: UserId,
    pub scopes: Option<sqlx::types::Json<Vec<TokenScope>>>,
}

pub async fn create_token(
    db: impl PgExecutor<'_>,
    user_id: UserId,
    name: &str,
    token_hash: &str,
    expires_at: Option<DateTime<Utc>>,
    scopes: Option<&[TokenScope]>,
) -> Result<ApiToken> {
    Ok(sqlx::query_as(
        "INSERT INTO api_tokens (id, user_id, name, token_hash, expires_at, scopes)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id, user_id, name, created_at, expires_at, last_used_at, scopes",
    )
    .bind(Uuid::now_v7())
    .bind(user_id)
    .bind(name)
    .bind(token_hash)
    .bind(expires_at)
    .bind(scopes.map(sqlx::types::Json))
    .fetch_one(db)
    .await?)
}

pub async fn tokens_of(db: impl PgExecutor<'_>, user_id: UserId) -> Result<Vec<ApiToken>> {
    Ok(sqlx::query_as(
        "SELECT id, user_id, name, created_at, expires_at, last_used_at, scopes FROM api_tokens
         WHERE user_id = $1 ORDER BY created_at DESC",
    )
    .bind(user_id)
    .fetch_all(db)
    .await?)
}

/// Deletes a token if it belongs to `user_id`. Returns whether it did.
pub async fn delete_token(db: impl PgExecutor<'_>, id: Uuid, user_id: UserId) -> Result<bool> {
    let res = sqlx::query("DELETE FROM api_tokens WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(user_id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Resolves a live token of an enabled user, and records its use.
pub async fn resolve_token(db: impl PgExecutor<'_>, token_hash: &str) -> Result<Option<LiveToken>> {
    Ok(sqlx::query_as(
        "UPDATE api_tokens t SET last_used_at = now() FROM users u
         WHERE t.token_hash = $1 AND (t.expires_at IS NULL OR t.expires_at > now())
           AND u.id = t.user_id AND NOT u.disabled
         RETURNING t.user_id, t.scopes",
    )
    .bind(token_hash)
    .fetch_optional(db)
    .await?)
}
