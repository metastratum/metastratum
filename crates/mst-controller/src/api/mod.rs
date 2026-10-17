//! REST API (`/api/v1`) and its OpenAPI description.

mod audit;
mod auth;
mod backups;
mod clusters;
mod console;
mod events;
mod external_networks;
mod floating_ips;
mod hosts;
mod images;
mod oidc;
mod security;
mod snapshots;
mod storage_pools;
mod tasks;
mod tenants;
mod tokens;
mod users;
mod vms;
mod vpcs;

pub use auth::hash_password;
pub use backups::{queue_delete as queue_backup_delete, repository_pool, start as start_backup};
pub use snapshots::check_idle as check_disks_idle;
pub use storage_pools::agent_pool;

use axum::Router;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, header};
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;

use crate::authz::Principal;
use crate::error::ApiError;
use crate::state::AppState;

pub const SESSION_COOKIE: &str = "mst_session";

#[derive(OpenApi)]
#[openapi(
    info(title = "metastratum API", version = "1"),
    tags(
        (name = "auth"), (name = "tenants"), (name = "users"), (name = "vms"), (name = "images"),
        (name = "hosts"), (name = "tasks"), (name = "audit"), (name = "events"), (name = "networks"),
        (name = "backups"), (name = "snapshots"),
    )
)]
struct ApiDoc;

fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(auth::routes())
        .merge(oidc::routes())
        .merge(vms::routes())
        .merge(backups::routes())
        .merge(snapshots::routes())
        .merge(console::routes())
        .merge(images::routes())
        .merge(hosts::routes())
        .merge(tasks::routes())
        .merge(clusters::routes())
        .merge(storage_pools::routes())
        .merge(tenants::routes())
        .merge(users::routes())
        .merge(tokens::routes())
        .merge(audit::routes())
        .merge(events::routes())
        .merge(external_networks::routes())
        .merge(vpcs::routes())
        .merge(security::routes())
        .merge(floating_ips::routes())
}

/// The API router (mounted under `/api/v1`) and its OpenAPI document.
pub fn router() -> (Router<AppState>, utoipa::openapi::OpenApi) {
    let (router, mut doc) = OpenApiRouter::new()
        .nest("/api/v1", api())
        .split_for_parts();
    doc.info.description = Some("Management API for metastratum. Authenticate with POST /api/v1/auth/login; the session cookie authenticates later calls.".into());
    let spec = doc.clone();
    let router = router.route(
        "/api/v1/openapi.json",
        axum::routing::get(move || async move { axum::Json(spec) }),
    );
    (router, doc)
}

/// API tokens carry this prefix, so they are told apart from session IDs.
pub const TOKEN_PREFIX: &str = "msttok_";

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
        principal_for_secret(state, &secret).await
    }
}

/// The principal a session ID or API token stands for, if still valid.
async fn principal_for_secret(state: &AppState, secret: &str) -> Result<Principal, ApiError> {
    let hash = mst_pki::hash_secret(secret);
    if secret.starts_with(TOKEN_PREFIX) {
        let token = mst_db::rbac::resolve_token(&state.db, &hash)
            .await?
            .ok_or_else(ApiError::unauthorized)?;
        let p = Principal::load(&state.db, token.user_id).await?;
        return match token.scopes {
            Some(scopes) => p.limit_to(&state.db, &scopes).await,
            None => Ok(p),
        };
    }
    let user_id = mst_db::sessions::resolve(&state.db, &hash)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    Principal::load(&state.db, user_id).await
}

#[cfg(test)]
mod tests {
    /// Generated clients key operations by ID; duplicates silently merge.
    #[test]
    fn operation_ids_are_unique() {
        let (_, doc) = super::router();
        let mut seen = std::collections::HashSet::new();
        for (path, item) in &doc.paths.paths {
            for op in [&item.get, &item.put, &item.post, &item.delete, &item.patch]
                .into_iter()
                .flatten()
            {
                let id = op.operation_id.clone().unwrap_or_default();
                assert!(
                    seen.insert(id.clone()),
                    "operation ID {id} repeats (at {path})"
                );
            }
        }
    }
}
