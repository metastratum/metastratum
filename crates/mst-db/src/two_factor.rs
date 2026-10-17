//! Second factors for password sign-in, backup codes, and the challenges of
//! sign-ins and enrollments in progress.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use mst_core::ids::{TenantId, UserId};
use serde_json::Value;
use sqlx::{PgConnection, PgExecutor};
use uuid::Uuid;

use crate::Result;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Factor {
    pub id: Uuid,
    pub user_id: UserId,
    /// `totp`, `webauthn` or `yubico_otp`.
    pub kind: String,
    pub name: String,
    pub data: Value,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

macro_rules! factor_columns {
    () => {
        "id, user_id, kind, name, data, created_at, last_used_at"
    };
}

pub async fn factors(db: impl PgExecutor<'_>, user: UserId) -> Result<Vec<Factor>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        factor_columns!(),
        " FROM second_factors WHERE user_id = $1 ORDER BY created_at"
    ))
    .bind(user)
    .fetch_all(db)
    .await?)
}

pub async fn add(
    db: impl PgExecutor<'_>,
    user: UserId,
    kind: &str,
    name: &str,
    data: &Value,
) -> Result<Factor> {
    Ok(sqlx::query_as(concat!(
        "INSERT INTO second_factors (id, user_id, kind, name, data)
         VALUES ($1, $2, $3, $4, $5) RETURNING ",
        factor_columns!()
    ))
    .bind(Uuid::now_v7())
    .bind(user)
    .bind(kind)
    .bind(name)
    .bind(data)
    .fetch_one(db)
    .await?)
}

/// Locks a user's factors of one kind, for a sign-in to check one and
/// record its use (TOTP time steps and WebAuthn counters only go forward).
pub async fn lock_kind(db: impl PgExecutor<'_>, user: UserId, kind: &str) -> Result<Vec<Factor>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        factor_columns!(),
        " FROM second_factors WHERE user_id = $1 AND kind = $2
         ORDER BY created_at FOR UPDATE"
    ))
    .bind(user)
    .bind(kind)
    .fetch_all(db)
    .await?)
}

/// Records a factor's use, with what it now holds.
pub async fn used(db: impl PgExecutor<'_>, id: Uuid, data: &Value) -> Result<()> {
    sqlx::query("UPDATE second_factors SET data = $2, last_used_at = now() WHERE id = $1")
        .bind(id)
        .bind(data)
        .execute(db)
        .await?;
    Ok(())
}

/// Removes one of a user's factors; false if the user has no such factor.
pub async fn remove(db: impl PgExecutor<'_>, user: UserId, id: Uuid) -> Result<bool> {
    Ok(
        sqlx::query("DELETE FROM second_factors WHERE id = $1 AND user_id = $2")
            .bind(id)
            .bind(user)
            .execute(db)
            .await?
            .rows_affected()
            > 0,
    )
}

/// Removes all of a user's factors and backup codes; returns how many
/// factors there were.
pub async fn reset(db: &mut PgConnection, user: UserId) -> Result<u64> {
    sqlx::query("DELETE FROM backup_codes WHERE user_id = $1")
        .bind(user)
        .execute(&mut *db)
        .await?;
    Ok(sqlx::query("DELETE FROM second_factors WHERE user_id = $1")
        .bind(user)
        .execute(&mut *db)
        .await?
        .rows_affected())
}

pub async fn has_factor(db: impl PgExecutor<'_>, user: UserId) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM second_factors WHERE user_id = $1)")
            .bind(user)
            .fetch_one(db)
            .await?,
    )
}

/// Which of these users have a second factor.
pub async fn with_factor(db: impl PgExecutor<'_>, users: &[UserId]) -> Result<HashSet<UserId>> {
    let ids: Vec<Uuid> = users.iter().map(|u| u.0).collect();
    let rows: Vec<UserId> =
        sqlx::query_scalar("SELECT DISTINCT user_id FROM second_factors WHERE user_id = ANY($1)")
            .bind(ids)
            .fetch_all(db)
            .await?;
    Ok(rows.into_iter().collect())
}

/// Replaces a user's backup codes.
pub async fn set_backup_codes(
    db: &mut PgConnection,
    user: UserId,
    hashes: &[String],
) -> Result<()> {
    sqlx::query("DELETE FROM backup_codes WHERE user_id = $1")
        .bind(user)
        .execute(&mut *db)
        .await?;
    sqlx::query("INSERT INTO backup_codes (user_id, code_hash) SELECT $1, unnest($2::text[])")
        .bind(user)
        .bind(hashes)
        .execute(&mut *db)
        .await?;
    Ok(())
}

/// Uses up a backup code; false if it is not one of the user's unused ones.
pub async fn use_backup_code(db: impl PgExecutor<'_>, user: UserId, hash: &str) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE backup_codes SET used_at = now()
         WHERE user_id = $1 AND code_hash = $2 AND used_at IS NULL",
    )
    .bind(user)
    .bind(hash)
    .execute(db)
    .await?
    .rows_affected()
        > 0)
}

pub async fn backup_codes_left(db: impl PgExecutor<'_>, user: UserId) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM backup_codes WHERE user_id = $1 AND used_at IS NULL",
    )
    .bind(user)
    .fetch_one(db)
    .await?)
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Challenge {
    pub user_id: UserId,
    pub data: Value,
    pub attempts: i32,
}

pub async fn create_challenge(
    db: impl PgExecutor<'_>,
    id_hash: &str,
    user: UserId,
    purpose: &str,
    data: &Value,
    ttl: Duration,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO auth_challenges (id_hash, user_id, purpose, data, expires_at)
         VALUES ($1, $2, $3, $4, now() + $5 * interval '1 second')",
    )
    .bind(id_hash)
    .bind(user)
    .bind(purpose)
    .bind(data)
    .bind(ttl.as_secs_f64())
    .execute(db)
    .await?;
    Ok(())
}

/// Spends one of a live challenge's `max` attempts. None when it is gone,
/// expired or out of attempts (it is then removed).
pub async fn attempt_challenge(
    db: impl PgExecutor<'_> + Copy,
    id_hash: &str,
    purpose: &str,
    max: i32,
) -> Result<Option<Challenge>> {
    let c: Option<Challenge> = sqlx::query_as(
        "UPDATE auth_challenges SET attempts = attempts + 1
         WHERE id_hash = $1 AND purpose = $2 AND expires_at > now() AND attempts < $3
         RETURNING user_id, data, attempts",
    )
    .bind(id_hash)
    .bind(purpose)
    .bind(max)
    .fetch_optional(db)
    .await?;
    if c.is_none() {
        delete_challenge(db, id_hash).await?;
    }
    Ok(c)
}

pub async fn delete_challenge(db: impl PgExecutor<'_>, id_hash: &str) -> Result<()> {
    sqlx::query("DELETE FROM auth_challenges WHERE id_hash = $1")
        .bind(id_hash)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn purge_expired_challenges(db: impl PgExecutor<'_>) -> Result<u64> {
    Ok(
        sqlx::query("DELETE FROM auth_challenges WHERE expires_at <= now()")
            .execute(db)
            .await?
            .rows_affected(),
    )
}

/// Whether the tenant, or one above it, requires a second factor.
pub async fn required(db: impl PgExecutor<'_>, tenant: TenantId) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM tenants
                        WHERE require_two_factor
                          AND path @> (SELECT path FROM tenants WHERE id = $1))",
    )
    .bind(tenant)
    .fetch_one(db)
    .await?)
}

/// The tenant's own setting, and the nearest tenant above it requiring a
/// second factor, if any.
pub async fn policy(
    db: impl PgExecutor<'_>,
    tenant: TenantId,
) -> Result<(bool, Option<(TenantId, String)>)> {
    let (own, above, above_name): (bool, Option<TenantId>, Option<String>) = sqlx::query_as(
        "SELECT t.require_two_factor, a.id, a.name
         FROM tenants t
         LEFT JOIN LATERAL (
             SELECT id, name FROM tenants
             WHERE require_two_factor AND path @> t.path AND path <> t.path
             ORDER BY nlevel(path) DESC LIMIT 1
         ) a ON true
         WHERE t.id = $1",
    )
    .bind(tenant)
    .fetch_one(db)
    .await?;
    Ok((own, above.zip(above_name)))
}

pub async fn set_required(db: impl PgExecutor<'_>, tenant: TenantId, required: bool) -> Result<()> {
    sqlx::query("UPDATE tenants SET require_two_factor = $2, updated_at = now() WHERE id = $1")
        .bind(tenant)
        .bind(required)
        .execute(db)
        .await?;
    Ok(())
}
