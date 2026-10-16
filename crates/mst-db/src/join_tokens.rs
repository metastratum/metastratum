use chrono::{DateTime, Utc};
use mst_core::ids::{ClusterId, HostId, UserId};
use sqlx::PgExecutor;

use crate::Result;

pub async fn create(
    db: impl PgExecutor<'_>,
    id: &str,
    secret_hash: &str,
    created_by: Option<UserId>,
    expires_at: DateTime<Utc>,
    cluster_id: Option<ClusterId>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO join_tokens (id, secret_hash, created_by, expires_at, cluster_id) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(secret_hash)
    .bind(created_by)
    .bind(expires_at)
    .bind(cluster_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Marks a token used if it exists, matches, is unexpired and unused, and
/// returns the cluster it enrolls hosts into. `None` means the token was not
/// valid; callers must not reveal which check failed.
pub async fn consume(
    db: impl PgExecutor<'_>,
    id: &str,
    secret_hash: &str,
) -> Result<Option<Option<ClusterId>>> {
    Ok(sqlx::query_scalar(
        "UPDATE join_tokens SET used_at = now()
         WHERE id = $1 AND secret_hash = $2 AND used_at IS NULL AND expires_at > now()
         RETURNING cluster_id",
    )
    .bind(id)
    .bind(secret_hash)
    .fetch_optional(db)
    .await?)
}

pub async fn attach_host(db: impl PgExecutor<'_>, id: &str, host_id: HostId) -> Result<()> {
    sqlx::query("UPDATE join_tokens SET host_id = $2 WHERE id = $1")
        .bind(id)
        .bind(host_id)
        .execute(db)
        .await?;
    Ok(())
}
