//! Controller replicas.

use mst_core::ids::HostId;
use sqlx::PgExecutor;

use crate::Result;

/// A replica not heard from for this long is gone.
const STALE_SECS: i64 = 30;

/// Registers this replica, or records that it is alive.
pub async fn heartbeat(db: impl PgExecutor<'_>, id: &str, peer_address: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO replicas (id, peer_address) VALUES ($1, $2)
         ON CONFLICT (id) DO UPDATE SET peer_address = $2, last_seen_at = now()",
    )
    .bind(id)
    .bind(peer_address)
    .execute(db)
    .await?;
    Ok(())
}

/// Whether `id` is a replica heard from recently.
pub async fn is_live(db: impl PgExecutor<'_>, id: &str) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM replicas
                        WHERE id = $1 AND last_seen_at > now() - make_interval(secs => $2))",
    )
    .bind(id)
    .bind(STALE_SECS as f64)
    .fetch_one(db)
    .await?)
}

/// Forgets replicas gone for a while (their sessions are long over).
pub async fn purge_stale(db: impl PgExecutor<'_>) -> Result<u64> {
    Ok(
        sqlx::query("DELETE FROM replicas WHERE last_seen_at < now() - interval '1 hour'")
            .execute(db)
            .await?
            .rows_affected(),
    )
}

/// The live replica holding `host`'s agent session: `(id, peer address)`.
pub async fn holding(db: impl PgExecutor<'_>, host: HostId) -> Result<Option<(String, String)>> {
    Ok(sqlx::query_as(
        "SELECT r.id, r.peer_address FROM hosts h JOIN replicas r ON r.id = h.connected_to
         WHERE h.id = $1 AND r.last_seen_at > now() - make_interval(secs => $2)",
    )
    .bind(host)
    .bind(STALE_SECS as f64)
    .fetch_optional(db)
    .await?)
}

/// Live replicas: `(id, peer address)`.
pub async fn live(db: impl PgExecutor<'_>) -> Result<Vec<(String, String)>> {
    Ok(sqlx::query_as(
        "SELECT id, peer_address FROM replicas WHERE last_seen_at > now() - make_interval(secs => $1) ORDER BY id",
    )
    .bind(STALE_SECS as f64)
    .fetch_all(db)
    .await?)
}
