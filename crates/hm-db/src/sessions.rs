use std::time::Duration;

use hm_core::ids::UserId;
use sqlx::PgExecutor;

use crate::Result;

pub async fn create(
    db: impl PgExecutor<'_>,
    id_hash: &str,
    user_id: UserId,
    ttl: Duration,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO sessions (id_hash, user_id, expires_at)
         VALUES ($1, $2, now() + $3 * interval '1 second')",
    )
    .bind(id_hash)
    .bind(user_id)
    .bind(ttl.as_secs_f64())
    .execute(db)
    .await?;
    Ok(())
}

/// Resolves a live session to its user and marks it used. Disabled users'
/// sessions do not resolve.
pub async fn resolve(db: impl PgExecutor<'_>, id_hash: &str) -> Result<Option<UserId>> {
    Ok(sqlx::query_scalar(
        "UPDATE sessions s SET last_seen_at = now()
         FROM users u
         WHERE s.id_hash = $1 AND s.expires_at > now() AND u.id = s.user_id AND NOT u.disabled
         RETURNING s.user_id",
    )
    .bind(id_hash)
    .fetch_optional(db)
    .await?)
}

pub async fn delete(db: impl PgExecutor<'_>, id_hash: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE id_hash = $1")
        .bind(id_hash)
        .execute(db)
        .await?;
    Ok(())
}

/// Ends a user's sessions, except optionally the one making the request.
pub async fn delete_for_user(
    db: impl PgExecutor<'_>,
    user_id: UserId,
    except_hash: Option<&str>,
) -> Result<u64> {
    Ok(
        sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND id_hash IS DISTINCT FROM $2")
            .bind(user_id)
            .bind(except_hash)
            .execute(db)
            .await?
            .rows_affected(),
    )
}

pub async fn purge_expired(db: impl PgExecutor<'_>) -> Result<u64> {
    Ok(
        sqlx::query("DELETE FROM sessions WHERE expires_at <= now()")
            .execute(db)
            .await?
            .rows_affected(),
    )
}
