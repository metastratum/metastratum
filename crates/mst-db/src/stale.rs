//! Copies of disks left where the disks no longer are (a move's old copy,
//! what a failed move copied), to remove.

use mst_core::ids::{HostId, StoragePoolId, VolumeId};
use sqlx::PgExecutor;
use uuid::Uuid;

use crate::Result;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Stale {
    pub id: Uuid,
    pub volume_id: VolumeId,
    pub pool_id: StoragePoolId,
    pub host_id: Option<HostId>,
    pub attempts: i32,
}

/// Records a copy to remove; returns its record's ID.
pub async fn add(
    db: impl PgExecutor<'_>,
    volume: VolumeId,
    pool: StoragePoolId,
    host: Option<HostId>,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO stale_volumes (id, volume_id, pool_id, host_id) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(volume)
    .bind(pool)
    .bind(host)
    .execute(db)
    .await?;
    Ok(id)
}

pub async fn get(db: impl PgExecutor<'_>, id: Uuid) -> Result<Option<Stale>> {
    Ok(sqlx::query_as(
        "SELECT id, volume_id, pool_id, host_id, attempts FROM stale_volumes WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Those to try again, oldest first.
pub async fn all(db: impl PgExecutor<'_>) -> Result<Vec<Stale>> {
    Ok(sqlx::query_as(
        "SELECT id, volume_id, pool_id, host_id, attempts FROM stale_volumes ORDER BY created_at",
    )
    .fetch_all(db)
    .await?)
}

pub async fn done(db: impl PgExecutor<'_>, id: Uuid) -> Result<()> {
    sqlx::query("DELETE FROM stale_volumes WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn failed(db: impl PgExecutor<'_>, id: Uuid, error: &str) -> Result<()> {
    sqlx::query("UPDATE stale_volumes SET attempts = attempts + 1, last_error = $2 WHERE id = $1")
        .bind(id)
        .bind(error)
        .execute(db)
        .await?;
    Ok(())
}
