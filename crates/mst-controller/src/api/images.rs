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
    /// Usable by the owning tenant's descendants too.
    shared: bool,
    created_at: DateTime<Utc>,
    /// `downloading` (VMs made from it meanwhile wait for it), `ready`, or
    /// `failed` (see `error`; it can be downloaded again).
    state: String,
    /// Why downloading it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
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
    fn new(i: mst_db::images::Image, hosts: Vec<String>, pool_name: String) -> Self {
        Self {
            storage_pool_id: i.storage_pool_id,
            storage_pool_name: pool_name,
            id: i.id,
            tenant_id: i.tenant_id,
            name: i.name,
            source_url: i.source_url,
            format: i.format,
            shared: i.shared,
            created_at: i.created_at,
            state: i.state,
            error: i.error,
            size_bytes: i.size_bytes,
            virtual_bytes: i.virtual_bytes,
            sha256: i.sha256,
            hosts,
        }
    }
}

async fn views(state: &AppState, images: Vec<mst_db::images::Image>) -> ApiResult<Vec<ImageView>> {
    let ids: Vec<ImageId> = images.iter().map(|i| i.id).collect();
    let mut hosts = mst_db::images::copy_hosts(&state.db, &ids).await?;
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
            ImageView::new(i, h, pool)
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
    let pool =
        mst_db::storage_pools::get(&state.db, req.storage_pool_id.unwrap_or(LOCAL_POOL)).await?;
    if !super::storage_pools::may_use(&state, tenant, &pool).await? {
        return Err(ApiError::not_found("storage pool"));
    }
    let mut tx = state.db.begin().await?;
    // Images are charged what they take once downloaded (`image_gib`);
    // none is downloaded where the quota is used up.
    let some = [("image_gib".to_owned(), 1)].into_iter().collect();
    mst_db::quotas::reserve(&mut tx, tenant, &some).await?;
    let image = mst_db::images::create(
        &mut *tx,
        mst_db::images::NewImage {
            tenant_id: tenant,
            name: req.name.trim(),
            source_url: &req.source_url,
            format: &req.format,
            shared: req.shared,
            storage_pool_id: pool.id,
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
        json!({"name": image.name, "url": image.source_url, "storage_pool": pool.name}),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(CreatedImage {
            task_id,
            image: ImageView::new(image, vec![], pool.name),
        }),
    ))
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
        "downloading" => return Err(ApiError::conflict("the image is being downloaded")),
        "ready" if !mst_db::images::copies(&mut *tx, id).await?.is_empty() => {
            return Err(ApiError::conflict("hosts hold the image already"));
        }
        _ => {}
    }
    mst_db::images::downloading(&mut *tx, id).await?;
    let task_id = enqueue_download(&mut tx, &p, &image).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(image.tenant_id),
        "image.download",
        Some(("image", id.0)),
        json!({"name": image.name, "url": image.source_url}),
    )
    .await;
    Ok((StatusCode::ACCEPTED, Json(DownloadTask { task_id })))
}

/// Deletes an image, and every host's copy. VMs made from it keep their
/// disks, which are copies; only disks made before the image store, built
/// on the image, keep it from going.
#[utoipa::path(delete, path = "/images/{id}", tag = "images", params(("id" = ImageId, Path)),
    responses((status = 204), (status = 404), (status = 409, description = "VM disks are built on it")))]
async fn delete_image(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<ImageId>,
) -> ApiResult<StatusCode> {
    let image = mst_db::images::get(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::ImageWrite, image.tenant_id, "image").await?;
    let tenant_path = mst_db::images::store_path(&state.db, image.tenant_id).await?;
    mst_db::images::delete(&mut *state.db.acquire().await?, id).await?;
    tokio::spawn(crate::cleanup::image_deleted(
        state.clone(),
        image.clone(),
        tenant_path,
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
