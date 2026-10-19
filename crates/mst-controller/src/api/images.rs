use axum::Json;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use mst_core::ids::{HostId, ImageId, LOCAL_POOL, StoragePoolId, TaskId, TenantId};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::{OpenApiRouter, UtoipaMethodRouterExt};
use utoipa_axum::routes;

use crate::authz::{Perm, Principal, audit, authorize, authorize_resource};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_images, create_image))
        .routes(routes!(delete_image))
        .routes(routes!(download_image))
        .routes(routes!(upload_image).layer(DefaultBodyLimit::max(MAX_PART)))
}

#[derive(Serialize, ToSchema)]
pub struct ImageView {
    id: ImageId,
    tenant_id: TenantId,
    name: String,
    /// Where it is downloaded from; empty for an uploaded image.
    source_url: String,
    /// `qcow2` or `raw`.
    format: String,
    /// What VMs made from it run: `linux` or `windows`.
    os: String,
    /// An uploaded image's size, and how much of it has arrived (see
    /// `PUT /images/{id}/upload`).
    #[serde(skip_serializing_if = "Option::is_none")]
    upload_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uploaded_bytes: Option<i64>,
    /// Usable by the owning tenant's descendants too.
    shared: bool,
    /// What VMs made from it get unless told otherwise: UEFI Secure Boot,
    /// a TPM.
    secure_boot: bool,
    tpm: bool,
    /// A Windows image's: the account its VMs are signed in with unless
    /// told otherwise (`Administrator` on Windows Server); none when each
    /// VM names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    admin_user: Option<String>,
    created_at: DateTime<Utc>,
    /// `uploading` (its parts arriving), `downloading` (being added to its
    /// pool, from its URL or its upload: VMs made from it meanwhile wait
    /// for it), `ready`, or `failed` (see `error`; it can be downloaded,
    /// or uploaded, again).
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
            upload_bytes: i.upload_bytes,
            uploaded_bytes: i.upload_bytes.map(|_| i.uploaded_bytes),
            shared: i.shared,
            secure_boot: i.secure_boot,
            tpm: i.tpm,
            admin_user: i.admin_user,
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
    /// HTTP(S) URL a host downloads the image from, as it is added. Or
    /// none, with `upload_bytes`.
    #[serde(default)]
    source_url: Option<String>,
    /// The size of a file to upload instead (bytes), in parts: see
    /// `PUT /images/{id}/upload`.
    #[serde(default)]
    upload_bytes: Option<u64>,
    /// `qcow2` or `raw`.
    format: String,
    /// What VMs made from it run: `linux` (the default) or `windows`, an
    /// image with cloudbase-init (see `deploy/images/windows`): VMs get
    /// Hyper-V enlightenments, and their password as Windows takes it.
    #[serde(default)]
    os: Option<String>,
    #[serde(default)]
    shared: bool,
    /// VMs made from it boot with UEFI Secure Boot unless told otherwise
    /// (Windows 11 requires it).
    #[serde(default)]
    secure_boot: bool,
    /// VMs made from it get a TPM 2.0 unless told otherwise (Windows 11
    /// requires one).
    #[serde(default)]
    tpm: bool,
    /// For a Windows image: the account its VMs are signed in with unless
    /// a VM names another, e.g. `Administrator` on Windows Server. Without
    /// it, each VM names one (Windows 11 has no Administrator to use).
    #[serde(default)]
    admin_user: Option<String>,
    /// The pool to keep it in (one the tenant may use): by default the
    /// local pool, on the local disk of a host picked at random.
    storage_pool_id: Option<StoragePoolId>,
}

#[derive(Serialize, ToSchema)]
pub struct CreatedImage {
    /// The download; none for an upload, whose last part starts adding it.
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<TaskId>,
    #[serde(flatten)]
    image: ImageView,
}

/// Adds an image: a host downloads it into the image store of the pool it
/// is kept in (the task): its own local disk (a host picked at random), or
/// a shared pool, which its hosts make disks from directly. Other hosts copy
/// it from there to make VMs from it. With `upload_bytes` instead of a URL,
/// it is uploaded (`PUT /images/{id}/upload`) to such a host, picked now,
/// and once all of it has arrived added as a download is.
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
    let source_url = match (&req.source_url, req.upload_bytes) {
        (Some(url), None) if url.starts_with("http://") || url.starts_with("https://") => {
            url.as_str()
        }
        (Some(_), None) => return Err(ApiError::bad_request("source_url must be an http(s) URL")),
        (None, Some(n)) if n > 0 && n <= i64::MAX as u64 => "",
        (None, Some(_)) => return Err(ApiError::bad_request("upload_bytes must be more than 0")),
        _ => {
            return Err(ApiError::bad_request(
                "give either source_url (to download it) or upload_bytes (to upload it)",
            ));
        }
    };
    if !matches!(req.format.as_str(), "qcow2" | "raw") {
        return Err(ApiError::bad_request("format must be qcow2 or raw"));
    }
    let os = req.os.as_deref().unwrap_or("linux");
    if !matches!(os, "linux" | "windows") {
        return Err(ApiError::bad_request("os must be linux or windows"));
    }
    let admin_user = req.admin_user.as_deref().map(str::trim);
    if let Some(u) = admin_user {
        if os != "windows" {
            return Err(ApiError::bad_request("admin_user is for windows images"));
        }
        windows_user(u)?;
    }
    let pool =
        mst_db::storage_pools::get(&state.db, req.storage_pool_id.unwrap_or(LOCAL_POOL)).await?;
    if !super::storage_pools::may_use(&state, tenant, &pool).await? {
        return Err(ApiError::not_found("storage pool"));
    }
    let upload_host = match req.upload_bytes {
        Some(_) => Some(upload_host(&state, &pool).await?),
        None => None,
    };
    let mut tx = state.db.begin().await?;
    may_download(&mut tx, tenant).await?;
    let image = mst_db::images::create(
        &mut *tx,
        mst_db::images::NewImage {
            tenant_id: tenant,
            name: req.name.trim(),
            source_url,
            format: &req.format,
            shared: req.shared,
            storage_pool_id: pool.id,
            os,
            secure_boot: req.secure_boot,
            tpm: req.tpm,
            admin_user,
            upload_bytes: req.upload_bytes.map(|n| n as i64),
            upload_host,
        },
    )
    .await?;
    let task_id = match image.uploaded() {
        true => None,
        false => Some(enqueue_download(&mut tx, &p, &image).await?),
    };
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(tenant),
        "image.create",
        Some(("image", image.id.0)),
        json!({"name": image.name, "url": image.source_url, "upload_bytes": image.upload_bytes,
               "storage_pool": pool.name, "os": image.os,
               "secure_boot": image.secure_boot, "tpm": image.tpm, "admin_user": image.admin_user}),
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

/// A host to upload an image to: one that can take it into the pool's
/// store (connected, and reaching a shared pool), at random.
async fn upload_host(state: &AppState, pool: &mst_db::storage_pools::Pool) -> ApiResult<HostId> {
    let hosts: Vec<_> = crate::engine::store_hosts(state)
        .await
        .map_err(ApiError::internal)?
        .into_iter()
        .filter(|h| pool.kind == "local" || crate::engine::reaches(h, pool))
        .collect();
    if hosts.is_empty() {
        return Err(ApiError::unavailable(match pool.kind.as_str() {
            "local" => "no connected host to upload the image to".to_owned(),
            _ => format!("no connected host reaches {}", pool.name),
        }));
    }
    Ok(hosts[rand::random_range(0..hosts.len())].id)
}

/// The most one request may carry of an upload.
const MAX_PART: usize = 32 << 20;
/// What goes to the host in one command (within what gRPC takes).
const PIECE: usize = 2 << 20;

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UploadQuery {
    /// Where the part goes in the file: as much of it as has arrived
    /// (`uploaded_bytes`). 0 starts a failed upload over.
    offset: u64,
}

/// Uploads a part of an image added with `upload_bytes`: the next up to
/// 32 MiB of the file, as the request's body, from `offset` (what has
/// arrived: `uploaded_bytes`). Parts go to the host picked for it as they
/// come. Once all of it has arrived, it is added to its pool as a download
/// is (`downloading`, then `ready`), checked the same way. An upload no part
/// reaches for a day fails. A failed upload is started over from offset 0.
#[utoipa::path(put, path = "/images/{id}/upload", tag = "images",
    params(("id" = ImageId, Path), UploadQuery),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses((status = 200, body = ImageView), (status = 400), (status = 404),
        (status = 409, description = "Not being uploaded, or the part is not the next one: see `uploaded_bytes`"),
        (status = 413, description = "A part larger than 32 MiB"), (status = 503)))]
async fn upload_image(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<ImageId>,
    Query(q): Query<UploadQuery>,
    body: Bytes,
) -> ApiResult<Json<ImageView>> {
    let image = mst_db::images::get(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::ImageWrite, image.tenant_id, "image").await?;
    let Some(total) = image.upload_bytes.map(|n| n as u64) else {
        return Err(ApiError::conflict(
            "the image is downloaded from its URL, not uploaded",
        ));
    };
    // Held until the part is through: parts arrive one after the other.
    let mut tx = state.db.begin().await?;
    let mut image = mst_db::images::get_for_update(&mut tx, id).await?;
    if image.state == "failed" && q.offset == 0 {
        let pool = mst_db::storage_pools::get(&mut *tx, image.storage_pool_id).await?;
        if !super::storage_pools::may_use(&state, image.tenant_id, &pool).await? {
            return Err(ApiError::conflict(format!(
                "the tenant may no longer use storage pool {}; add the image again, kept elsewhere",
                pool.name
            )));
        }
        may_download(&mut tx, image.tenant_id).await?;
        let host = upload_host(&state, &pool).await?;
        mst_db::images::upload_again(&mut *tx, id, host).await?;
        image = mst_db::images::get_for_update(&mut tx, id).await?;
    }
    match image.state.as_str() {
        "uploading" => {}
        "failed" => {
            return Err(ApiError::conflict(format!(
                "uploading it failed ({}); upload it again, from offset 0",
                image.error.as_deref().unwrap_or("no reason given")
            )));
        }
        _ => return Err(ApiError::conflict("all of the image has been uploaded")),
    }
    let here = image.uploaded_bytes as u64;
    if q.offset != here {
        return Err(ApiError::conflict(format!(
            "{here} bytes of the image have arrived: send the part from offset {here}"
        )));
    }
    let end = here + body.len() as u64;
    if end > total {
        return Err(ApiError::bad_request(format!(
            "the part goes beyond the image's {total} bytes"
        )));
    }
    if body.is_empty() {
        return Err(ApiError::bad_request("the part is empty"));
    }
    let host = image.download_host_id.ok_or_else(|| {
        ApiError::conflict("the host the upload went to is gone; upload it again")
    })?;
    #[derive(Deserialize)]
    struct Received {
        received: u64,
    }
    let mut at = here;
    for piece in body.chunks(PIECE) {
        let cmd = mst_proto::command::Cmd::ReceiveImage(mst_proto::ReceiveImage {
            image_id: id.to_string(),
            offset: at,
            data: piece.to_vec(),
            total,
        });
        let got: Received = state
            .agents
            .query(host, cmd, std::time::Duration::from_secs(60))
            .await
            .map_err(|e| ApiError::unavailable(format!("the host taking the upload: {e}")))?;
        if got.received < at + piece.len() as u64 {
            // It has less than was sent (it lost some): sent again from there.
            mst_db::images::uploaded_so_far(&mut *tx, id, got.received as i64).await?;
            tx.commit().await?;
            return Err(ApiError::conflict(format!(
                "the host has {0} bytes of the image: send the part from offset {0}",
                got.received
            )));
        }
        at += piece.len() as u64;
    }
    mst_db::images::uploaded_so_far(&mut *tx, id, end as i64).await?;
    let done = end == total && mst_db::images::upload_done(&mut *tx, id).await?;
    if done {
        enqueue_download(&mut tx, &p, &image).await?;
    }
    tx.commit().await?;
    if done {
        audit(
            &state.db,
            Some(&p),
            Some(image.tenant_id),
            "image.upload",
            Some(("image", id.0)),
            json!({"name": image.name, "bytes": total}),
        )
        .await;
    }
    let image = mst_db::images::get(&state.db, id).await?;
    let view = views(&state, vec![image])
        .await?
        .pop()
        .ok_or_else(|| ApiError::not_found("image"))?;
    Ok(Json(view))
}

/// Refuses what Windows does not take as an account name.
pub(crate) fn windows_user(name: &str) -> ApiResult<()> {
    let bad = |c: char| c.is_control() || "\"/\\[]:;|=,+*?<>@%".contains(c);
    if name.is_empty() || name.chars().count() > 20 || name.chars().any(bad) || name.ends_with('.')
    {
        return Err(ApiError::bad_request(
            "a Windows account name is 1-20 characters, none of \" / \\ [ ] : ; | = , + * ? < > @ %, and does not end in a dot",
        ));
    }
    Ok(())
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
    if image.uploaded() {
        return Err(ApiError::conflict(
            "the image was uploaded: upload it again (from offset 0)",
        ));
    }
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
