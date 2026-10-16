use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use hm_core::ids::{ImageId, TenantId};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::authz::{Perm, Principal, authorize, authorize_resource};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_images, create_image))
        .routes(routes!(delete_image))
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
}

impl From<hm_db::images::Image> for ImageView {
    fn from(i: hm_db::images::Image) -> Self {
        Self {
            id: i.id,
            tenant_id: i.tenant_id,
            name: i.name,
            source_url: i.source_url,
            format: i.format,
            shared: i.shared,
            created_at: i.created_at,
        }
    }
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
    Ok(Json(
        hm_db::images::list_usable(&state.db, tenant)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

#[derive(Deserialize, ToSchema)]
pub struct CreateImage {
    tenant_id: Option<TenantId>,
    name: String,
    /// HTTP(S) URL hosts download the image from on first use.
    source_url: String,
    /// `qcow2` or `raw`.
    format: String,
    #[serde(default)]
    shared: bool,
}

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
    if !(req.source_url.starts_with("http://") || req.source_url.starts_with("https://")) {
        return Err(ApiError::bad_request("source_url must be an http(s) URL"));
    }
    if !matches!(req.format.as_str(), "qcow2" | "raw") {
        return Err(ApiError::bad_request("format must be qcow2 or raw"));
    }
    let image = hm_db::images::create(
        &state.db,
        hm_db::images::NewImage {
            tenant_id: tenant,
            name: req.name.trim(),
            source_url: &req.source_url,
            format: &req.format,
            shared: req.shared,
        },
    )
    .await?;
    Ok((StatusCode::CREATED, Json(image.into())))
}

#[utoipa::path(delete, path = "/images/{id}", tag = "images", params(("id" = ImageId, Path)),
    responses((status = 204), (status = 404), (status = 409, description = "In use by volumes")))]
async fn delete_image(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<ImageId>,
) -> ApiResult<StatusCode> {
    let image = hm_db::images::get(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::ImageWrite, image.tenant_id, "image").await?;
    hm_db::images::delete(&state.db, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
