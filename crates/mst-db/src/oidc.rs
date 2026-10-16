//! Identity providers and sign-ins in progress.

use std::time::Duration;

use mst_core::ids::{TenantId, UserId};
use serde::{Deserialize, Serialize};
use sqlx::types::Json;
use sqlx::{PgConnection, PgExecutor};

use crate::{Result, conflict_on_unique};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleMapping {
    pub group: String,
    pub role: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Provider {
    pub tenant_id: TenantId,
    pub name: String,
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub scopes: String,
    pub username_claim: String,
    pub groups_claim: String,
    pub default_role: Option<String>,
    pub role_mappings: Json<Vec<RoleMapping>>,
}

pub async fn get(db: impl PgExecutor<'_>, tenant_id: TenantId) -> Result<Option<Provider>> {
    Ok(sqlx::query_as(
        "SELECT tenant_id, name, issuer, client_id, client_secret, scopes, username_claim, groups_claim,
                default_role, role_mappings
         FROM identity_providers WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_optional(db)
    .await?)
}

pub async fn upsert(db: impl PgExecutor<'_>, p: &Provider) -> Result<()> {
    sqlx::query(
        "INSERT INTO identity_providers (tenant_id, name, issuer, client_id, client_secret, scopes, username_claim,
                                         groups_claim, default_role, role_mappings)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (tenant_id) DO UPDATE SET
            name = EXCLUDED.name, issuer = EXCLUDED.issuer, client_id = EXCLUDED.client_id,
            client_secret = EXCLUDED.client_secret, scopes = EXCLUDED.scopes,
            username_claim = EXCLUDED.username_claim, groups_claim = EXCLUDED.groups_claim,
            default_role = EXCLUDED.default_role, role_mappings = EXCLUDED.role_mappings, updated_at = now()",
    )
    .bind(p.tenant_id)
    .bind(&p.name)
    .bind(&p.issuer)
    .bind(&p.client_id)
    .bind(&p.client_secret)
    .bind(&p.scopes)
    .bind(&p.username_claim)
    .bind(&p.groups_claim)
    .bind(&p.default_role)
    .bind(&p.role_mappings)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete(db: impl PgExecutor<'_>, tenant_id: TenantId) -> Result<()> {
    sqlx::query("DELETE FROM identity_providers WHERE tenant_id = $1")
        .bind(tenant_id)
        .execute(db)
        .await?;
    Ok(())
}

pub struct PendingLogin {
    pub tenant_id: TenantId,
    pub nonce: String,
    pub pkce_verifier: String,
    pub return_to: String,
}

pub async fn start_login(
    db: impl PgExecutor<'_>,
    state_hash: &str,
    l: &PendingLogin,
    ttl: Duration,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO oidc_logins (state_hash, tenant_id, nonce, pkce_verifier, return_to, expires_at)
         VALUES ($1, $2, $3, $4, $5, now() + $6 * interval '1 second')",
    )
    .bind(state_hash)
    .bind(l.tenant_id)
    .bind(&l.nonce)
    .bind(&l.pkce_verifier)
    .bind(&l.return_to)
    .bind(ttl.as_secs_f64())
    .execute(db)
    .await?;
    Ok(())
}

/// Takes a sign-in in progress (single use); `None` if unknown or expired.
pub async fn finish_login(
    db: impl PgExecutor<'_>,
    state_hash: &str,
) -> Result<Option<PendingLogin>> {
    let row: Option<(TenantId, String, String, String)> = sqlx::query_as(
        "DELETE FROM oidc_logins WHERE state_hash = $1 AND expires_at > now()
         RETURNING tenant_id, nonce, pkce_verifier, return_to",
    )
    .bind(state_hash)
    .fetch_optional(db)
    .await?;
    Ok(row.map(
        |(tenant_id, nonce, pkce_verifier, return_to)| PendingLogin {
            tenant_id,
            nonce,
            pkce_verifier,
            return_to,
        },
    ))
}

pub async fn purge_expired(db: impl PgExecutor<'_>) -> Result<u64> {
    Ok(
        sqlx::query("DELETE FROM oidc_logins WHERE expires_at <= now()")
            .execute(db)
            .await?
            .rows_affected(),
    )
}

/// The tenant's user with this identity-provider subject, if any.
pub async fn user_by_subject(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    subject: &str,
) -> Result<Option<UserId>> {
    Ok(
        sqlx::query_scalar("SELECT id FROM users WHERE tenant_id = $1 AND external_subject = $2")
            .bind(tenant_id)
            .bind(subject)
            .fetch_optional(db)
            .await?,
    )
}

/// The user an identity provider vouched for, created on first sign-in.
/// Fails with a conflict if a local account already has the username.
pub async fn upsert_user(
    db: &mut PgConnection,
    tenant_id: TenantId,
    subject: &str,
    username: &str,
    display_name: &str,
    email: Option<&str>,
) -> Result<UserId> {
    if let Some(id) = user_by_subject(&mut *db, tenant_id, subject).await? {
        sqlx::query(
            "UPDATE users SET display_name = $2, email = $3, updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(display_name)
        .bind(email)
        .execute(&mut *db)
        .await?;
        return Ok(id);
    }
    let id = UserId::new();
    sqlx::query(
        "INSERT INTO users (id, tenant_id, username, display_name, email, external_subject)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(tenant_id)
    .bind(username)
    .bind(display_name)
    .bind(email)
    .bind(subject)
    .execute(&mut *db)
    .await
    .map_err(|e| conflict_on_unique(e, "an account with this username"))?;
    Ok(id)
}

/// Replaces the user's identity-provider-derived roles at the tenant.
pub async fn sync_roles(
    db: &mut PgConnection,
    user_id: UserId,
    tenant_id: TenantId,
    roles: &[String],
) -> Result<()> {
    sqlx::query(
        "DELETE FROM role_bindings WHERE user_id = $1 AND tenant_id = $2 AND source = 'oidc'",
    )
    .bind(user_id)
    .bind(tenant_id)
    .execute(&mut *db)
    .await?;
    for role in roles {
        sqlx::query(
            "INSERT INTO role_bindings (id, user_id, tenant_id, role, source) VALUES ($1, $2, $3, $4, 'oidc')
             ON CONFLICT (user_id, tenant_id, role) DO NOTHING",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(user_id)
        .bind(tenant_id)
        .bind(role)
        .execute(&mut *db)
        .await?;
    }
    Ok(())
}

/// Every tenant's client secret, as stored.
pub async fn client_secrets(db: impl PgExecutor<'_>) -> Result<Vec<(TenantId, String)>> {
    Ok(
        sqlx::query_as("SELECT tenant_id, client_secret FROM identity_providers")
            .fetch_all(db)
            .await?,
    )
}

pub async fn set_client_secret(
    db: impl PgExecutor<'_>,
    tenant: TenantId,
    secret: &str,
) -> Result<()> {
    sqlx::query("UPDATE identity_providers SET client_secret = $2 WHERE tenant_id = $1")
        .bind(tenant)
        .bind(secret)
        .execute(db)
        .await?;
    Ok(())
}
