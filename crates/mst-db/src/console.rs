use std::time::Duration;

use mst_core::ids::{UserId, VmId};
use sqlx::PgExecutor;

use crate::Result;

pub async fn create_ticket(
    db: impl PgExecutor<'_>,
    token_hash: &str,
    user_id: UserId,
    vm_id: VmId,
    kind: &str,
    ttl: Duration,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO console_tickets (token_hash, user_id, vm_id, kind, expires_at)
         VALUES ($1, $2, $3, $4, now() + $5 * interval '1 second')",
    )
    .bind(token_hash)
    .bind(user_id)
    .bind(vm_id)
    .bind(kind)
    .bind(ttl.as_secs_f64())
    .execute(db)
    .await?;
    Ok(())
}

/// Atomically consumes a live ticket: a ticket can only ever be used once.
pub async fn consume_ticket(
    db: impl PgExecutor<'_>,
    token_hash: &str,
) -> Result<Option<(UserId, VmId, String)>> {
    Ok(sqlx::query_as(
        "DELETE FROM console_tickets WHERE token_hash = $1 AND expires_at > now()
         RETURNING user_id, vm_id, kind",
    )
    .bind(token_hash)
    .fetch_optional(db)
    .await?)
}
