use std::sync::LazyLock;
use std::time::Duration;

use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use hm_core::ids::{TenantId, UserId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::SESSION_COOKIE;
use crate::authz::Principal;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);

/// Verifying against this when the user does not exist keeps response timing
/// from revealing which usernames exist.
static DUMMY_HASH: LazyLock<String> =
    LazyLock::new(|| hash_password("not-a-real-password").unwrap());

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    Ok(Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|e| anyhow::anyhow!("hashing password: {e}"))?
        .to_string())
}

fn verify_password(password: &str, hash: &str) -> bool {
    Argon2::default()
        .verify_password(password.as_bytes(), hash)
        .is_ok()
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(login))
        .routes(routes!(logout))
        .routes(routes!(me))
}

#[derive(Deserialize, ToSchema)]
pub struct LoginRequest {
    username: String,
    password: String,
    /// Tenant as a slug path below the provider, e.g. `acme/dev`. Omit for
    /// provider users.
    #[serde(default)]
    tenant: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct RoleAt {
    tenant_id: TenantId,
    tenant_name: String,
    /// Slug path of the tenant (empty for the root).
    tenant: String,
    #[schema(value_type = String)]
    role: String,
}

#[derive(Serialize, ToSchema)]
pub struct Me {
    /// Roles the user holds; each also applies to the tenant's sub-tenants.
    roles: Vec<RoleAt>,
    user_id: UserId,
    username: String,
    display_name: String,
    tenant_id: TenantId,
    /// Slug path of the user's tenant, empty for the provider.
    tenant: String,
    tenant_name: String,
    /// Whether the user administers the provider (hosts, enrollment).
    provider_admin: bool,
    /// Signs in with a password (rather than through an identity provider).
    local: bool,
}

async fn resolve_tenant(
    state: &AppState,
    path: Option<&str>,
) -> ApiResult<Option<hm_db::tenants::Tenant>> {
    let Some(mut tenant) = hm_db::tenants::root(&state.db).await? else {
        return Ok(None);
    };
    for slug in path.unwrap_or("").split('/').filter(|s| !s.is_empty()) {
        match hm_db::tenants::child_by_slug(&state.db, Some(tenant.id), slug).await? {
            Some(t) => tenant = t,
            None => return Ok(None),
        }
    }
    Ok(Some(tenant))
}

#[utoipa::path(post, path = "/auth/login", tag = "auth", request_body = LoginRequest,
    responses((status = 204, description = "Logged in; session cookie set"), (status = 401)))]
async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> ApiResult<impl IntoResponse> {
    let user = match resolve_tenant(&state, req.tenant.as_deref()).await? {
        Some(t) => hm_db::users::find_by_username(&state.db, t.id, &req.username).await?,
        None => None,
    };
    let hash = user.as_ref().and_then(|u| u.password_hash.clone());
    let password = req.password;
    let ok = tokio::task::spawn_blocking(move || match hash {
        Some(h) => verify_password(&password, &h),
        None => {
            verify_password(&password, &DUMMY_HASH);
            false
        }
    })
    .await
    .map_err(ApiError::internal)?;
    let user = match user {
        Some(u) if ok && !u.disabled => u,
        other => {
            let entry = hm_db::audit::Entry {
                user_id: other.as_ref().map(|u| u.id),
                username: Some(&req.username),
                tenant_id: other.as_ref().map(|u| u.tenant_id),
                action: "auth.login",
                target: None,
                success: false,
                details: serde_json::json!({"tenant": req.tenant}),
            };
            if let Err(e) = hm_db::audit::record(&state.db, entry).await {
                tracing::error!(error = %e, "writing audit log");
            }
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
                "invalid username or password",
            ));
        }
    };

    let headers = open_session(&state, &user, "password").await?;
    Ok((StatusCode::NO_CONTENT, headers))
}

/// Starts a session for a user who just proved who they are, and returns the
/// `Set-Cookie` header carrying it.
pub async fn open_session(
    state: &AppState,
    user: &hm_db::users::User,
    method: &str,
) -> ApiResult<HeaderMap> {
    let secret = hm_pki::random_hex(32);
    hm_db::sessions::create(
        &state.db,
        &hm_pki::hash_secret(&secret),
        user.id,
        SESSION_TTL,
    )
    .await?;
    tracing::info!(user = %user.username, tenant = %user.tenant_id, method, "login");
    let entry = hm_db::audit::Entry {
        user_id: Some(user.id),
        username: Some(&user.username),
        tenant_id: Some(user.tenant_id),
        action: "auth.login",
        target: None,
        success: true,
        details: serde_json::json!({"method": method}),
    };
    if let Err(e) = hm_db::audit::record(&state.db, entry).await {
        tracing::error!(error = %e, "writing audit log");
    }
    let secure = if state.secure_cookies { "; Secure" } else { "" };
    let cookie = format!(
        "{SESSION_COOKIE}={secret}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure}",
        SESSION_TTL.as_secs()
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(ApiError::internal)?,
    );
    Ok(headers)
}

/// Resolves a slug path below the root (`acme/dev`) to a tenant.
pub async fn tenant_by_path(
    state: &AppState,
    path: Option<&str>,
) -> ApiResult<Option<hm_db::tenants::Tenant>> {
    resolve_tenant(state, path).await
}

#[utoipa::path(post, path = "/auth/logout", tag = "auth", responses((status = 204)))]
async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<impl IntoResponse> {
    if let Some(secret) = super::session_secret(&headers) {
        hm_db::sessions::delete(&state.db, &hm_pki::hash_secret(&secret)).await?;
    }
    let mut out = HeaderMap::new();
    out.insert(
        header::SET_COOKIE,
        HeaderValue::from_static("hm_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"),
    );
    Ok((StatusCode::NO_CONTENT, out))
}

#[utoipa::path(get, path = "/auth/me", tag = "auth", responses((status = 200, body = Me), (status = 401)))]
async fn me(State(state): State<AppState>, p: Principal) -> ApiResult<Json<Me>> {
    let user = hm_db::users::get(&state.db, p.user_id).await?;
    let lineage = hm_db::tenants::lineage(&state.db, p.tenant_id).await?;
    let slugs: Vec<_> = lineage.iter().skip(1).map(|t| t.slug.as_str()).collect();
    let bindings = hm_db::rbac::bindings_of(&state.db, p.user_id).await?;
    let ids: Vec<_> = bindings.iter().map(|b| b.tenant_id).collect();
    let paths = hm_db::tenants::slug_paths(&state.db, &ids).await?;
    let roles = bindings
        .into_iter()
        .map(|b| RoleAt {
            tenant: paths.get(&b.tenant_id).cloned().unwrap_or_default(),
            tenant_id: b.tenant_id,
            tenant_name: b.tenant_name,
            role: b.role,
        })
        .collect();
    Ok(Json(Me {
        local: user.password_hash.is_some(),
        roles,
        user_id: user.id,
        username: user.username,
        display_name: user.display_name,
        tenant_id: p.tenant_id,
        tenant: slugs.join("/"),
        tenant_name: lineage.last().map(|t| t.name.clone()).unwrap_or_default(),
        provider_admin: p.is_provider_admin(),
    }))
}
