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
use axum::http::StatusCode;
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
use crate::ratelimit::Https;
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(methods))
        .routes(routes!(start))
        .routes(routes!(callback))
        .routes(routes!(get_provider, put_provider, delete_provider))
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
/// operator listed in `--trusted-oidc-issuers`, may be internal.
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

fn redirect_uri(state: &AppState) -> String {
    format!(
        "{}/api/v1/auth/oidc/callback",
        state.public_url.trim_end_matches('/')
    )
}

/// Only paths on this site, to avoid an open redirect.
fn safe_return_to(r: Option<&str>) -> String {
    match r {
        Some(p) if p.starts_with('/') && !p.starts_with("//") && !p.contains('\\') => p.to_owned(),
        _ => "/".into(),
    }
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MethodsQuery {
    /// Slug path of the tenant (`acme/dev`); empty for the provider.
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
    Query(q): Query<MethodsQuery>,
) -> ApiResult<Json<AuthMethods>> {
    let oidc = match tenant_by_path(&state, q.tenant.as_deref()).await? {
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
async fn start(State(state): State<AppState>, Query(q): Query<StartQuery>) -> ApiResult<Response> {
    let Some(tenant) = tenant_by_path(&state, q.tenant.as_deref()).await? else {
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
    .set_redirect_uri(RedirectUrl::new(redirect_uri(&state)).map_err(ApiError::internal)?);
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
    mst_db::oidc::start_login(
        &state.db,
        &mst_pki::hash_secret(csrf.secret()),
        &mst_db::oidc::PendingLogin {
            tenant_id: tenant.id,
            nonce: nonce.secret().clone(),
            pkce_verifier: verifier.secret().clone(),
            return_to: safe_return_to(q.return_to.as_deref()),
        },
        LOGIN_TTL,
    )
    .await?;
    Ok(Redirect::to(url.as_str()).into_response())
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
    Query(q): Query<CallbackQuery>,
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
    let Some(pending) = mst_db::oidc::finish_login(&state.db, &mst_pki::hash_secret(&csrf)).await?
    else {
        return Ok(login_error(
            "That sign-in expired or was already used. Start again.",
        ));
    };
    match finish(
        &state,
        pending.tenant_id,
        code,
        pending.nonce,
        pending.pkce_verifier,
    )
    .await
    {
        Ok(user) => {
            let headers = open_session(&state, &user, "oidc", https).await?;
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
    .set_redirect_uri(RedirectUrl::new(redirect_uri(state)).map_err(|e| e.to_string())?);
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
    /// URL to register with the identity provider.
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
        redirect_uri: redirect_uri(&state),
    }))
}

/// Configures the tenant's identity provider. The issuer must be reachable:
/// its discovery document is fetched before saving.
#[utoipa::path(put, path = "/tenants/{id}/identity-provider", tag = "tenants", params(("id" = TenantId, Path)),
    request_body = ProviderView, responses((status = 200, body = ProviderView), (status = 400), (status = 403)))]
async fn put_provider(
    State(state): State<AppState>,
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
    get_provider(State(state), p, Path(id)).await
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
    use super::*;

    #[test]
    fn return_paths_stay_on_site() {
        assert_eq!(safe_return_to(Some("/vms/1")), "/vms/1");
        for bad in ["https://evil.example", "//evil.example", "/\\evil", "vms"] {
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
