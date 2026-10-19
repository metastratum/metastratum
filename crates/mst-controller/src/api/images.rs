use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use mst_core::ids::{ImageId, LOCAL_POOL, StoragePoolId, TaskId, TenantId};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::authz::{Perm, Principal, audit, authorize, authorize_resource};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_images, create_image))
        .routes(routes!(delete_image))
        .routes(routes!(download_image))
}

#[derive(Serialize, ToSchema)]
pub struct ImageView {
    id: ImageId,
    tenant_id: TenantId,
    name: String,
    source_url: String,
    /// `qcow2` or `raw`.
    format: String,
    /// What VMs made from it run: `linux` or `windows`.
    os: String,
    /// Usable by the owning tenant's descendants too.
    shared: bool,
    created_at: DateTime<Utc>,
    /// `downloading` (VMs made from it meanwhile wait for it), `ready`, or
    /// `failed` (see `error`; it can be downloaded again).
    state: String,
    /// Why downloading it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// `downloading`, but no task is downloading it (what was ended without
    /// saying how): it can be downloaded again.
    stuck: bool,
    /// Its size, stored (qcow2).
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<i64>,
    /// The size of the disk it holds: VMs' disks made from it are at least
    /// as large.
    #[serde(skip_serializing_if = "Option::is_none")]
    virtual_bytes: Option<i64>,
    /// SHA-256 of the file downloaded from `source_url`.
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    /// SHA-256 of the disk it holds, read as a raw disk: every copy, and a
    /// download again, is checked against it.
    #[serde(skip_serializing_if = "Option::is_none")]
    digest: Option<String>,
    /// The hosts holding a copy on their local disk (VMs made from it on
    /// other hosts copy it from these).
    hosts: Vec<String>,
    /// The pool it is kept in: the local pool (a host's local disk, picked
    /// at random), or a shared pool, which the hosts attached to it make
    /// disks from directly.
    storage_pool_id: StoragePoolId,
    storage_pool_name: String,
}

impl ImageView {
    fn new(i: mst_db::images::Image, hosts: Vec<String>, pool_name: String, stuck: bool) -> Self {
        Self {
            stuck: stuck && i.state == "downloading",
            storage_pool_id: i.storage_pool_id,
            storage_pool_name: pool_name,
            id: i.id,
            tenant_id: i.tenant_id,
            name: i.name,
            source_url: i.source_url,
            format: i.format,
            os: i.os,
            shared: i.shared,
            created_at: i.created_at,
            state: i.state,
            error: i.error,
            size_bytes: i.size_bytes,
            virtual_bytes: i.virtual_bytes,
            sha256: i.sha256,
            digest: i.digest,
            hosts,
        }
    }
}

async fn views(state: &AppState, images: Vec<mst_db::images::Image>) -> ApiResult<Vec<ImageView>> {
    let ids: Vec<ImageId> = images.iter().map(|i| i.id).collect();
    let mut hosts = mst_db::images::copy_hosts(&state.db, &ids).await?;
    let under_way = mst_db::images::downloads_under_way(&state.db, &ids).await?;
    let pools: std::collections::HashMap<StoragePoolId, String> =
        mst_db::storage_pools::list(&state.db)
            .await?
            .into_iter()
            .map(|p| (p.id, p.name))
            .collect();
    Ok(images
        .into_iter()
        .map(|i| {
            let h = hosts.remove(&i.id).unwrap_or_default();
            let pool = pools.get(&i.storage_pool_id).cloned().unwrap_or_default();
            let stuck = !under_way.contains(&i.id);
            ImageView::new(i, h, pool, stuck)
        })
        .collect())
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Defaults to the caller's tenant.
    tenant_id: Option<TenantId>,
}

/// Images usable by the tenant: its own and those shared by its ancestors.
#[utoipa::path(get, path = "/images", tag = "images", params(ListQuery),
    responses((status = 200, body = Vec<ImageView>)))]
async fn list_images(
    State(state): State<AppState>,
    p: Principal,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<ImageView>>> {
    let tenant = q.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::ImageRead, tenant).await?;
    let images = mst_db::images::list_usable(&state.db, tenant).await?;
    Ok(Json(views(&state, images).await?))
}

#[derive(Deserialize, ToSchema)]
pub struct CreateImage {
    tenant_id: Option<TenantId>,
    name: String,
    /// HTTP(S) URL a host downloads the image from, as it is added.
    source_url: String,
    /// `qcow2` or `raw`.
    format: String,
    /// What VMs made from it run: `linux` (the default) or `windows`, an
    /// image with cloudbase-init (see `deploy/images/windows`): VMs get
    /// Hyper-V enlightenments, and their password as Windows takes it.
    #[serde(default)]
    os: Option<String>,
    #[serde(default)]
    shared: bool,
    /// The pool to keep it in (one the tenant may use): by default the
    /// local pool, on the local disk of a host picked at random.
    storage_pool_id: Option<StoragePoolId>,
}

#[derive(Serialize, ToSchema)]
pub struct CreatedImage {
    /// The download.
    task_id: TaskId,
    #[serde(flatten)]
    image: ImageView,
}

/// Adds an image: a host downloads it into the image store of the pool it
/// is kept in (the task): its own local disk (a host picked at random), or
/// a shared pool, which its hosts make disks from directly. Other hosts copy
/// it from there to make VMs from it.
#[utoipa::path(post, path = "/images", tag = "images", request_body = CreateImage,
    responses((status = 201, body = CreatedImage), (status = 400), (status = 409)))]
async fn create_image(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<CreateImage>,
) -> ApiResult<(StatusCode, Json<CreatedImage>)> {
    let tenant = req.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::ImageWrite, tenant).await?;
    if req.name.trim().is_empty() || req.name.len() > 128 {
        return Err(ApiError::bad_request("name must be 1-128 characters"));
    }
    if !(req.source_url.starts_with("http://") || req.source_url.starts_with("https://")) {
        return Err(ApiError::bad_request("source_url must be an http(s) URL"));
    }
    if !matches!(req.format.as_str(), "qcow2" | "raw") {
        return Err(ApiError::bad_request("format must be qcow2 or raw"));
    }
    let os = req.os.as_deref().unwrap_or("linux");
    if !matches!(os, "linux" | "windows") {
        return Err(ApiError::bad_request("os must be linux or windows"));
    }
    let pool =
        mst_db::storage_pools::get(&state.db, req.storage_pool_id.unwrap_or(LOCAL_POOL)).await?;
    if !super::storage_pools::may_use(&state, tenant, &pool).await? {
        return Err(ApiError::not_found("storage pool"));
    }
    let mut tx = state.db.begin().await?;
    may_download(&mut tx, tenant).await?;
    let image = mst_db::images::create(
        &mut *tx,
        mst_db::images::NewImage {
            tenant_id: tenant,
            name: req.name.trim(),
            source_url: &req.source_url,
            format: &req.format,
            shared: req.shared,
            storage_pool_id: pool.id,
            os,
        },
    )
    .await?;
    let task_id = enqueue_download(&mut tx, &p, &image).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(tenant),
        "image.create",
        Some(("image", image.id.0)),
        json!({"name": image.name, "url": image.source_url, "storage_pool": pool.name, "os": image.os}),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(CreatedImage {
            task_id,
            image: ImageView::new(image, vec![], pool.name, false),
        }),
    ))
}

/// How many images a tenant tree (a child of the provider's, and its
/// sub-tenants; or the provider's own) may download at once: each can take
/// up to the host's largest download before it is charged, and a download
/// slot (see the engine) for up to two hours.
const DOWNLOADS_AT_ONCE: i64 = 2;

/// Refuses a download where the tenant's image quota is used up (an image
/// is charged what it takes once downloaded, `image_gib`), or its tree has
/// two downloads under way already (of images deleted meanwhile too, until
/// they stop). Locks the tenant's lineage (see `quotas::reserve`), which
/// serializes this per tree, until the transaction ends.
async fn may_download(tx: &mut mst_db::sqlx::PgConnection, tenant: TenantId) -> ApiResult<()> {
    let some = [("image_gib".to_owned(), 1)].into_iter().collect();
    mst_db::quotas::reserve(&mut *tx, tenant, &some).await?;
    if mst_db::images::downloads_in_tree(&mut *tx, tenant).await? >= DOWNLOADS_AT_ONCE {
        return Err(ApiError::conflict(format!(
            "{DOWNLOADS_AT_ONCE} images of the tenant's are downloading (its sub-tenants' and parent's count too); add more once they are done"
        )));
    }
    Ok(())
}

async fn enqueue_download(
    tx: &mut mst_db::sqlx::PgConnection,
    p: &Principal,
    image: &mst_db::images::Image,
) -> ApiResult<TaskId> {
    Ok(mst_db::tasks::enqueue(
        &mut *tx,
        mst_db::tasks::NewTask {
            tenant_id: image.tenant_id,
            kind: "image.download",
            target: Some(("image", image.id.0)),
            params: json!({}),
            created_by: Some(p.user_id),
        },
    )
    .await?
    .id)
}

#[derive(Serialize, ToSchema)]
pub struct DownloadTask {
    task_id: TaskId,
}

/// Downloads the image again: after downloading it failed, or when no
/// host holds a copy any more (each was decommissioned). Downloaded again,
/// it must be the file first downloaded, if one was.
#[utoipa::path(post, path = "/images/{id}/download", tag = "images", params(("id" = ImageId, Path)),
    responses((status = 202, body = DownloadTask), (status = 404),
              (status = 409, description = "Being downloaded, or held by a host")))]
async fn download_image(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<ImageId>,
) -> ApiResult<(StatusCode, Json<DownloadTask>)> {
    let image = mst_db::images::get(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::ImageWrite, image.tenant_id, "image").await?;
    let mut tx = state.db.begin().await?;
    let image = mst_db::images::get_for_update(&mut tx, id).await?;
    match image.state.as_str() {
        // Unless what was downloading it ended without saying how.
        "downloading" if mst_db::images::download_under_way(&mut *tx, id).await? => {
            return Err(ApiError::conflict("the image is being downloaded"));
        }
        "ready" if !mst_db::images::copies(&mut *tx, id).await?.is_empty() => {
            return Err(ApiError::conflict("hosts hold the image already"));
        }
        _ => {}
    }
    let pool = mst_db::storage_pools::get(&mut *tx, image.storage_pool_id).await?;
    if !super::storage_pools::may_use(&state, image.tenant_id, &pool).await? {
        return Err(ApiError::conflict(format!(
            "the tenant may no longer use storage pool {}; add the image again, kept elsewhere",
            pool.name
        )));
    }
    may_download(&mut tx, image.tenant_id).await?;
    mst_db::images::downloading(&mut *tx, id).await?;
    let task_id = enqueue_download(&mut tx, &p, &image).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(image.tenant_id),
        "image.download",
        Some(("image", id.0)),
        json!({"name": image.name, "url": image.source_url, "storage_pool_id": image.storage_pool_id}),
    )
    .await;
    Ok((StatusCode::ACCEPTED, Json(DownloadTask { task_id })))
}

/// Deletes an image, and every copy of it. VMs made from it keep their
/// disks, which are copies of their own.
#[utoipa::path(delete, path = "/images/{id}", tag = "images", params(("id" = ImageId, Path)),
    responses((status = 204), (status = 404)))]
async fn delete_image(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<ImageId>,
) -> ApiResult<StatusCode> {
    let image = mst_db::images::get(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::ImageWrite, image.tenant_id, "image").await?;
    let tenant_path = mst_db::images::store_path(&state.db, image.tenant_id).await?;
    // The shared pools holding it, or (being downloaded) about to: their
    // copies go too.
    let mut shared: Vec<StoragePoolId> = mst_db::images::copies(&state.db, id)
        .await?
        .into_iter()
        .filter(|c| c.host_id.is_none())
        .map(|c| c.pool_id)
        .collect();
    if image.storage_pool_id != LOCAL_POOL && !shared.contains(&image.storage_pool_id) {
        shared.push(image.storage_pool_id);
    }
    mst_db::images::delete(&state.db, id).await?;
    tokio::spawn(crate::engine::image_deleted(
        state.clone(),
        image.clone(),
        tenant_path,
        shared,
    ));
    audit(
        &state.db,
        Some(&p),
        Some(image.tenant_id),
        "image.delete",
        Some(("image", id.0)),
        json!({"name": image.name}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}
