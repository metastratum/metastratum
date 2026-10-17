//! Per-tenant OpenID Connect sign-in (authorization code flow with PKCE).
//!
//! Each tenant may configure one identity provider. Users of the tenant sign
//! in there; on return the ID token is verified (signature, issuer, audience,
//! expiry, nonce), the user is created on first sign-in (matched by the
//! provider's subject ID afterwards), and their roles at the tenant are
//! re-derived from their groups.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Extension, Json};
use mst_core::ids::TenantId;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, HttpRequest, HttpResponse, IssuerUrl,
    Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use super::auth::{open_session, tenant_by_path};
use crate::authz::{Perm, Principal, Role, audit, authorize_resource};
use crate::error::{ApiError, ApiResult};
use crate::ratelimit::{Https, Site};
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(methods))
        .routes(routes!(start))
        .routes(routes!(callback))
        .routes(routes!(get_provider, put_provider, delete_provider))
        .routes(routes!(get_sign_in))
}

const LOGIN_TTL: Duration = Duration::from_secs(600);
const DISCOVERY_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct HttpError(String);

/// Where the controller may connect on behalf of a tenant's identity
/// provider. Sub-tenants' providers must be on the internet: otherwise a
/// tenant administrator could make the controller probe its own network
/// (discovery errors are shown to them).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Reach {
    Any,
    PublicOnly,
}

/// The provider's (root tenant's) identity provider, and issuers the
/// operator listed in `api.trusted_oidc_issuers`, may be internal.
async fn reach(state: &AppState, tenant: TenantId, issuer: &str) -> mst_db::Result<Reach> {
    let trusted = state
        .trusted_oidc_issuers
        .iter()
        .any(|t| t.trim_end_matches('/') == issuer.trim_end_matches('/'))
        || mst_db::tenants::get(&state.db, tenant)
            .await?
            .parent_id
            .is_none();
    Ok(if trusted {
        Reach::Any
    } else {
        Reach::PublicOnly
    })
}

/// The HTTP client openidconnect uses: our reqwest, never following
/// redirects (an identity provider must not bounce us elsewhere), and with
/// [`Reach::PublicOnly`] connecting to public addresses only. That covers the
/// token and key endpoints too, whose URLs the discovery document names.
fn http_client(
    reach: Reach,
) -> impl Fn(HttpRequest) -> Pin<Box<dyn Future<Output = Result<HttpResponse, HttpError>> + Send>> {
    fn build(public_only: bool) -> reqwest::Client {
        let mut b = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10));
        if public_only {
            b = b
                .no_proxy()
                .dns_resolver(std::sync::Arc::new(mst_storage::PublicOnly));
        }
        b.build().expect("HTTP client")
    }
    static ANY: LazyLock<reqwest::Client> = LazyLock::new(|| build(false));
    static PUBLIC: LazyLock<reqwest::Client> = LazyLock::new(|| build(true));
    move |req: HttpRequest| {
        Box::pin(async move {
            let req = reqwest::Request::try_from(req).map_err(|e| HttpError(e.to_string()))?;
            let client = match reach {
                Reach::Any => &*ANY,
                Reach::PublicOnly => {
                    let policy = mst_storage::FetchPolicy::default();
                    mst_storage::check_url(req.url(), policy).map_err(HttpError)?;
                    &*PUBLIC
                }
            };
            let resp = client
                .execute(req)
                .await
                .map_err(|e| HttpError(chain(&e)))?;
            let mut out = HttpResponse::new(Vec::new());
            *out.status_mut() = resp.status();
            *out.headers_mut() = resp.headers().clone();
            *out.body_mut() = resp
                .bytes()
                .await
                .map_err(|e| HttpError(e.to_string()))?
                .to_vec();
            Ok(out)
        })
    }
}

/// An error with its causes, which openidconnect keeps out of `Display`.
fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        out.push_str(": ");
        out.push_str(&s.to_string());
        source = s.source();
    }
    out
}

/// Provider metadata, discovered and cached briefly per issuer (and reach,
/// so a document fetched for a trusted tenant is not handed to others).
async fn discover(issuer: &str, reach: Reach) -> Result<CoreProviderMetadata, String> {
    type Cache = HashMap<(String, Reach), (Instant, CoreProviderMetadata)>;
    static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Mutex::default);
    let key = (issuer.to_owned(), reach);
    if let Some((at, m)) = CACHE.lock().unwrap().get(&key)
        && at.elapsed() < DISCOVERY_TTL
    {
        return Ok(m.clone());
    }
    let url = IssuerUrl::new(issuer.to_owned()).map_err(|e| format!("invalid issuer URL: {e}"))?;
    let meta = CoreProviderMetadata::discover_async(url, &http_client(reach))
        .await
        .map_err(|e| format!("discovering {issuer}: {}", chain(&e)))?;
    CACHE
        .lock()
        .unwrap()
        .insert(key, (Instant::now(), meta.clone()));
    Ok(meta)
}

/// Where the identity provider sends the browser back: this site, as the
/// browser reached it. The provider accepts only the URIs registered with
/// it, so a request naming another host gets nowhere.
fn redirect_uri(site: &Site) -> Option<String> {
    Some(format!("{}/api/v1/auth/oidc/callback", site.0.as_deref()?))
}

#[derive(Serialize, ToSchema)]
pub struct SignInView {
    /// The tenant's sign-in page. At the domain bound nearest above the
    /// tenant (or to it), with the path below that tenant; else at the
    /// address the caller used, with the tenant's full path.
    page: String,
    /// Where an identity provider sends people back, at that same address:
    /// the URI to register with it.
    redirect_uri: String,
    /// The domain both are at, if one is bound to the tenant or above it.
    domain: Option<String>,
}

/// Where a tenant's people sign in, seen from `site`: at the nearest domain
/// (bound to the tenant at `depth`, 1 for the root) with the path below it,
/// keeping the caller's scheme and port, or else at the caller's address.
fn sign_in_at(
    site: &Site,
    slug_path: &str,
    nearest: Option<(String, usize)>,
) -> Option<SignInView> {
    let (base, below, domain) = match nearest {
        Some((domain, depth)) => {
            let below: Vec<_> = slug_path
                .split('/')
                .filter(|s| !s.is_empty())
                .skip(depth.saturating_sub(1))
                .collect();
            (site.with_host(&domain)?, below.join("/"), Some(domain))
        }
        None => (site.0.clone()?, slug_path.to_owned(), None),
    };
    let page = if below.is_empty() {
        format!("{base}/login")
    } else {
        format!("{base}/login?tenant={below}")
    };
    Some(SignInView {
        page,
        redirect_uri: format!("{base}/api/v1/auth/oidc/callback"),
        domain,
    })
}

async fn sign_in_for(state: &AppState, site: &Site, id: TenantId) -> ApiResult<SignInView> {
    let paths = mst_db::tenants::slug_paths(&state.db, &[id]).await?;
    let nearest = mst_db::tenants::nearest_domain(&state.db, id).await?;
    sign_in_at(site, paths.get(&id).map_or("", String::as_str), nearest)
        .ok_or_else(|| ApiError::bad_request("the request does not say which host it was sent to"))
}

/// Where the tenant's people sign in, and the redirect URI to register with
/// its identity provider.
#[utoipa::path(get, path = "/tenants/{id}/sign-in", tag = "tenants", params(("id" = TenantId, Path)),
    responses((status = 200, body = SignInView), (status = 404)))]
async fn get_sign_in(
    State(state): State<AppState>,
    Extension(site): Extension<Site>,
    p: Principal,
    Path(id): Path<TenantId>,
) -> ApiResult<Json<SignInView>> {
    authorize_resource(&state.db, &p, Perm::TenantRead, id, "tenant").await?;
    Ok(Json(sign_in_for(&state, &site, id).await?))
}

/// Only paths on this site, to avoid an open redirect. Browsers drop tabs
/// and newlines from a URL before reading it (`/\t/evil.example` becomes
/// `//evil.example`), so no control characters or spaces at all.
fn safe_return_to(r: Option<&str>) -> String {
    match r {
        Some(p)
            if p.starts_with('/')
                && !p.starts_with("//")
                && !p.contains('\\')
                && !p.chars().any(|c| c.is_control() || c.is_whitespace()) =>
        {
            p.to_owned()
        }
        _ => "/".into(),
    }
}

/// Binds a sign-in to the browser that started it: the pending sign-in is
/// found by its state together with this cookie's value, which only that
/// browser holds. Without it, someone could start a sign-in at their own
/// identity provider and hand the callback link to someone else, signing
/// them in as the sender. `Lax`, as the identity provider sends the browser
/// back from another site.
const BROWSER_COOKIE: &str = "mst_oidc";

fn browser_cookie(value: &str, max_age: u64, https: bool) -> ApiResult<HeaderValue> {
    let secure = if https { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{BROWSER_COOKIE}={value}; Path=/api/v1/auth/oidc/callback; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"
    ))
    .map_err(ApiError::internal)
}

fn pending_key(state: &str, browser: &str) -> String {
    mst_pki::hash_secret(&format!("{state}:{browser}"))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MethodsQuery {
    /// Slug path of the tenant (`acme/dev`), from the tenant the site's
    /// domain is bound to; empty for that tenant (or the provider).
    tenant: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct AuthMethods {
    /// Name of the tenant's identity provider, if it has one.
    oidc: Option<String>,
}

/// How users of a tenant can sign in. Unknown tenants look like tenants
/// without an identity provider, so this cannot be used to probe for them.
#[utoipa::path(get, path = "/auth/methods", tag = "auth", params(MethodsQuery), responses((status = 200, body = AuthMethods)))]
async fn methods(
    State(state): State<AppState>,
    Extension(site): Extension<Site>,
    Query(q): Query<MethodsQuery>,
) -> ApiResult<Json<AuthMethods>> {
    let oidc = match tenant_by_path(&state, &site, q.tenant.as_deref()).await? {
        Some(t) => mst_db::oidc::get(&state.db, t.id).await?.map(|p| p.name),
        None => None,
    };
    Ok(Json(AuthMethods { oidc }))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StartQuery {
    tenant: Option<String>,
    /// Path to return to after signing in.
    return_to: Option<String>,
}

fn login_error(message: &str) -> Response {
    let q: String = url_encode(message);
    Redirect::to(&format!("/login?error={q}")).into_response()
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Sends the browser to the tenant's identity provider.
#[utoipa::path(get, path = "/auth/oidc/start", tag = "auth", params(StartQuery), responses((status = 303)))]
async fn start(
    State(state): State<AppState>,
    Extension(site): Extension<Site>,
    Extension(Https(https)): Extension<Https>,
    Query(q): Query<StartQuery>,
) -> ApiResult<Response> {
    let Some(redirect) = redirect_uri(&site) else {
        return Ok(login_error(
            "The request did not say which host it was sent to. Sign in again from the login page.",
        ));
    };
    let Some(tenant) = tenant_by_path(&state, &site, q.tenant.as_deref()).await? else {
        return Ok(login_error(
            "That tenant does not sign in with an identity provider.",
        ));
    };
    let Some(provider) = mst_db::oidc::get(&state.db, tenant.id).await? else {
        return Ok(login_error(
            "That tenant does not sign in with an identity provider.",
        ));
    };
    let reach = reach(&state, tenant.id, &provider.issuer).await?;
    let meta = match discover(&provider.issuer, reach).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(tenant = %tenant.id, error = %e, "OIDC discovery failed");
            return Ok(login_error(
                "The identity provider cannot be reached. Try again later.",
            ));
        }
    };
    let client = CoreClient::from_provider_metadata(
        meta,
        ClientId::new(provider.client_id.clone()),
        Some(ClientSecret::new(
            crate::secrets::open(
                &crate::secrets::idp_secret(provider.tenant_id),
                &provider.client_secret,
            )
            .map_err(ApiError::internal)?,
        )),
    )
    .set_redirect_uri(RedirectUrl::new(redirect).map_err(ApiError::internal)?);
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let mut req = client.authorize_url(
        CoreAuthenticationFlow::AuthorizationCode,
        CsrfToken::new_random,
        Nonce::new_random,
    );
    for scope in provider
        .scopes
        .split_whitespace()
        .filter(|s| *s != "openid")
    {
        req = req.add_scope(Scope::new(scope.to_owned()));
    }
    let (url, csrf, nonce) = req.set_pkce_challenge(challenge).url();
    let browser = mst_pki::random_hex(32);
    mst_db::oidc::start_login(
        &state.db,
        &pending_key(csrf.secret(), &browser),
        &mst_db::oidc::PendingLogin {
            tenant_id: tenant.id,
            nonce: nonce.secret().clone(),
            pkce_verifier: verifier.secret().clone(),
            return_to: safe_return_to(q.return_to.as_deref()),
        },
        LOGIN_TTL,
    )
    .await?;
    let mut resp = Redirect::to(url.as_str()).into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        browser_cookie(&browser, LOGIN_TTL.as_secs(), https)?,
    );
    Ok(resp)
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Where the identity provider sends the browser back.
#[utoipa::path(get, path = "/auth/oidc/callback", tag = "auth", params(CallbackQuery), responses((status = 303)))]
async fn callback(
    State(state): State<AppState>,
    Extension(Https(https)): Extension<Https>,
    Extension(site): Extension<Site>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> ApiResult<Response> {
    let mut resp = callback_inner(&state, https, &site, &headers, q).await?;
    // The cookie is single-use too.
    resp.headers_mut()
        .append(header::SET_COOKIE, browser_cookie("", 0, https)?);
    Ok(resp)
}

async fn callback_inner(
    state: &AppState,
    https: bool,
    site: &Site,
    headers: &HeaderMap,
    q: CallbackQuery,
) -> ApiResult<Response> {
    if let Some(e) = &q.error {
        let detail = q.error_description.as_deref().unwrap_or(e);
        return Ok(login_error(&format!(
            "The identity provider refused the sign-in: {detail}"
        )));
    }
    let (Some(code), Some(csrf)) = (q.code, q.state) else {
        return Ok(login_error(
            "The identity provider returned an incomplete response.",
        ));
    };
    let browser = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == BROWSER_COOKIE)
        .map(|(_, v)| v.to_owned())
        .unwrap_or_default();
    let Some(pending) =
        mst_db::oidc::finish_login(&state.db, &pending_key(&csrf, &browser)).await?
    else {
        return Ok(login_error(
            "That sign-in expired, was already used, or was started in another browser. Start again.",
        ));
    };
    // The browser came back to the redirect URI the sign-in started with.
    let Some(redirect) = redirect_uri(site) else {
        return Ok(login_error(
            "The sign-in came back without a host. Start again.",
        ));
    };
    match finish(
        state,
        &redirect,
        pending.tenant_id,
        code,
        pending.nonce,
        pending.pkce_verifier,
    )
    .await
    {
        Ok(user) => {
            let headers = open_session(state, &user, "oidc", https).await?;
            Ok((headers, Redirect::to(&pending.return_to)).into_response())
        }
        Err(message) => {
            tracing::warn!(tenant = %pending.tenant_id, error = %message, "OIDC sign-in failed");
            let entry = mst_db::audit::Entry {
                user_id: None,
                username: None,
                tenant_id: Some(pending.tenant_id),
                action: "auth.login",
                target: None,
                success: false,
                details: json!({"method": "oidc", "error": message}),
            };
            let _ = mst_db::audit::record(&state.db, entry).await;
            Ok(login_error(&message))
        }
    }
}

/// Verifies the provider's response and resolves (or creates) the user.
/// Errors are messages for the person signing in.
async fn finish(
    state: &AppState,
    redirect: &str,
    tenant_id: TenantId,
    code: String,
    nonce: String,
    verifier: String,
) -> Result<mst_db::users::User, String> {
    let db_err = |e: mst_db::Error| {
        tracing::error!(error = %e, "OIDC sign-in");
        "Signing in failed on our side. Try again.".to_owned()
    };
    let provider = mst_db::oidc::get(&state.db, tenant_id)
        .await
        .map_err(db_err)?
        .ok_or("This tenant no longer signs in with an identity provider.")?;
    let reach = reach(state, tenant_id, &provider.issuer)
        .await
        .map_err(db_err)?;
    let meta = discover(&provider.issuer, reach).await?;
    let client = CoreClient::from_provider_metadata(
        meta,
        ClientId::new(provider.client_id.clone()),
        Some(ClientSecret::new(
            crate::secrets::open(
                &crate::secrets::idp_secret(provider.tenant_id),
                &provider.client_secret,
            )
            .map_err(|e| e.to_string())?,
        )),
    )
    .set_redirect_uri(RedirectUrl::new(redirect.to_owned()).map_err(|e| e.to_string())?);
    let tokens = client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|e| format!("The identity provider has no token endpoint: {e}"))?
        .set_pkce_verifier(PkceCodeVerifier::new(verifier))
        .request_async(&http_client(reach))
        .await
        .map_err(|e| format!("Exchanging the sign-in code failed: {}", chain(&e)))?;
    let id_token = tokens
        .id_token()
        .ok_or("The identity provider returned no ID token.")?;
    let claims = id_token
        .claims(&client.id_token_verifier(), &Nonce::new(nonce))
        .map_err(|e| format!("The ID token did not verify: {e}"))?;
    let subject = claims.subject().as_str().to_owned();

    // The token is verified; read the raw claims for configurable ones
    // (username, groups) that the typed claims do not model.
    let raw = raw_claims(&id_token.to_string()).ok_or("The ID token could not be read.")?;
    let username = [
        provider.username_claim.as_str(),
        "preferred_username",
        "email",
    ]
    .iter()
    .find_map(|c| raw.get(*c).and_then(Value::as_str))
    .unwrap_or(&subject)
    .to_owned();
    let display_name = raw
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(&username)
        .to_owned();
    let email = raw.get("email").and_then(Value::as_str).map(str::to_owned);
    let groups: Vec<String> = match raw.get(&provider.groups_claim) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => vec![],
    };
    let roles = roles_for(&provider, &groups);
    let denied = || {
        // For the administrator setting up group mappings.
        tracing::info!(%tenant_id, user = %username, ?groups, "no OIDC group maps to a role");
        "Your account has no role in this tenant. Ask an administrator for access.".to_owned()
    };
    // Someone unknown with no mapped role gets no account. A known user's
    // roles are re-synced either way: they may keep roles granted by hand.
    let known = mst_db::oidc::user_by_subject(&state.db, tenant_id, &subject)
        .await
        .map_err(db_err)?;
    if roles.is_empty() && known.is_none() {
        return Err(denied());
    }

    let mut tx = state.db.begin().await.map_err(|e| db_err(e.into()))?;
    let user_id = mst_db::oidc::upsert_user(
        &mut tx,
        tenant_id,
        &subject,
        &username,
        &display_name,
        email.as_deref(),
    )
    .await
    .map_err(|e| match e {
        mst_db::Error::Domain(mst_core::Error::Conflict(_)) => {
            format!("A local account named {username} already exists in this tenant.")
        }
        e => db_err(e),
    })?;
    mst_db::oidc::sync_roles(&mut tx, user_id, tenant_id, &roles)
        .await
        .map_err(db_err)?;
    tx.commit().await.map_err(|e| db_err(e.into()))?;
    let user = mst_db::users::get(&state.db, user_id)
        .await
        .map_err(db_err)?;
    if mst_db::rbac::bindings_of(&state.db, user_id)
        .await
        .map_err(db_err)?
        .is_empty()
    {
        return Err(denied());
    }
    if user.disabled {
        return Err("Your account is disabled.".into());
    }
    Ok(user)
}

/// The payload of a (verified) JWT.
fn raw_claims(jwt: &str) -> Option<serde_json::Map<String, Value>> {
    use base64::Engine;
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Roles from mapped groups, else the default role.
fn roles_for(p: &mst_db::oidc::Provider, groups: &[String]) -> Vec<String> {
    let mut roles: Vec<String> = p
        .role_mappings
        .0
        .iter()
        .filter(|m| groups.contains(&m.group))
        .map(|m| m.role.clone())
        .collect();
    if roles.is_empty()
        && let Some(d) = &p.default_role
    {
        roles.push(d.clone());
    }
    roles.sort();
    roles.dedup();
    roles
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct ProviderView {
    /// Shown on the sign-in button.
    name: String,
    issuer: String,
    client_id: String,
    /// Write-only: never returned. Omit on update to keep the stored one.
    #[serde(default, skip_serializing)]
    client_secret: Option<String>,
    /// Space-separated; `openid` is implied. Default `openid profile email`.
    scopes: Option<String>,
    /// Claim holding the username (default `preferred_username`).
    username_claim: Option<String>,
    /// Claim holding group names (default `groups`).
    groups_claim: Option<String>,
    /// Role for users with no mapped group; omit to refuse them.
    #[schema(value_type = Option<String>)]
    default_role: Option<Role>,
    #[serde(default)]
    role_mappings: Vec<MappingView>,
    /// URL to register with the identity provider (see `SignInView`).
    #[serde(default)]
    redirect_uri: String,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub struct MappingView {
    group: String,
    #[schema(value_type = String)]
    role: Role,
}

#[utoipa::path(get, path = "/tenants/{id}/identity-provider", tag = "tenants", params(("id" = TenantId, Path)),
    responses((status = 200, body = ProviderView), (status = 404)))]
async fn get_provider(
    State(state): State<AppState>,
    Extension(site): Extension<Site>,
    p: Principal,
    Path(id): Path<TenantId>,
) -> ApiResult<Json<ProviderView>> {
    authorize_resource(&state.db, &p, Perm::UserRead, id, "tenant").await?;
    let prov = mst_db::oidc::get(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("identity provider"))?;
    Ok(Json(ProviderView {
        name: prov.name,
        issuer: prov.issuer,
        client_id: prov.client_id,
        client_secret: None,
        scopes: Some(prov.scopes),
        username_claim: Some(prov.username_claim),
        groups_claim: Some(prov.groups_claim),
        default_role: prov.default_role.and_then(|r| r.parse().ok()),
        role_mappings: prov
            .role_mappings
            .0
            .into_iter()
            .filter_map(|m| {
                Some(MappingView {
                    group: m.group,
                    role: m.role.parse().ok()?,
                })
            })
            .collect(),
        redirect_uri: sign_in_for(&state, &site, id)
            .await
            .map(|s| s.redirect_uri)
            .unwrap_or_default(),
    }))
}

/// Configures the tenant's identity provider. The issuer must be reachable:
/// its discovery document is fetched before saving.
#[utoipa::path(put, path = "/tenants/{id}/identity-provider", tag = "tenants", params(("id" = TenantId, Path)),
    request_body = ProviderView, responses((status = 200, body = ProviderView), (status = 400), (status = 403)))]
async fn put_provider(
    State(state): State<AppState>,
    site: Extension<Site>,
    p: Principal,
    Path(id): Path<TenantId>,
    Json(req): Json<ProviderView>,
) -> ApiResult<Json<ProviderView>> {
    authorize_resource(&state.db, &p, Perm::UserWrite, id, "tenant").await?;
    let existing = mst_db::oidc::get(&state.db, id).await?;
    let client_secret = match (req.client_secret.filter(|s| !s.is_empty()), &existing) {
        (Some(s), _) => crate::secrets::seal(&crate::secrets::idp_secret(id), &s),
        (None, Some(e)) => e.client_secret.clone(),
        (None, None) => return Err(ApiError::bad_request("a client secret is required")),
    };
    let name = req.name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(ApiError::bad_request("name must be 1-64 characters"));
    }
    if !(req.issuer.starts_with("https://") || req.issuer.starts_with("http://"))
        || req.client_id.trim().is_empty()
    {
        return Err(ApiError::bad_request(
            "an http(s) issuer URL and a client ID are required",
        ));
    }
    let reach = reach(&state, id, &req.issuer).await?;
    discover(&req.issuer, reach)
        .await
        .map_err(ApiError::bad_request)?;
    let provider = mst_db::oidc::Provider {
        tenant_id: id,
        name: name.to_owned(),
        issuer: req.issuer.clone(),
        client_id: req.client_id.trim().to_owned(),
        client_secret,
        scopes: req
            .scopes
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "openid profile email".into()),
        username_claim: req
            .username_claim
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "preferred_username".into()),
        groups_claim: req
            .groups_claim
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "groups".into()),
        default_role: req.default_role.map(|r| r.as_str().to_owned()),
        role_mappings: mst_db::sqlx::types::Json(
            req.role_mappings
                .iter()
                .map(|m| mst_db::oidc::RoleMapping {
                    group: m.group.clone(),
                    role: m.role.as_str().to_owned(),
                })
                .collect(),
        ),
    };
    mst_db::oidc::upsert(&state.db, &provider).await?;
    audit(
        &state.db,
        Some(&p),
        Some(id),
        "tenant.identity_provider",
        Some(("tenant", id.0)),
        json!({"issuer": provider.issuer, "client_id": provider.client_id}),
    )
    .await;
    get_provider(State(state), site, p, Path(id)).await
}

#[utoipa::path(delete, path = "/tenants/{id}/identity-provider", tag = "tenants", params(("id" = TenantId, Path)),
    responses((status = 204), (status = 403), (status = 404)))]
async fn delete_provider(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TenantId>,
) -> ApiResult<StatusCode> {
    authorize_resource(&state.db, &p, Perm::UserWrite, id, "tenant").await?;
    mst_db::oidc::delete(&state.db, id).await?;
    audit(
        &state.db,
        Some(&p),
        Some(id),
        "tenant.identity_provider.delete",
        Some(("tenant", id.0)),
        json!({}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    #[test]
    fn sign_in_is_at_the_nearest_domain() {
        let site = super::Site(Some("https://cloud.example.net".into()));
        let at = |path: &str, nearest: Option<(&str, usize)>| {
            let v =
                super::sign_in_at(&site, path, nearest.map(|(d, n)| (d.to_owned(), n))).unwrap();
            (v.page, v.redirect_uri)
        };
        // No domain: this address, the full path.
        assert_eq!(
            at("acme/dev", None),
            (
                "https://cloud.example.net/login?tenant=acme/dev".into(),
                "https://cloud.example.net/api/v1/auth/oidc/callback".into()
            )
        );
        assert_eq!(at("", None).0, "https://cloud.example.net/login");
        // Bound to acme (depth 2): the path below it.
        assert_eq!(
            at("acme/dev/web", Some(("mst.acme.com", 2))),
            (
                "https://mst.acme.com/login?tenant=dev/web".into(),
                "https://mst.acme.com/api/v1/auth/oidc/callback".into()
            )
        );
        assert_eq!(
            at("acme", Some(("mst.acme.com", 2))).0,
            "https://mst.acme.com/login"
        );
        // Bound to the root: like no domain, at that name.
        assert_eq!(
            at("acme", Some(("cloud.example.org", 1))).0,
            "https://cloud.example.org/login?tenant=acme"
        );
    }

    use super::*;

    #[test]
    fn return_paths_stay_on_site() {
        assert_eq!(safe_return_to(Some("/vms/1")), "/vms/1");
        for bad in [
            "https://evil.example",
            "//evil.example",
            "/\\evil",
            "vms",
            // Browsers drop these, leaving `//evil.example`.
            "/\t/evil.example",
            "/\n/evil.example",
            "/ /evil.example",
        ] {
            assert_eq!(safe_return_to(Some(bad)), "/", "{bad}");
        }
        assert_eq!(safe_return_to(None), "/");
    }

    #[test]
    fn roles_come_from_groups_or_the_default() {
        let p = mst_db::oidc::Provider {
            tenant_id: TenantId::new(),
            name: "x".into(),
            issuer: "https://i".into(),
            client_id: "c".into(),
            client_secret: "s".into(),
            scopes: "openid".into(),
            username_claim: "preferred_username".into(),
            groups_claim: "groups".into(),
            default_role: Some("viewer".into()),
            role_mappings: mst_db::sqlx::types::Json(vec![
                mst_db::oidc::RoleMapping {
                    group: "ops".into(),
                    role: "operator".into(),
                },
                mst_db::oidc::RoleMapping {
                    group: "admins".into(),
                    role: "admin".into(),
                },
            ]),
        };
        assert_eq!(
            roles_for(&p, &["ops".into(), "admins".into()]),
            ["admin", "operator"]
        );
        assert_eq!(roles_for(&p, &["other".into()]), ["viewer"]);
        let strict = mst_db::oidc::Provider {
            default_role: None,
            ..p
        };
        assert!(roles_for(&strict, &[]).is_empty());
    }

    #[test]
    fn jwt_payloads_decode() {
        let jwt = "e30.eyJzdWIiOiJ4IiwiZ3JvdXBzIjpbImEiXX0.sig";
        let c = raw_claims(jwt).unwrap();
        assert_eq!(c["groups"], json!(["a"]));
    }
}
