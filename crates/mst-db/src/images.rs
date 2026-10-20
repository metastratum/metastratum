use chrono::{DateTime, Utc};
use mst_core::ids::{HostId, ImageId, LOCAL_POOL, StoragePoolId, TenantId};
use sqlx::PgExecutor;

use crate::{Result, conflict_on_unique};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Image {
    pub id: ImageId,
    pub tenant_id: TenantId,
    pub name: String,
    /// `download` (the controller downloaded it from a URL, as it was added:
    /// it keeps no URL) or `upload`.
    pub origin: String,
    pub format: String,
    pub shared: bool,
    pub created_at: DateTime<Utc>,
    /// `downloading` or `uploading` (its file coming to the host), `storing`
    /// (the host adding it to the store), `ready`, or `failed` (for good).
    pub state: String,
    /// Why adding it failed.
    pub error: Option<String>,
    /// The stored file's size.
    pub size_bytes: Option<i64>,
    /// The size of the disk it holds.
    pub virtual_bytes: Option<i64>,
    /// SHA-256 of the file as it came.
    pub sha256: Option<String>,
    /// SHA-256 of the disk it holds, read as a raw disk: what every copy
    /// must hold.
    pub digest: Option<String>,
    /// The host its file came to, which added it to the store.
    pub download_host_id: Option<HostId>,
    /// Where it is kept: a host's local disk (the local pool), or a shared
    /// pool.
    pub storage_pool_id: StoragePoolId,
    /// `linux` or `windows`: what VMs made from it run.
    pub os: String,
    /// What VMs made from it get by default: Secure Boot, a TPM.
    pub secure_boot: bool,
    pub tpm: bool,
    /// The account a Windows image's VMs are signed in with by default; none
    /// when who makes the VM names one.
    pub admin_user: Option<String>,
    /// The file's size (an upload's from the start, a download's once it is
    /// all there, or from the start if the server said), and how much of it
    /// has come.
    pub file_bytes: Option<i64>,
    pub received_bytes: i64,
}

impl Image {
    /// Uploaded rather than downloaded.
    pub fn uploaded(&self) -> bool {
        self.origin == "upload"
    }
}

macro_rules! image_columns {
    () => {
        "i.id, i.tenant_id, i.name, i.origin, i.format, i.shared, i.created_at,
         i.state, i.error, i.size_bytes, i.virtual_bytes, i.sha256, i.digest,
         i.download_host_id, i.storage_pool_id, i.os, i.secure_boot, i.tpm, i.admin_user,
         i.file_bytes, i.received_bytes"
    };
}

pub struct NewImage<'a> {
    pub tenant_id: TenantId,
    pub name: &'a str,
    /// `download` or `upload`.
    pub origin: &'a str,
    pub format: &'a str,
    pub shared: bool,
    pub storage_pool_id: StoragePoolId,
    pub os: &'a str,
    pub secure_boot: bool,
    pub tpm: bool,
    pub admin_user: Option<&'a str>,
    /// The file's size, if known (an upload's is).
    pub file_bytes: Option<i64>,
    /// The host its file comes to.
    pub host: HostId,
}

/// Adds an image, its file to come to `host`: downloaded by the controller,
/// or uploaded.
pub async fn create(db: impl PgExecutor<'_>, new: NewImage<'_>) -> Result<Image> {
    sqlx::query_as(concat!(
        "INSERT INTO images AS i (id, tenant_id, name, origin, format, shared, state,
                                  storage_pool_id, os, secure_boot, tpm, admin_user,
                                  file_bytes, download_host_id, received_at)
         VALUES ($1, $2, $3, $4, $5, $6,
                 CASE WHEN $4 = 'upload' THEN 'uploading' ELSE 'downloading' END,
                 $7, $8, $9, $10, $11, $12, $13, now()) RETURNING ",
        image_columns!()
    ))
    .bind(ImageId::new())
    .bind(new.tenant_id)
    .bind(new.name)
    .bind(new.origin)
    .bind(new.format)
    .bind(new.shared)
    .bind(new.storage_pool_id)
    .bind(new.os)
    .bind(new.secure_boot)
    .bind(new.tpm)
    .bind(new.admin_user)
    .bind(new.file_bytes)
    .bind(new.host)
    .fetch_one(db)
    .await
    .map_err(|e| conflict_on_unique(e, "image"))
}

/// Records how much of an image's file has come. Returns whether it is
/// still coming (not deleted, or failed, meanwhile).
pub async fn received_so_far(db: impl PgExecutor<'_>, id: ImageId, bytes: i64) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE images SET received_bytes = $2, received_at = now()
         WHERE id = $1 AND state IN ('downloading', 'uploading')",
    )
    .bind(id)
    .bind(bytes)
    .execute(db)
    .await?
    .rows_affected()
        == 1)
}

/// Records a part of an upload having come: `from` (what had, as the part
/// was taken) to `to`. Returns false, recording nothing, when another part
/// came meanwhile (parts sent at once), or the upload ended.
pub async fn received_part(
    db: impl PgExecutor<'_>,
    id: ImageId,
    from: i64,
    to: i64,
) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE images SET received_bytes = $3, received_at = now()
         WHERE id = $1 AND state = 'uploading' AND received_bytes = $2",
    )
    .bind(id)
    .bind(from)
    .bind(to)
    .execute(db)
    .await?
    .rows_affected()
        == 1)
}

/// The size of a download's file, as the server says it will be.
pub async fn set_file_bytes(db: impl PgExecutor<'_>, id: ImageId, bytes: i64) -> Result<()> {
    sqlx::query("UPDATE images SET file_bytes = $2 WHERE id = $1 AND state = 'downloading'")
        .bind(id)
        .bind(bytes)
        .execute(db)
        .await?;
    Ok(())
}

/// An image's file all there (`bytes` of it): to be added to the store
/// (`storing`). Returns whether it was coming.
pub async fn received_all(db: impl PgExecutor<'_>, id: ImageId, bytes: i64) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE images SET state = 'storing', file_bytes = $2, received_bytes = $2,
                received_at = now()
         WHERE id = $1 AND state IN ('downloading', 'uploading')
           AND (file_bytes IS NULL OR file_bytes = $2)",
    )
    .bind(id)
    .bind(bytes)
    .execute(db)
    .await?
    .rows_affected()
        == 1)
}

/// Images whose file stopped coming: a download no part of which came since
/// `downloads_before` (the controller downloading it went away), an upload
/// none came of since `uploads_before`.
pub async fn stalled(
    db: impl PgExecutor<'_>,
    downloads_before: DateTime<Utc>,
    uploads_before: DateTime<Utc>,
) -> Result<Vec<Image>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        image_columns!(),
        " FROM images i WHERE (i.state = 'downloading' AND i.received_at < $1)
                          OR (i.state = 'uploading' AND i.received_at < $2)"
    ))
    .bind(downloads_before)
    .bind(uploads_before)
    .fetch_all(db)
    .await?)
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

/// How many images the tenant's tree is adding (their files coming, or
/// being stored), and images deleted meanwhile still being stored (tasks):
/// that of the provider's child it is in (or the provider's own).
pub async fn adding_in_tree(db: impl PgExecutor<'_>, tenant: TenantId) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "WITH me AS (SELECT subpath(path, 0, least(nlevel(path), 2)) AS top FROM tenants
                     WHERE id = $1)
         SELECT (SELECT count(*) FROM images i JOIN tenants o ON o.id = i.tenant_id, me
                 WHERE i.state IN ('downloading', 'uploading', 'storing')
                   AND subpath(o.path, 0, least(nlevel(o.path), 2)) = me.top)
              + (SELECT count(*) FROM tasks t JOIN tenants o ON o.id = t.tenant_id, me
                 WHERE t.kind = 'image.store' AND t.state IN ('pending', 'running')
                   AND NOT EXISTS (SELECT 1 FROM images i WHERE i.id = t.target_id)
                   AND subpath(o.path, 0, least(nlevel(o.path), 2)) = me.top)",
    )
    .bind(tenant)
    .fetch_one(db)
    .await?)
}

/// Which of the images a task is storing (pending or running).
pub async fn stores_under_way(
    db: impl PgExecutor<'_>,
    ids: &[ImageId],
) -> Result<std::collections::HashSet<ImageId>> {
    let rows: Vec<(ImageId,)> = sqlx::query_as(
        "SELECT DISTINCT target_id FROM tasks WHERE target_id = ANY($1)
         AND kind = 'image.store' AND state IN ('pending', 'running')",
    )
    .bind(ids.iter().map(|i| i.0).collect::<Vec<_>>())
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|(i,)| i).collect())
}

/// Images being stored that no task is storing: what stored them ended
/// without saying how.
pub async fn stopped_stores(db: impl PgExecutor<'_>) -> Result<Vec<ImageId>> {
    Ok(sqlx::query_scalar(
        "SELECT i.id FROM images i WHERE i.state = 'storing'
         AND NOT EXISTS (SELECT 1 FROM tasks t WHERE t.target_id = i.id
                         AND t.kind = 'image.store' AND t.state IN ('pending', 'running'))",
    )
    .fetch_all(db)
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

/// Records the image stored, and its first copy: it is ready, with the
/// sizes, hash and digest of what was stored. Only an image being stored
/// becomes so: returns false, recording nothing, for one gone, failed, or
/// ready already (a second run of the same task).
pub async fn stored(
    db: &mut sqlx::PgConnection,
    id: ImageId,
    at: Copy,
    stored: &Stored,
) -> Result<bool> {
    let changed = sqlx::query(
        "UPDATE images SET state = 'ready', error = NULL, size_bytes = $2, virtual_bytes = $3,
                sha256 = nullif($4, ''), digest = $5
         WHERE id = $1 AND state = 'storing'",
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

/// Marks adding the image failed (for good), saying why: only an image
/// being added (not one another run of the task made ready). Returns
/// whether it did.
pub async fn failed(db: impl PgExecutor<'_>, id: ImageId, error: &str) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE images SET state = 'failed', error = $2
         WHERE id = $1 AND state IN ('downloading', 'uploading', 'storing')",
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

/// Images (ready) whose only copy is on the host: no other host that is
/// not decommissioned, and no shared pool, holds one. Nothing downloads an
/// image again, so they go with the host.
pub async fn only_on_host(db: impl PgExecutor<'_>, host: HostId) -> Result<Vec<Image>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        image_columns!(),
        " FROM images i WHERE i.state = 'ready'
           AND EXISTS (SELECT 1 FROM image_copies c WHERE c.image_id = i.id AND c.host_id = $1)
           AND NOT EXISTS (SELECT 1 FROM image_copies c LEFT JOIN hosts h ON h.id = c.host_id
                           WHERE c.image_id = i.id AND c.host_id IS DISTINCT FROM $1
                             AND (h.id IS NULL OR h.state <> 'decommissioned'))
         ORDER BY i.name"
    ))
    .bind(host)
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
