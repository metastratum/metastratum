//! REST API (`/api/v1`) and its OpenAPI description.

mod audit;
mod auth;
mod console;
mod hosts;
mod images;
mod tasks;
mod tenants;
mod tokens;
mod users;
mod vms;

pub use auth::hash_password;

use axum::Router;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, header};
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;

use crate::authz::Principal;
use crate::error::ApiError;
use crate::state::AppState;

pub const SESSION_COOKIE: &str = "hm_session";

#[derive(OpenApi)]
#[openapi(
    info(title = "hypermon API", version = "1"),
    tags(
        (name = "auth"), (name = "tenants"), (name = "users"), (name = "vms"), (name = "images"),
        (name = "hosts"), (name = "tasks"), (name = "audit"),
    )
)]
struct ApiDoc;

fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(auth::routes())
        .merge(vms::routes())
        .merge(console::routes())
        .merge(images::routes())
        .merge(hosts::routes())
        .merge(tasks::routes())
        .merge(tenants::routes())
        .merge(users::routes())
        .merge(tokens::routes())
        .merge(audit::routes())
}

/// The API router (mounted under `/api/v1`) and its OpenAPI document.
pub fn router() -> (Router<AppState>, utoipa::openapi::OpenApi) {
    let (router, mut doc) = OpenApiRouter::new()
        .nest("/api/v1", api())
        .split_for_parts();
    doc.info.description = Some("Management API for hypermon. Authenticate with POST /api/v1/auth/login; the session cookie authenticates later calls.".into());
    let spec = doc.clone();
    let router = router.route(
        "/api/v1/openapi.json",
        axum::routing::get(move || async move { axum::Json(spec) }),
    );
    (router, doc)
}

/// API tokens carry this prefix, so they are told apart from session IDs.
pub const TOKEN_PREFIX: &str = "hmtok_";

/// Extracts the session secret or API token from the cookie or a bearer token.
fn session_secret(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        && let Some(t) = v.strip_prefix("Bearer ")
    {
        return Some(t.trim().to_owned());
    }
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == SESSION_COOKIE)
        .map(|(_, v)| v.to_owned())
}

impl FromRequestParts<AppState> for Principal {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let secret = session_secret(&parts.headers).ok_or_else(ApiError::unauthorized)?;
        let hash = hm_pki::hash_secret(&secret);
        let user_id = if secret.starts_with(TOKEN_PREFIX) {
            hm_db::rbac::resolve_token(&state.db, &hash).await?
        } else {
            hm_db::sessions::resolve(&state.db, &hash).await?
        }
        .ok_or_else(ApiError::unauthorized)?;
        Principal::load(&state.db, user_id).await
    }
}
