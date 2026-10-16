//! Counts of the platform's state, for metrics.

use sqlx::PgExecutor;

use crate::Result;

/// A count of rows sharing some labels.
#[derive(Debug, sqlx::FromRow)]
pub struct Count {
    pub a: String,
    pub b: String,
    pub n: i64,
}

/// VMs by status and reported power state.
pub async fn vms(db: impl PgExecutor<'_>) -> Result<Vec<Count>> {
    Ok(sqlx::query_as(
        "SELECT status AS a, power AS b, count(*) AS n FROM vms GROUP BY status, power",
    )
    .fetch_all(db)
    .await?)
}

/// Unfinished tasks by kind and state (`pending`, `running`).
pub async fn open_tasks(db: impl PgExecutor<'_>) -> Result<Vec<Count>> {
    Ok(sqlx::query_as(
        "SELECT kind AS a, state AS b, count(*) AS n FROM tasks
         WHERE state IN ('pending', 'running') GROUP BY kind, state",
    )
    .fetch_all(db)
    .await?)
}

/// Migrations in progress.
pub async fn migrations(db: impl PgExecutor<'_>) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT count(*) FROM migrations")
        .fetch_one(db)
        .await?)
}

/// Controller replicas heard from recently.
pub async fn live_replicas(db: impl PgExecutor<'_>) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM replicas WHERE last_seen_at > now() - interval '30 seconds'",
    )
    .fetch_one(db)
    .await?)
}
