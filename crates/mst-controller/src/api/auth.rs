use std::sync::LazyLock;
use std::time::Duration;

use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use axum::Extension;
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mst_core::ids::{TenantId, UserId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::SESSION_COOKIE;
use crate::authz::Principal;
use crate::error::{ApiError, ApiResult};
use crate::ratelimit::{ClientIp, Https, Site};
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

pub(super) fn verify_password(password: &str, hash: &str) -> bool {
    Argon2::default()
        .verify_password(password.as_bytes(), hash)
        .is_ok()
}

/// What failed sign-ins count against: the account, wherever they come
/// from, under whichever domain.
pub(super) fn account_key(tenant: TenantId, username: &str) -> String {
    format!("{tenant}/{}", username.to_lowercase())
}

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(site_info))
        .routes(routes!(login))
        .routes(routes!(logout))
        .routes(routes!(me))
}

#[derive(Deserialize, ToSchema)]
pub struct LoginRequest {
    username: String,
    password: String,
    /// Tenant as a slug path, e.g. `acme/dev`. It starts from the tenant
    /// the site's domain is bound to, else from the provider; omit it for
    /// that tenant itself.
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
    /// Signs in with a second factor too.
    two_factor: bool,
    /// The tenant, or one above it, requires a second factor, and the user
    /// has none yet: adding one is all they can do until they have.
    two_factor_needed: bool,
}

/// The tenant this site is bound to by its domain name, if any.
async fn bound_tenant(state: &AppState, site: &Site) -> ApiResult<Option<mst_db::tenants::Tenant>> {
    match site.host() {
        Some(host) => Ok(mst_db::tenants::by_domain(&state.db, &host).await?),
        None => Ok(None),
    }
}

/// Where tenant paths entered at this site start: the tenant its domain is
/// bound to, or else the provider.
async fn base_tenant(state: &AppState, site: &Site) -> ApiResult<Option<mst_db::tenants::Tenant>> {
    match bound_tenant(state, site).await? {
        Some(t) => Ok(Some(t)),
        None => Ok(mst_db::tenants::root(&state.db).await?),
    }
}

async fn resolve_tenant(
    state: &AppState,
    site: &Site,
    path: Option<&str>,
) -> ApiResult<Option<mst_db::tenants::Tenant>> {
    let Some(mut tenant) = base_tenant(state, site).await? else {
        return Ok(None);
    };
    for slug in path.unwrap_or("").split('/').filter(|s| !s.is_empty()) {
        match mst_db::tenants::child_by_slug(&state.db, Some(tenant.id), slug).await? {
            // A tenant with a domain of its own is signed in to there only:
            // from here, it and its sub-tenants do not exist.
            Some(t) if !mst_db::tenants::has_domain(&state.db, t.id).await? => tenant = t,
            _ => return Ok(None),
        }
    }
    Ok(Some(tenant))
}

#[derive(Serialize, ToSchema)]
pub struct SiteView {
    /// Name of the tenant this site's domain is bound to; null when it is
    /// bound to none, and sign-ins start from the provider.
    tenant_name: Option<String>,
    /// Whether the tenant sign-ins start from has sub-tenants that can be
    /// signed in to here (those without a domain of their own): whether the
    /// sign-in page needs to ask for one.
    sub_tenants: bool,
}

/// What the sign-in page at this address needs to know: which tenant its
/// domain is bound to, and whether there are sub-tenants to choose from.
#[utoipa::path(get, path = "/auth/site", tag = "auth", responses((status = 200, body = SiteView)))]
async fn site_info(
    State(state): State<AppState>,
    Extension(site): Extension<Site>,
) -> ApiResult<Json<SiteView>> {
    let bound = bound_tenant(&state, &site).await?;
    let base = match &bound {
        Some(t) => Some(t.clone()),
        None => mst_db::tenants::root(&state.db).await?,
    };
    let sub_tenants = match &base {
        Some(t) => mst_db::tenants::has_undomained_children(&state.db, t.id).await?,
        None => false,
    };
    Ok(Json(SiteView {
        tenant_name: bound.map(|t| t.name),
        sub_tenants,
    }))
}

/// Answered when the password was right and a second factor is to come.
#[derive(Serialize, ToSchema)]
pub struct LoginNextStep {
    second_factor: super::two_factor::SecondFactorChallenge,
}

#[utoipa::path(post, path = "/auth/login", tag = "auth", request_body = LoginRequest,
    responses((status = 204, description = "Logged in; session cookie set"),
        (status = 200, body = LoginNextStep,
            description = "The password was right; send a second factor to /auth/login/second-factor"),
        (status = 401),
        (status = 429, description = "Too many attempts; see Retry-After")))]
async fn login(
    State(state): State<AppState>,
    Extension(ClientIp(ip)): Extension<ClientIp>,
    Extension(Https(https)): Extension<Https>,
    Extension(site): Extension<Site>,
    Json(req): Json<LoginRequest>,
) -> ApiResult<Response> {
    if let Err(retry) = state.limits.logins.take(&ip).await {
        return Ok(crate::ratelimit::too_many(retry));
    }
    let tenant = resolve_tenant(&state, &site, req.tenant.as_deref()).await?;
    // Failures count against the account wherever they come from, under
    // whichever domain.
    let username = req.username.to_lowercase();
    let account = match &tenant {
        Some(t) => account_key(t.id, &username),
        None => format!(
            "{}/{}/{username}",
            site.host().unwrap_or_default(),
            req.tenant.as_deref().unwrap_or("").trim_matches('/')
        ),
    };
    if let Err(retry) = state.limits.login_failures.check(&account).await {
        return Ok(crate::ratelimit::too_many(retry));
    }
    let user = match &tenant {
        Some(t) => mst_db::users::find_by_username(&state.db, t.id, &req.username).await?,
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
            let _ = state.limits.login_failures.take(&account).await;
            metrics::counter!("mst_logins_total", "method" => "password", "outcome" => "failed")
                .increment(1);
            let entry = mst_db::audit::Entry {
                user_id: other.as_ref().map(|u| u.id),
                username: Some(&req.username),
                tenant_id: other.as_ref().map(|u| u.tenant_id),
                action: "auth.login",
                target: None,
                success: false,
                details: serde_json::json!({"tenant": req.tenant, "site": site.host()}),
            };
            if let Err(e) = mst_db::audit::record(&state.db, entry).await {
                tracing::error!(error = %e, "writing audit log");
            }
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
                "invalid username or password",
            ));
        }
    };

    let factors = mst_db::two_factor::factors(&state.db, user.id).await?;
    if !factors.is_empty() {
        let second_factor =
            super::two_factor::begin_login(&state, &user, &factors, &site, https).await?;
        return Ok(Json(LoginNextStep { second_factor }).into_response());
    }
    let headers = open_session(&state, &user, "password", https).await?;
    Ok((StatusCode::NO_CONTENT, headers).into_response())
}

/// Starts a session for a user who just proved who they are, and returns the
/// `Set-Cookie` header carrying it (Secure when the client came over HTTPS).
pub async fn open_session(
    state: &AppState,
    user: &mst_db::users::User,
    method: &str,
    https: bool,
) -> ApiResult<HeaderMap> {
    metrics::counter!("mst_logins_total", "method" => method.to_owned(), "outcome" => "succeeded")
        .increment(1);
    let secret = mst_pki::random_hex(32);
    mst_db::sessions::create(
        &state.db,
        &mst_pki::hash_secret(&secret),
        user.id,
        SESSION_TTL,
    )
    .await?;
    tracing::info!(user = %user.username, tenant = %user.tenant_id, method, "login");
    let entry = mst_db::audit::Entry {
        user_id: Some(user.id),
        username: Some(&user.username),
        tenant_id: Some(user.tenant_id),
        action: "auth.login",
        target: None,
        success: true,
        details: serde_json::json!({"method": method}),
    };
    if let Err(e) = mst_db::audit::record(&state.db, entry).await {
        tracing::error!(error = %e, "writing audit log");
    }
    let secure = if https { "; Secure" } else { "" };
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

/// Resolves a slug path (`acme/dev`) to a tenant, starting from the tenant
/// the site's domain is bound to, or else the provider. Tenants with a domain
/// of their own, and theirs, are not found: they are signed in to there.
pub async fn tenant_by_path(
    state: &AppState,
    site: &Site,
    path: Option<&str>,
) -> ApiResult<Option<mst_db::tenants::Tenant>> {
    resolve_tenant(state, site, path).await
}

#[utoipa::path(post, path = "/auth/logout", tag = "auth", responses((status = 204)))]
async fn logout(
    State(state): State<AppState>,
    Extension(Https(https)): Extension<Https>,
    headers: HeaderMap,
) -> ApiResult<impl IntoResponse> {
    if let Some(secret) = super::session_secret(&headers) {
        mst_db::sessions::delete(&state.db, &mst_pki::hash_secret(&secret)).await?;
    }
    let secure = if https { "; Secure" } else { "" };
    let cookie = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{secure}");
    let mut out = HeaderMap::new();
    out.insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(ApiError::internal)?,
    );
    Ok((StatusCode::NO_CONTENT, out))
}

#[utoipa::path(get, path = "/auth/me", tag = "auth", responses((status = 200, body = Me), (status = 401)))]
async fn me(State(state): State<AppState>, p: Principal) -> ApiResult<Json<Me>> {
    let user = mst_db::users::get(&state.db, p.user_id).await?;
    let two_factor = mst_db::two_factor::has_factor(&state.db, p.user_id).await?;
    let lineage = mst_db::tenants::lineage(&state.db, p.tenant_id).await?;
    let slugs: Vec<_> = lineage.iter().skip(1).map(|t| t.slug.as_str()).collect();
    let bindings = mst_db::rbac::bindings_of(&state.db, p.user_id).await?;
    let ids: Vec<_> = bindings.iter().map(|b| b.tenant_id).collect();
    let paths = mst_db::tenants::slug_paths(&state.db, &ids).await?;
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
        two_factor,
        two_factor_needed: p.enroll_only,
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
