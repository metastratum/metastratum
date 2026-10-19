use chrono::{DateTime, Utc};
use mst_core::ids::{HostId, ImageId, LOCAL_POOL, StoragePoolId, TenantId};
use sqlx::PgExecutor;

use crate::{Result, conflict_on_unique};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Image {
    pub id: ImageId,
    pub tenant_id: TenantId,
    pub name: String,
    pub source_url: String,
    pub format: String,
    pub shared: bool,
    pub created_at: DateTime<Utc>,
    /// `downloading`, `ready` or `failed`.
    pub state: String,
    /// Why the download failed.
    pub error: Option<String>,
    /// The stored file's size.
    pub size_bytes: Option<i64>,
    /// The size of the disk it holds.
    pub virtual_bytes: Option<i64>,
    /// SHA-256 of the file downloaded from `source_url`.
    pub sha256: Option<String>,
    /// SHA-256 of the disk it holds, read as a raw disk: what every copy,
    /// and a download again, must hold.
    pub digest: Option<String>,
    /// The host downloading it (or that did).
    pub download_host_id: Option<HostId>,
    /// Where it is kept: a host's local disk (the local pool), or a shared
    /// pool.
    pub storage_pool_id: StoragePoolId,
    /// `linux` or `windows`: what VMs made from it run.
    pub os: String,
}

macro_rules! image_columns {
    () => {
        "i.id, i.tenant_id, i.name, i.source_url, i.format, i.shared, i.created_at,
         i.state, i.error, i.size_bytes, i.virtual_bytes, i.sha256, i.digest,
         i.download_host_id, i.storage_pool_id, i.os"
    };
}

pub struct NewImage<'a> {
    pub tenant_id: TenantId,
    pub name: &'a str,
    pub source_url: &'a str,
    pub format: &'a str,
    pub shared: bool,
    pub storage_pool_id: StoragePoolId,
    pub os: &'a str,
}

/// Adds an image, to be downloaded (see [`downloading`]).
pub async fn create(db: impl PgExecutor<'_>, new: NewImage<'_>) -> Result<Image> {
    sqlx::query_as(concat!(
        "INSERT INTO images AS i (id, tenant_id, name, source_url, format, shared, state,
                                  storage_pool_id, os)
         VALUES ($1, $2, $3, $4, $5, $6, 'downloading', $7, $8) RETURNING ",
        image_columns!()
    ))
    .bind(ImageId::new())
    .bind(new.tenant_id)
    .bind(new.name)
    .bind(new.source_url)
    .bind(new.format)
    .bind(new.shared)
    .bind(new.storage_pool_id)
    .bind(new.os)
    .fetch_one(db)
    .await
    .map_err(|e| conflict_on_unique(e, "image"))
}

pub async fn get(db: impl PgExecutor<'_>, id: ImageId) -> Result<Image> {
    sqlx::query_as(concat!(
        "SELECT ",
        image_columns!(),
        " FROM images i WHERE i.id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        mst_core::Error::NotFound {
            kind: "image",
            id: id.to_string(),
        }
        .into()
    })
}

/// Images a tenant may use: its own, plus those its ancestors share.
pub async fn list_usable(db: impl PgExecutor<'_>, tenant_id: TenantId) -> Result<Vec<Image>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        image_columns!(),
        " FROM images i JOIN tenants t ON t.id = i.tenant_id
         WHERE i.tenant_id = $1
            OR (i.shared AND t.path @> (SELECT path FROM tenants WHERE id = $1))
         ORDER BY i.name"
    ))
    .bind(tenant_id)
    .fetch_all(db)
    .await?)
}

/// Whether `tenant_id` may use the image (see [`list_usable`]).
pub async fn is_usable_by(
    db: impl PgExecutor<'_>,
    id: ImageId,
    tenant_id: TenantId,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM images i JOIN tenants t ON t.id = i.tenant_id
             WHERE i.id = $1 AND (i.tenant_id = $2
                OR (i.shared AND t.path @> (SELECT path FROM tenants WHERE id = $2))))",
    )
    .bind(id)
    .bind(tenant_id)
    .fetch_one(db)
    .await?)
}

/// Deletes an image. VMs made from it keep their disks: copies of their own.
pub async fn delete(db: impl PgExecutor<'_>, id: ImageId) -> Result<()> {
    sqlx::query("DELETE FROM images WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Every image's ID (for agents to tell which cached copies to keep).
pub async fn all_ids(db: impl PgExecutor<'_>) -> Result<Vec<ImageId>> {
    Ok(sqlx::query_scalar("SELECT id FROM images")
        .fetch_all(db)
        .await?)
}

/// The image locked until the transaction ends.
pub async fn get_for_update(db: &mut sqlx::PgConnection, id: ImageId) -> Result<Image> {
    sqlx::query_as(concat!(
        "SELECT ",
        image_columns!(),
        " FROM images i WHERE i.id = $1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        mst_core::Error::NotFound {
            kind: "image",
            id: id.to_string(),
        }
        .into()
    })
}

/// Marks the image as being downloaded (again): on the host it was
/// downloading on, if any, where what a download cut off left may be.
pub async fn downloading(db: impl PgExecutor<'_>, id: ImageId) -> Result<()> {
    sqlx::query("UPDATE images SET state = 'downloading', error = NULL WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Records the host downloading the image.
pub async fn set_download_host(db: impl PgExecutor<'_>, id: ImageId, host: HostId) -> Result<()> {
    sqlx::query("UPDATE images SET download_host_id = $2 WHERE id = $1")
        .bind(id)
        .bind(host)
        .execute(db)
        .await?;
    Ok(())
}

/// How many downloads (tasks pending or running, for images deleted
/// meanwhile too) the tenant's tree has under way: that of the provider's
/// child it is in (or the provider's own).
pub async fn downloads_in_tree(db: impl PgExecutor<'_>, tenant: TenantId) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "WITH me AS (SELECT subpath(path, 0, least(nlevel(path), 2)) AS top FROM tenants
                     WHERE id = $1)
         SELECT count(*) FROM tasks t JOIN tenants o ON o.id = t.tenant_id, me
         WHERE t.kind = 'image.download' AND t.state IN ('pending', 'running')
           AND subpath(o.path, 0, least(nlevel(o.path), 2)) = me.top",
    )
    .bind(tenant)
    .fetch_one(db)
    .await?)
}

/// Which of the images a task is downloading (pending or running).
pub async fn downloads_under_way(
    db: impl PgExecutor<'_>,
    ids: &[ImageId],
) -> Result<std::collections::HashSet<ImageId>> {
    let rows: Vec<(ImageId,)> = sqlx::query_as(
        "SELECT DISTINCT target_id FROM tasks WHERE target_id = ANY($1)
         AND kind = 'image.download' AND state IN ('pending', 'running')",
    )
    .bind(ids.iter().map(|i| i.0).collect::<Vec<_>>())
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|(i,)| i).collect())
}

/// Whether a task is downloading the image (pending or running).
pub async fn download_under_way(db: impl PgExecutor<'_>, id: ImageId) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM tasks WHERE target_id = $1 AND kind = 'image.download'
                        AND state IN ('pending', 'running'))",
    )
    .bind(id)
    .fetch_one(db)
    .await?)
}

/// What a store holds of an image, as the host reports it.
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct Stored {
    pub size_bytes: i64,
    pub virtual_bytes: i64,
    /// Of the file downloaded; empty when it was not downloaded there.
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub digest: String,
}

/// Where a copy of an image is: in a host's local image store, or in a
/// shared pool's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Copy {
    pub pool_id: StoragePoolId,
    pub host_id: Option<HostId>,
}

impl Copy {
    pub fn local(host: HostId) -> Self {
        Self {
            pool_id: LOCAL_POOL,
            host_id: Some(host),
        }
    }

    pub fn shared(pool: StoragePoolId) -> Self {
        Self {
            pool_id: pool,
            host_id: None,
        }
    }
}

/// Records the image downloaded, and its first copy: it is ready, with the
/// sizes, hash and digest of what was downloaded. Only an image being
/// downloaded becomes so: returns false, recording nothing, for one gone,
/// failed, or ready already (a second run of the same download).
pub async fn downloaded(
    db: &mut sqlx::PgConnection,
    id: ImageId,
    at: Copy,
    stored: &Stored,
) -> Result<bool> {
    let changed = sqlx::query(
        "UPDATE images SET state = 'ready', error = NULL, size_bytes = $2, virtual_bytes = $3,
                sha256 = nullif($4, ''), digest = $5
         WHERE id = $1 AND state = 'downloading'",
    )
    .bind(id)
    .bind(stored.size_bytes)
    .bind(stored.virtual_bytes)
    .bind(&stored.sha256)
    .bind(&stored.digest)
    .execute(&mut *db)
    .await?
    .rows_affected();
    if changed == 0 {
        return Ok(false);
    }
    add_copy(db, id, at).await?;
    Ok(true)
}

/// Records a copy of the image. Fails if the image is gone.
pub async fn add_copy(db: &mut sqlx::PgConnection, id: ImageId, at: Copy) -> Result<()> {
    let found = sqlx::query(
        "INSERT INTO image_copies (image_id, pool_id, host_id)
         SELECT id, $2, $3 FROM images WHERE id = $1
         ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(at.pool_id)
    .bind(at.host_id)
    .execute(&mut *db)
    .await?
    .rows_affected();
    let exists: bool = match found {
        1 => true,
        _ => {
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM images WHERE id = $1)")
                .bind(id)
                .fetch_one(&mut *db)
                .await?
        }
    };
    if !exists {
        return Err(mst_core::Error::NotFound {
            kind: "image",
            id: id.to_string(),
        }
        .into());
    }
    // Live updates: its holders changed.
    sqlx::query("UPDATE images SET state = state WHERE id = $1")
        .bind(id)
        .execute(&mut *db)
        .await?;
    Ok(())
}

/// Forgets a copy (found missing).
pub async fn remove_copy(db: impl PgExecutor<'_>, id: ImageId, at: Copy) -> Result<()> {
    sqlx::query(
        "DELETE FROM image_copies WHERE image_id = $1 AND pool_id = $2
         AND host_id IS NOT DISTINCT FROM $3",
    )
    .bind(id)
    .bind(at.pool_id)
    .bind(at.host_id)
    .execute(db)
    .await?;
    Ok(())
}

/// Marks the download failed, saying why: only an image being downloaded
/// (not one another run of the download made ready). Returns whether it
/// did.
pub async fn failed(db: impl PgExecutor<'_>, id: ImageId, error: &str) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE images SET state = 'failed', error = $2 WHERE id = $1 AND state = 'downloading'",
    )
    .bind(id)
    .bind(error)
    .execute(db)
    .await?
    .rows_affected()
        == 1)
}

/// The copies of an image, oldest first (decommissioned hosts hold
/// nothing).
pub async fn copies(db: impl PgExecutor<'_>, id: ImageId) -> Result<Vec<Copy>> {
    Ok(sqlx::query_as(
        "SELECT c.pool_id, c.host_id FROM image_copies c LEFT JOIN hosts h ON h.id = c.host_id
         WHERE c.image_id = $1 AND (h.id IS NULL OR h.state <> 'decommissioned')
         ORDER BY c.created_at",
    )
    .bind(id)
    .fetch_all(db)
    .await?)
}

/// How many images a pool keeps, or holds copies of.
pub async fn count_in_pool(db: impl PgExecutor<'_>, pool: StoragePoolId) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM images i WHERE i.storage_pool_id = $1
            OR EXISTS (SELECT 1 FROM image_copies c WHERE c.image_id = i.id AND c.pool_id = $1)",
    )
    .bind(pool)
    .fetch_one(db)
    .await?)
}

/// The names of the hosts holding a copy of each of the images.
pub async fn copy_hosts(
    db: impl PgExecutor<'_>,
    ids: &[ImageId],
) -> Result<std::collections::HashMap<ImageId, Vec<String>>> {
    let rows: Vec<(ImageId, String)> = sqlx::query_as(
        "SELECT c.image_id, h.name FROM image_copies c JOIN hosts h ON h.id = c.host_id
         WHERE c.image_id = ANY($1) AND h.state <> 'decommissioned' ORDER BY h.name",
    )
    .bind(ids.iter().map(|i| i.0).collect::<Vec<_>>())
    .fetch_all(db)
    .await?;
    let mut out = std::collections::HashMap::<ImageId, Vec<String>>::new();
    for (id, name) in rows {
        out.entry(id).or_default().push(name);
    }
    Ok(out)
}

/// Where a tenant's images are in hosts' stores: its lineage, tenant IDs
/// (simple hex form) separated by '/'.
pub async fn store_path(db: impl PgExecutor<'_>, tenant: TenantId) -> Result<String> {
    sqlx::query_scalar("SELECT replace(ltree2text(path), '.', '/') FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_optional(db)
        .await?
        .ok_or_else(|| {
            mst_core::Error::NotFound {
                kind: "tenant",
                id: tenant.to_string(),
            }
            .into()
        })
}

/// Holds the lock serializing getting an image onto a host, on a connection
/// of its own: released when dropped, or if this process dies.
pub struct CopyLock(sqlx::PgConnection);

impl CopyLock {
    pub async fn take(pool: &sqlx::PgPool, id: ImageId, host: HostId) -> Result<Self> {
        use sqlx::Connection;
        let mut conn = sqlx::PgConnection::connect_with(&pool.connect_options()).await?;
        sqlx::query("SELECT pg_advisory_lock(hashtextextended('image-copy:' || $1::text || ':' || $2::text, 0))")
            .bind(id)
            .bind(host)
            .execute(&mut conn)
            .await?;
        Ok(Self(conn))
    }

    pub async fn release(self) {
        use sqlx::Connection;
        // Closing the session drops its advisory locks.
        let _ = self.0.close().await;
    }
}
