use axum::Json;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use mst_core::ids::{HostId, ImageId, LOCAL_POOL, StoragePoolId, TenantId};
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
        .routes(routes!(upload_image).layer(DefaultBodyLimit::max(MAX_PART)))
}

#[derive(Serialize, ToSchema)]
pub struct ImageView {
    id: ImageId,
    tenant_id: TenantId,
    name: String,
    /// `download` (from a URL, by the controller, as it was added: the URL
    /// is not kept with it; the audit log records it) or `upload`.
    origin: String,
    /// `qcow2` or `raw`.
    format: String,
    /// What VMs made from it run: `linux` or `windows`.
    os: String,
    /// The size of its file, as it came (an upload's from the start, a
    /// download's once known), and, while it is coming, how much has.
    #[serde(skip_serializing_if = "Option::is_none")]
    file_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    received_bytes: Option<i64>,
    /// Usable by the owning tenant's descendants too.
    shared: bool,
    /// What VMs made from it get unless told otherwise: UEFI Secure Boot,
    /// a TPM, and whether they may sleep.
    secure_boot: bool,
    tpm: bool,
    sleep: bool,
    /// A Windows image's: the account its VMs are signed in with unless
    /// told otherwise (`Administrator` on Windows Server); none when each
    /// VM names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    admin_user: Option<String>,
    created_at: DateTime<Utc>,
    /// Being added: `downloading` or `uploading` (its file coming to the
    /// host picked for it), `storing` (that host checking it and adding it
    /// to its pool); then `ready`, or `failed` (see `error`: for good;
    /// delete it and add it again). VMs made from one being downloaded or
    /// stored wait for it.
    state: String,
    /// Why adding it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// `storing`, but no task is storing it (what was ended without saying
    /// how): failed, in effect (it fails within minutes).
    stuck: bool,
    /// Its size, stored (qcow2).
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<i64>,
    /// The size of the disk it holds: VMs' disks made from it are at least
    /// as large.
    #[serde(skip_serializing_if = "Option::is_none")]
    virtual_bytes: Option<i64>,
    /// SHA-256 of the file as it came.
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    /// SHA-256 of the disk it holds, read as a raw disk: every copy is
    /// checked against it.
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
            stuck: stuck && i.state == "storing",
            storage_pool_id: i.storage_pool_id,
            storage_pool_name: pool_name,
            id: i.id,
            tenant_id: i.tenant_id,
            name: i.name,
            received_bytes: matches!(i.state.as_str(), "downloading" | "uploading")
                .then_some(i.received_bytes),
            origin: i.origin,
            file_bytes: i.file_bytes,
            format: i.format,
            os: i.os,
            shared: i.shared,
            secure_boot: i.secure_boot,
            tpm: i.tpm,
            sleep: i.sleep,
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
    let under_way = mst_db::images::stores_under_way(&state.db, &ids).await?;
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
    /// HTTP(S) URL the controller downloads the image from, as it is added
    /// (and only then: the image keeps no URL). Or none, with
    /// `upload_bytes`.
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
    /// VMs made from it may sleep (suspend to RAM or to disk) unless told
    /// otherwise, as desktops do when idle; asleep, starting one wakes it.
    /// By default they may.
    #[serde(default = "yes")]
    sleep: bool,
    /// For a Windows image: the account its VMs are signed in with unless
    /// a VM names another, e.g. `Administrator` on Windows Server. Without
    /// it, each VM names one (Windows 11 has no Administrator to use).
    #[serde(default)]
    admin_user: Option<String>,
    /// The pool to keep it in (one the tenant may use): by default the
    /// local pool, on the local disk of a host picked at random.
    storage_pool_id: Option<StoragePoolId>,
}

fn yes() -> bool {
    true
}

/// Adds an image. Its file comes to a host that can keep it in its pool
/// (picked at random; for a shared pool, one attached to it): downloaded
/// from `source_url` by the controller (in the background: follow
/// `state`), or uploaded in parts (`PUT /images/{id}/upload`) with
/// `upload_bytes`. Once all of it is there, the host checks it and adds it
/// to the pool's image store (its own local disk, or the shared pool,
/// which its hosts make disks from directly). Other hosts copy it from
/// there to make VMs from it. It is downloaded as it is added, and never
/// again: the image keeps no URL (the audit log records it).
#[utoipa::path(post, path = "/images", tag = "images", request_body = CreateImage,
    responses((status = 201, body = ImageView), (status = 400), (status = 409)))]
async fn create_image(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<CreateImage>,
) -> ApiResult<(StatusCode, Json<ImageView>)> {
    let tenant = req.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::ImageWrite, tenant).await?;
    if req.name.trim().is_empty() || req.name.len() > 128 {
        return Err(ApiError::bad_request("name must be 1-128 characters"));
    }
    let source_url = match (&req.source_url, req.upload_bytes) {
        (Some(url), None) => {
            // Refused at once where it can be (a tenant's image from a
            // private address); names are checked as they are resolved.
            let parsed = reqwest::Url::parse(url)
                .map_err(|e| ApiError::bad_request(format!("source_url: {e}")))?;
            let policy = mst_storage::FetchPolicy {
                allow_private: mst_db::tenants::get(&state.db, tenant)
                    .await?
                    .parent_id
                    .is_none(),
                ..mst_storage::FetchPolicy::default()
            };
            mst_storage::check_url(&parsed, policy)
                .map_err(|e| ApiError::bad_request(format!("source_url: {e}")))?;
            Some(url.as_str())
        }
        (None, Some(n)) if n > 0 && n <= i64::MAX as u64 => None,
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
    let host = receiving_host(&state, &pool).await?;
    let mut tx = state.db.begin().await?;
    may_add(&mut tx, tenant).await?;
    let image = mst_db::images::create(
        &mut *tx,
        mst_db::images::NewImage {
            tenant_id: tenant,
            name: req.name.trim(),
            origin: if source_url.is_some() {
                "download"
            } else {
                "upload"
            },
            format: &req.format,
            shared: req.shared,
            storage_pool_id: pool.id,
            os,
            secure_boot: req.secure_boot,
            tpm: req.tpm,
            sleep: req.sleep,
            admin_user,
            file_bytes: req.upload_bytes.map(|n| n as i64),
            host,
        },
    )
    .await?;
    tx.commit().await?;
    if let Some(url) = source_url {
        crate::receive::download(
            state.clone(),
            image.clone(),
            url.to_owned(),
            Some(p.user_id),
        );
    }
    audit(
        &state.db,
        Some(&p),
        Some(tenant),
        "image.create",
        Some(("image", image.id.0)),
        json!({"name": image.name, "url": source_url, "upload_bytes": req.upload_bytes,
               "storage_pool": pool.name, "os": image.os,
               "secure_boot": image.secure_boot, "tpm": image.tpm, "sleep": image.sleep,
               "admin_user": image.admin_user}),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(ImageView::new(image, vec![], pool.name, false)),
    ))
}

/// A host for an image's file to come to: one that can take it into the
/// pool's store (connected, in service: not in maintenance or being
/// evacuated, reaching a shared pool, and with an agent that receives and
/// stores images), at random.
async fn receiving_host(state: &AppState, pool: &mst_db::storage_pools::Pool) -> ApiResult<HostId> {
    let receive = mst_db::hosts::supporting(&state.db, "receive_image").await?;
    let store = mst_db::hosts::supporting(&state.db, "store_image").await?;
    let hosts: Vec<_> = crate::engine::store_hosts(state)
        .await
        .map_err(ApiError::internal)?
        .into_iter()
        .filter(|h| h.state == "ready")
        .filter(|h| receive.contains(&h.id) && store.contains(&h.id))
        .filter(|h| pool.kind == "local" || crate::engine::reaches(h, pool))
        .collect();
    if hosts.is_empty() {
        return Err(ApiError::unavailable(match pool.kind.as_str() {
            "local" => {
                "no connected host in service to keep the image (with an agent that takes images: upgrade it)"
                    .to_owned()
            }
            _ => format!(
                "no connected host in service reaches {} (with an agent that takes images: upgrade it)",
                pool.name
            ),
        }));
    }
    Ok(hosts[rand::random_range(0..hosts.len())].id)
}

/// The most one request may carry of an upload.
const MAX_PART: usize = 32 << 20;
/// How many upload parts a controller holds at once (each read whole, up to
/// `MAX_PART`, before it goes on to its host).
static UPLOAD_PARTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UploadQuery {
    /// Where the part goes in the file: as much of it as has arrived
    /// (`received_bytes`).
    offset: u64,
}

/// Uploads a part of an image added with `upload_bytes`: the next up to
/// 32 MiB of the file, as the request's body, from `offset` (what has
/// arrived: `received_bytes`). Parts go to the host picked for it as they
/// come. Once all of it has arrived, it is added to its pool as a download
/// is (`storing`, then `ready`), checked the same way. An upload no part
/// reaches for a day fails. A failed upload has failed for good: delete it
/// and add it again.
#[utoipa::path(put, path = "/images/{id}/upload", tag = "images",
    params(("id" = ImageId, Path), UploadQuery),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses((status = 200, body = ImageView), (status = 400), (status = 404),
        (status = 409, description = "Not being uploaded, or the part is not the next one: see `received_bytes`"),
        (status = 413, description = "A part larger than 32 MiB, or an image larger than its host takes (it fails)"),
        (status = 503, description = "The host does not answer, or too many parts are on their way: send it again")))]
async fn upload_image(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<ImageId>,
    Query(q): Query<UploadQuery>,
    body: axum::body::Body,
) -> ApiResult<Json<ImageView>> {
    // Checked before the part is read.
    let image = mst_db::images::get(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::ImageWrite, image.tenant_id, "image").await?;
    if !image.uploaded() {
        return Err(ApiError::conflict("the image is downloaded, not uploaded"));
    }
    match image.state.as_str() {
        "uploading" => {}
        "failed" => {
            return Err(ApiError::conflict(format!(
                "uploading it failed ({}); delete it and add it again",
                image.error.as_deref().unwrap_or("no reason given")
            )));
        }
        _ => return Err(ApiError::conflict("all of the image has been uploaded")),
    }
    let total = image.file_bytes.unwrap_or_default() as u64;
    let here = image.received_bytes as u64;
    if q.offset != here {
        return Err(ApiError::conflict(format!(
            "{here} bytes of the image have arrived: send the part from offset {here}"
        )));
    }
    let host = image.download_host_id.ok_or_else(|| {
        ApiError::conflict("the host the upload went to is gone; delete it and add it again")
    })?;
    // Parts held at once here are bounded (each is read whole first).
    let _permit = UPLOAD_PARTS.try_acquire().map_err(|_| {
        ApiError::unavailable("too many upload parts are on their way here; send it again shortly")
    })?;
    let body = axum::body::to_bytes(body, MAX_PART).await.map_err(|_| {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            format!("a part is at most {} MiB", MAX_PART >> 20),
        )
    })?;
    let end = here + body.len() as u64;
    if end > total {
        return Err(ApiError::bad_request(format!(
            "the part goes beyond the image's {total} bytes"
        )));
    }
    if body.is_empty() {
        return Err(ApiError::bad_request("the part is empty"));
    }
    // No transaction is held while the host takes it: parts sent at once go
    // to the same place, and only one of them is recorded.
    match crate::receive::send_part(&state, host, id, here, &body, total).await {
        Ok(()) => {}
        // It has less than was sent (it lost some): sent again from there.
        Err(crate::receive::PartError::Behind(had)) => {
            mst_db::images::received_so_far(&state.db, id, had as i64).await?;
            return Err(ApiError::conflict(format!(
                "the host has {had} bytes of the image: send the part from offset {had}"
            )));
        }
        // It will never take it: the upload fails, for good.
        Err(crate::receive::PartError::Failed(e))
            if format!("{e:#}").contains("larger than the") =>
        {
            let why = format!("{e:#}");
            if mst_db::images::failed(&state.db, id, &why).await? {
                crate::engine::remove_leftover(&state, id).await;
            }
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                why,
            ));
        }
        Err(crate::receive::PartError::Failed(e)) => {
            return Err(ApiError::unavailable(format!("{e:#}")));
        }
    }
    if !mst_db::images::received_part(&state.db, id, here as i64, end as i64).await? {
        return Err(ApiError::conflict(
            "another part arrived meanwhile: see received_bytes for where to go on",
        ));
    }
    let done = if end == total {
        let mut tx = state.db.begin().await?;
        let done = crate::receive::done(&mut tx, &image, total, Some(p.user_id))
            .await
            .map_err(ApiError::internal)?;
        tx.commit().await?;
        done
    } else {
        false
    };
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
    if name.is_empty()
        || name.chars().count() > 20
        || name.chars().any(bad)
        || name.ends_with('.')
        || name.chars().all(|c| c == '.' || c == ' ')
        || name != name.trim()
    {
        return Err(ApiError::bad_request(
            "a Windows account name is 1-20 characters, none of \" / \\ [ ] : ; | = , + * ? < > @ %, not only dots and spaces, and does not start or end in a space or end in a dot",
        ));
    }
    // Windows' own accounts (switched off) and groups: no one signs in as
    // them.
    const BUILT_IN: &[&str] = &[
        "guest",
        "defaultaccount",
        "wdagutilityaccount",
        "krbtgt",
        "administrators",
        "users",
        "guests",
        "power users",
        "remote desktop users",
        "backup operators",
        "system",
        "local service",
        "network service",
        "everyone",
        "authenticated users",
    ];
    if BUILT_IN.contains(&name.to_lowercase().as_str()) {
        return Err(ApiError::bad_request(format!(
            "{name} is one of Windows' own accounts or groups: name another"
        )));
    }
    Ok(())
}

/// How many images a tenant tree (a child of the provider's, and its
/// sub-tenants; or the provider's own) may add at once (their files coming,
/// or being stored): each can take up to the host's largest image before
/// it is charged, and a store slot (see the engine) for hours.
const ADDING_AT_ONCE: i64 = 2;

/// Refuses adding an image where the tenant's image quota is used up (an
/// image is charged what it takes once stored, `image_gib`), or its tree is
/// adding two already (images being stored for deleted images count too,
/// until their tasks stop). Locks the tenant's lineage (see
/// `quotas::reserve`), which serializes this per tree, until the
/// transaction ends.
async fn may_add(tx: &mut mst_db::sqlx::PgConnection, tenant: TenantId) -> ApiResult<()> {
    let some = [("image_gib".to_owned(), 1)].into_iter().collect();
    mst_db::quotas::reserve(&mut *tx, tenant, &some).await?;
    if mst_db::images::adding_in_tree(&mut *tx, tenant).await? >= ADDING_AT_ONCE {
        return Err(ApiError::conflict(format!(
            "{ADDING_AT_ONCE} images of the tenant's are being added (its sub-tenants' and parent's count too); add more once they are done"
        )));
    }
    Ok(())
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
