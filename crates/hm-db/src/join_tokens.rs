use chrono::{DateTime, Utc};
use hm_core::ids::{HostId, UserId};
use sqlx::PgExecutor;

use crate::Result;

pub async fn create(
    db: impl PgExecutor<'_>,
    id: &str,
    secret_hash: &str,
    created_by: Option<UserId>,
    expires_at: DateTime<Utc>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO join_tokens (id, secret_hash, created_by, expires_at) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(secret_hash)
    .bind(created_by)
    .bind(expires_at)
    .execute(db)
    .await?;
    Ok(())
}

/// Marks a token used if it exists, matches, is unexpired and unused.
/// Returns false otherwise; callers must not reveal which check failed.
pub async fn consume(db: impl PgExecutor<'_>, id: &str, secret_hash: &str) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE join_tokens SET used_at = now()
         WHERE id = $1 AND secret_hash = $2 AND used_at IS NULL AND expires_at > now()",
    )
    .bind(id)
    .bind(secret_hash)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

pub async fn attach_host(db: impl PgExecutor<'_>, id: &str, host_id: HostId) -> Result<()> {
    sqlx::query("UPDATE join_tokens SET host_id = $2 WHERE id = $1")
        .bind(id)
        .bind(host_id)
        .execute(db)
        .await?;
    Ok(())
}
