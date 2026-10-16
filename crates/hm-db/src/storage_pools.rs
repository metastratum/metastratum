//! Storage pools and the clusters shared pools are attached to.

use chrono::{DateTime, Utc};
use hm_core::ids::{ClusterId, StoragePoolId};
use serde_json::Value;
use sqlx::{PgConnection, PgExecutor};

use crate::{Result, conflict_on_unique};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Pool {
    pub id: StoragePoolId,
    pub name: String,
    /// `local`, `nfs` or `rbd`.
    pub kind: String,
    pub config: Value,
    /// Clusters it is attached to (empty for `local`, which every host has).
    pub clusters: Vec<ClusterId>,
    pub created_at: DateTime<Utc>,
}

macro_rules! pool_query {
    ($where:literal) => {
        concat!(
            "SELECT p.id, p.name, p.kind, p.config, p.created_at,
                    coalesce(array_agg(c.cluster_id) FILTER (WHERE c.cluster_id IS NOT NULL), '{}') AS clusters
             FROM storage_pools p LEFT JOIN storage_pool_clusters c ON c.pool_id = p.id ",
            $where,
            " GROUP BY p.id ORDER BY p.name"
        )
    };
}

pub async fn list(db: impl PgExecutor<'_>) -> Result<Vec<Pool>> {
    Ok(sqlx::query_as(pool_query!("")).fetch_all(db).await?)
}

pub async fn get(db: impl PgExecutor<'_>, id: StoragePoolId) -> Result<Pool> {
    sqlx::query_as(pool_query!("WHERE p.id = $1"))
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| {
            hm_core::Error::NotFound {
                kind: "storage pool",
                id: id.to_string(),
            }
            .into()
        })
}

pub async fn create(
    db: &mut PgConnection,
    name: &str,
    kind: &str,
    config: &Value,
    clusters: &[ClusterId],
) -> Result<StoragePoolId> {
    let id = StoragePoolId::new();
    sqlx::query("INSERT INTO storage_pools (id, name, kind, config) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(name)
        .bind(kind)
        .bind(config)
        .execute(&mut *db)
        .await
        .map_err(|e| conflict_on_unique(e, "a storage pool with this name"))?;
    set_clusters(db, id, clusters).await?;
    Ok(id)
}

pub async fn update(
    db: &mut PgConnection,
    id: StoragePoolId,
    name: &str,
    config: &Value,
    clusters: &[ClusterId],
) -> Result<()> {
    sqlx::query("UPDATE storage_pools SET name = $2, config = $3 WHERE id = $1")
        .bind(id)
        .bind(name)
        .bind(config)
        .execute(&mut *db)
        .await
        .map_err(|e| conflict_on_unique(e, "a storage pool with this name"))?;
    set_clusters(db, id, clusters).await
}

async fn set_clusters(
    db: &mut PgConnection,
    id: StoragePoolId,
    clusters: &[ClusterId],
) -> Result<()> {
    sqlx::query("DELETE FROM storage_pool_clusters WHERE pool_id = $1")
        .bind(id)
        .execute(&mut *db)
        .await?;
    sqlx::query(
        "INSERT INTO storage_pool_clusters (pool_id, cluster_id) SELECT $1, unnest($2::uuid[])",
    )
    .bind(id)
    .bind(clusters.iter().map(|c| c.0).collect::<Vec<_>>())
    .execute(&mut *db)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(d)
            if d.constraint() == Some("storage_pool_clusters_cluster_id_fkey") =>
        {
            hm_core::Error::InvalidArgument("unknown cluster".into()).into()
        }
        _ => crate::Error::from(e),
    })?;
    Ok(())
}

/// Number of volumes in the pool; pools in use cannot be deleted.
pub async fn volume_count(db: impl PgExecutor<'_>, id: StoragePoolId) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT count(*) FROM volumes WHERE pool_id = $1")
            .bind(id)
            .fetch_one(db)
            .await?,
    )
}

/// Deletes a pool and grants of it.
pub async fn delete(db: &mut PgConnection, id: StoragePoolId) -> Result<()> {
    sqlx::query("DELETE FROM tenant_grants WHERE kind = 'storage_pool' AND ref_id = $1")
        .bind(id)
        .execute(&mut *db)
        .await?;
    sqlx::query("DELETE FROM storage_pools WHERE id = $1")
        .bind(id)
        .execute(&mut *db)
        .await?;
    Ok(())
}

/// Replaces a pool's configuration only (e.g. to seal its secrets).
pub async fn set_config(db: impl PgExecutor<'_>, id: StoragePoolId, config: &serde_json::Value) -> Result<()> {
    sqlx::query("UPDATE storage_pools SET config = $2 WHERE id = $1")
        .bind(id)
        .bind(sqlx::types::Json(config))
        .execute(db)
        .await?;
    Ok(())
}
