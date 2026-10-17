//! Two-factor sign-in for password accounts: the second step of signing
//! in, people adding and removing their second factors, administrators
//! resetting them, and tenants requiring them.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use mst_core::ids::{TenantId, UserId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::authz::{Perm, Principal, audit, authorize, authorize_resource};
use crate::error::{ApiError, ApiResult};
use crate::ratelimit::{Https, Site};
use crate::state::AppState;
use crate::two_factor::{self as tf, CodeKind, totp, webauthn, yubico};

/// How long the second step of a sign-in may take, and how many wrong
/// answers it gets.
const LOGIN_TTL: Duration = Duration::from_secs(300);
const LOGIN_ATTEMPTS: i32 = 5;
/// How long adding a factor may take between its two steps.
const ENROLL_TTL: Duration = Duration::from_secs(600);
const ENROLL_ATTEMPTS: i32 = 5;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(second_step))
        .routes(routes!(status, reset))
        .routes(routes!(start_totp))
        .routes(routes!(add_totp))
        .routes(routes!(start_webauthn))
        .routes(routes!(add_webauthn))
        .routes(routes!(add_yubico_otp))
        .routes(routes!(new_backup_codes))
        .routes(routes!(remove_factor))
        .routes(routes!(get_policy, set_policy))
}

/// What a session that must first add a second factor may reach: who it
/// is, and its own second factors.
pub fn enrollment_path(path: &str, user: UserId) -> bool {
    let path = path.strip_prefix("/api/v1").unwrap_or(path);
    let own = format!("/users/{user}/two-factor");
    path == "/auth/me"
        || path
            .strip_prefix(&own)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn sealed_context(user: UserId) -> String {
    format!("second_factor.totp:{user}")
}

// ------------------------------------------------------------ signing in

/// The password was right; a second factor is to come.
#[derive(Serialize, ToSchema)]
pub struct SecondFactorChallenge {
    /// Send it back with the second factor, within five minutes.
    token: String,
    /// What the user can answer with: `totp`, `webauthn`, `yubico_otp`,
    /// `backup_code`.
    methods: Vec<String>,
    /// Options for `navigator.credentials.get()`, when the user has
    /// security keys registered for this address.
    #[schema(value_type = Option<Object>)]
    webauthn: Option<Value>,
}

/// The relying party for WebAuthn: the address the browser is at. None
/// where browsers will not use security keys: at an IP address, or over
/// plain HTTP anywhere but localhost.
fn relying_party(site: &Site, https: bool) -> Option<webauthn::Rp> {
    let host = site.host()?;
    let is_ip = host.parse::<std::net::IpAddr>().is_ok();
    let local = host == "localhost" || host.ends_with(".localhost");
    if is_ip || !https && !local {
        return None;
    }
    Some(webauthn::Rp {
        id: host,
        origin: site.0.clone()?,
    })
}

fn credentials(factors: &[mst_db::two_factor::Factor]) -> Vec<(Uuid, webauthn::Credential)> {
    factors
        .iter()
        .filter(|f| f.kind == "webauthn")
        .filter_map(|f| Some((f.id, serde_json::from_value(f.data.clone()).ok()?)))
        .collect()
}

/// Starts the second step for a user with second factors.
pub async fn begin_login(
    state: &AppState,
    user: &mst_db::users::User,
    factors: &[mst_db::two_factor::Factor],
    site: &Site,
    https: bool,
) -> ApiResult<SecondFactorChallenge> {
    let mut methods: Vec<String> = Vec::new();
    let mut add = |m: &str| {
        if !methods.iter().any(|x| x == m) {
            methods.push(m.to_owned());
        }
    };
    for f in factors {
        match f.kind.as_str() {
            "yubico_otp" if state.yubico.is_none() => {}
            "webauthn" => {}
            k => add(k),
        }
    }
    let rp = relying_party(site, https);
    let here: Vec<webauthn::Credential> = credentials(factors)
        .into_iter()
        .map(|(_, c)| c)
        .filter(|c| rp.as_ref().is_some_and(|rp| rp.id == c.rp_id))
        .collect();
    let challenge = webauthn::challenge();
    let options = match &rp {
        Some(rp) if !here.is_empty() => {
            add("webauthn");
            Some(webauthn::request_options(&rp.id, &challenge, &here))
        }
        _ => None,
    };
    if mst_db::two_factor::backup_codes_left(&state.db, user.id).await? > 0 {
        add("backup_code");
    }
    let token = mst_pki::random_hex(32);
    let data = json!({
        "challenge": webauthn::b64(&challenge),
        "rp_id": rp.as_ref().map(|r| &r.id),
        "origin": rp.as_ref().map(|r| &r.origin),
    });
    mst_db::two_factor::create_challenge(
        &state.db,
        &mst_pki::hash_secret(&token),
        user.id,
        "login",
        &data,
        LOGIN_TTL,
    )
    .await?;
    Ok(SecondFactorChallenge {
        token,
        methods,
        webauthn: options,
    })
}

#[derive(Deserialize, ToSchema)]
pub struct SecondStep {
    /// From the password step.
    token: String,
    /// A code from an authenticator app, a YubiKey's OTP, or a backup code.
    #[serde(default)]
    code: Option<String>,
    /// What `navigator.credentials.get()` gave, as its `toJSON()`.
    #[serde(default)]
    webauthn: Option<webauthn::AuthenticationResponse>,
}

fn wrong_answer() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "unauthenticated",
        "that code or key is not one of yours",
    )
}

/// The second step of a password sign-in.
#[utoipa::path(post, path = "/auth/login/second-factor", tag = "auth", request_body = SecondStep,
    responses((status = 204, description = "Signed in; session cookie set"),
        (status = 401, description = "Wrong, or the sign-in expired (`sign_in_expired`): start again"),
        (status = 429), (status = 503, description = "The Yubico OTP service did not answer")))]
async fn second_step(
    State(state): State<AppState>,
    Extension(Https(https)): Extension<Https>,
    Json(req): Json<SecondStep>,
) -> ApiResult<Response> {
    // Not limited per address like the password step: each second step
    // follows one of those, and gets five answers at most.
    let id_hash = mst_pki::hash_secret(&req.token);
    let Some(challenge) =
        mst_db::two_factor::attempt_challenge(&state.db, &id_hash, "login", LOGIN_ATTEMPTS).await?
    else {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "sign_in_expired",
            "the sign-in expired or had too many wrong answers; start again",
        ));
    };
    let user = mst_db::users::get(&state.db, challenge.user_id).await?;
    let account = super::auth::account_key(user.tenant_id, &user.username);
    // Taken now, given back on success (see `auth::login`).
    if let Err(retry) = state.limits.login_failures.take(&account).await {
        return Ok(crate::ratelimit::too_many(retry));
    }
    let method = match (&req.webauthn, req.code.as_deref()) {
        (Some(resp), _) => check_webauthn(&state, &user, &challenge.data, resp).await?,
        (None, Some(code)) => check_code(&state, &user, code).await?,
        (None, None) => {
            return Err(ApiError::bad_request(
                "give a code or a security key's answer",
            ));
        }
    };
    let Some(method) = method.filter(|_| !user.disabled) else {
        metrics::counter!("mst_logins_total", "method" => "second_factor", "outcome" => "failed")
            .increment(1);
        let entry = mst_db::audit::Entry {
            user_id: Some(user.id),
            username: Some(&user.username),
            tenant_id: Some(user.tenant_id),
            action: "auth.login",
            target: None,
            success: false,
            details: json!({"step": "second_factor"}),
        };
        if let Err(e) = mst_db::audit::record(&state.db, entry).await {
            tracing::error!(error = %e, "writing audit log");
        }
        return Err(wrong_answer());
    };
    state.limits.login_failures.give_back(&account).await;
    mst_db::two_factor::delete_challenge(&state.db, &id_hash).await?;
    let headers =
        super::auth::open_session(&state, &user, &format!("password+{method}"), https).await?;
    Ok((StatusCode::NO_CONTENT, headers).into_response())
}

async fn check_webauthn(
    state: &AppState,
    user: &mst_db::users::User,
    data: &Value,
    resp: &webauthn::AuthenticationResponse,
) -> ApiResult<Option<&'static str>> {
    let (Some(rp_id), Some(origin), Some(challenge)) = (
        data["rp_id"].as_str(),
        data["origin"].as_str(),
        data["challenge"].as_str(),
    ) else {
        return Ok(None);
    };
    let rp = webauthn::Rp {
        id: rp_id.to_owned(),
        origin: origin.to_owned(),
    };
    let challenge = webauthn::unb64(challenge).map_err(ApiError::internal)?;
    let mut tx = state.db.begin().await?;
    let factors = mst_db::two_factor::lock_kind(&mut *tx, user.id, "webauthn").await?;
    let Some((id, mut cred)) = credentials(&factors)
        .into_iter()
        .find(|(_, c)| c.credential_id == resp.raw_id.trim_end_matches('='))
    else {
        return Ok(None);
    };
    match webauthn::authenticate(&rp, &challenge, &cred, resp) {
        Ok(count) => {
            cred.sign_count = count;
            mst_db::two_factor::used(&mut *tx, id, &json!(cred)).await?;
            tx.commit().await?;
            Ok(Some("webauthn"))
        }
        Err(e) => {
            tracing::info!(user = %user.username, error = %format!("{e:#}"), "security key refused");
            Ok(None)
        }
    }
}

async fn check_code(
    state: &AppState,
    user: &mst_db::users::User,
    code: &str,
) -> ApiResult<Option<&'static str>> {
    let code = code.trim();
    match tf::code_kind(code) {
        Some(CodeKind::Totp) => {
            let now = Utc::now().timestamp().max(0) as u64;
            let mut tx = state.db.begin().await?;
            for f in mst_db::two_factor::lock_kind(&mut *tx, user.id, "totp").await? {
                let Some(secret) = open_totp(user.id, &f.data) else {
                    continue;
                };
                let last = f.data["last_step"].as_u64();
                if let Some(step) = totp::check(&secret, code, now, last) {
                    let mut data = f.data.clone();
                    data["last_step"] = step.into();
                    mst_db::two_factor::used(&mut *tx, f.id, &data).await?;
                    tx.commit().await?;
                    return Ok(Some("totp"));
                }
            }
            Ok(None)
        }
        Some(CodeKind::YubicoOtp) => {
            let Some(validator) = &state.yubico else {
                return Ok(None);
            };
            let factors = mst_db::two_factor::factors(&state.db, user.id).await?;
            let Some(f) = factors.iter().find(|f| {
                f.kind == "yubico_otp"
                    && f.data["public_id"].as_str() == Some(yubico::public_id(code))
            }) else {
                return Ok(None);
            };
            match validator.verify(code).await {
                Ok(true) => {
                    mst_db::two_factor::used(&state.db, f.id, &f.data).await?;
                    Ok(Some("yubico_otp"))
                }
                Ok(false) => Ok(None),
                Err(_) => Err(yubico_unavailable()),
            }
        }
        Some(CodeKind::Backup) => {
            let normalized = tf::normalize_backup_code(code).unwrap_or_default();
            let hash = tf::hash_backup_code(user.id, &normalized);
            Ok(
                mst_db::two_factor::use_backup_code(&state.db, user.id, &hash)
                    .await?
                    .then_some("backup_code"),
            )
        }
        None => Ok(None),
    }
}

fn open_totp(user: UserId, data: &Value) -> Option<Vec<u8>> {
    let sealed = data["secret"].as_str()?;
    let secret = crate::secrets::open(&sealed_context(user), sealed).ok()?;
    totp::base32_decode(&secret)
}

fn yubico_unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "yubico_unavailable",
        "the Yubico OTP service did not answer; try again, or use another second factor",
    )
}

// ------------------------------------------------------- managing factors

#[derive(Serialize, ToSchema)]
pub struct FactorView {
    id: Uuid,
    /// `totp`, `webauthn` or `yubico_otp`.
    kind: String,
    name: String,
    /// For a security key: the address it works at.
    site: Option<String>,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
}

impl From<mst_db::two_factor::Factor> for FactorView {
    fn from(f: mst_db::two_factor::Factor) -> Self {
        Self {
            site: f.data["rp_id"].as_str().map(str::to_owned),
            id: f.id,
            kind: f.kind,
            name: f.name,
            created_at: f.created_at,
            last_used_at: f.last_used_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct TwoFactorStatus {
    factors: Vec<FactorView>,
    /// Unused backup codes.
    backup_codes_left: i64,
    /// The user's tenant, or one above it, requires a second factor.
    required: bool,
    /// Whether this installation can check Yubico OTPs.
    yubico_otp: bool,
}

/// The user is the caller, signed in as themselves (not with a scoped
/// token), with a password.
async fn own_local(state: &AppState, p: &Principal, id: UserId) -> ApiResult<mst_db::users::User> {
    if id != p.user_id || p.limited {
        return Err(ApiError::forbidden_msg(
            "only the user can change their own second factors",
        ));
    }
    let user = mst_db::users::get(&state.db, id).await?;
    if user.password_hash.is_none() {
        return Err(ApiError::bad_request(
            "this account signs in through an identity provider, which handles second factors",
        ));
    }
    Ok(user)
}

/// Checks the caller's password again before a change to how they sign
/// in. Wrong guesses count against signing in.
pub(super) async fn confirm_password(
    state: &AppState,
    user: &mst_db::users::User,
    password: &str,
) -> ApiResult<()> {
    let account = super::auth::account_key(user.tenant_id, &user.username);
    if state.limits.login_failures.take(&account).await.is_err() {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "too many wrong passwords; wait a minute",
        ));
    }
    let hash = user.password_hash.clone().unwrap_or_default();
    let password = password.to_owned();
    let ok = tokio::task::spawn_blocking(move || super::auth::verify_password(&password, &hash))
        .await
        .map_err(ApiError::internal)?;
    if !ok {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "wrong_password",
            "that is not your password",
        ));
    }
    state.limits.login_failures.give_back(&account).await;
    Ok(())
}

#[utoipa::path(get, path = "/users/{id}/two-factor", tag = "users", params(("id" = UserId, Path)),
    responses((status = 200, body = TwoFactorStatus), (status = 403), (status = 404)))]
async fn status(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<UserId>,
) -> ApiResult<Json<TwoFactorStatus>> {
    let user = mst_db::users::get(&state.db, id).await?;
    if id != p.user_id {
        authorize_resource(&state.db, &p, Perm::UserRead, user.tenant_id, "user").await?;
    }
    Ok(Json(TwoFactorStatus {
        factors: mst_db::two_factor::factors(&state.db, id)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
        backup_codes_left: mst_db::two_factor::backup_codes_left(&state.db, id).await?,
        required: user.password_hash.is_some()
            && mst_db::two_factor::required(&state.db, user.tenant_id).await?,
        yubico_otp: state.yubico.is_some(),
    }))
}

#[derive(Deserialize, ToSchema)]
pub struct ConfirmPassword {
    password: String,
}

#[derive(Serialize, ToSchema)]
pub struct TotpEnrollment {
    /// Send it back with the first code.
    token: String,
    /// The secret, base32, for typing into the app.
    secret: String,
    /// The `otpauth://` URI, for a QR code.
    uri: String,
}

/// Starts adding an authenticator app: the secret to give it.
#[utoipa::path(post, path = "/users/{id}/two-factor/totp/start", tag = "users",
    params(("id" = UserId, Path)), request_body = ConfirmPassword,
    responses((status = 200, body = TotpEnrollment), (status = 403)))]
async fn start_totp(
    State(state): State<AppState>,
    p: Principal,
    Extension(site): Extension<Site>,
    Path(id): Path<UserId>,
    Json(req): Json<ConfirmPassword>,
) -> ApiResult<Json<TotpEnrollment>> {
    let user = own_local(&state, &p, id).await?;
    confirm_password(&state, &user, &req.password).await?;
    let secret = totp::base32(&totp::new_secret());
    let token = mst_pki::random_hex(32);
    mst_db::two_factor::create_challenge(
        &state.db,
        &mst_pki::hash_secret(&token),
        id,
        "totp",
        &json!({"secret": crate::secrets::seal(&sealed_context(id), &secret)}),
        ENROLL_TTL,
    )
    .await?;
    let issuer = site.host().unwrap_or_else(|| "metastratum".into());
    // Accounts of sub-tenants say which one, as people may have several.
    let lineage = mst_db::tenants::lineage(&state.db, user.tenant_id).await?;
    let slugs: Vec<_> = lineage.iter().skip(1).map(|t| t.slug.as_str()).collect();
    let account = if slugs.is_empty() {
        user.username.clone()
    } else {
        format!("{}@{}", user.username, slugs.join("/"))
    };
    let uri = totp::uri(
        &issuer,
        &account,
        &totp::base32_decode(&secret).unwrap_or_default(),
    );
    Ok(Json(TotpEnrollment { token, secret, uri }))
}

#[derive(Deserialize, ToSchema)]
pub struct AddTotp {
    token: String,
    /// The code the app shows now.
    code: String,
    /// What to call it (`Phone`).
    name: String,
}

/// A factor just added. With the first one come the backup codes, shown
/// this once.
#[derive(Serialize, ToSchema)]
pub struct FactorAdded {
    factor: FactorView,
    backup_codes: Option<Vec<String>>,
}

fn valid_name(name: &str) -> ApiResult<&str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 64 {
        return Err(ApiError::bad_request(
            "give it a name of up to 64 characters",
        ));
    }
    Ok(name)
}

/// Adds a factor, and the first backup codes along with the first one.
/// The user's other sessions end: one opened with the password alone (by
/// someone who learnt it, while there was no factor to ask for) must not
/// outlive the factor's arrival.
async fn add_factor(
    state: &AppState,
    p: &Principal,
    headers: &HeaderMap,
    user: &mst_db::users::User,
    kind: &str,
    name: &str,
    data: &Value,
) -> ApiResult<FactorAdded> {
    let current = super::session_secret(headers).map(|s| mst_pki::hash_secret(&s));
    let mut tx = state.db.begin().await?;
    mst_db::sessions::delete_for_user(&mut *tx, user.id, current.as_deref()).await?;
    let first = !mst_db::two_factor::has_factor(&mut *tx, user.id).await?;
    let factor = mst_db::two_factor::add(&mut *tx, user.id, kind, name, data).await?;
    let backup_codes = if first {
        let codes = tf::new_backup_codes();
        let hashes: Vec<String> = codes
            .iter()
            .filter_map(|c| tf::normalize_backup_code(c))
            .map(|c| tf::hash_backup_code(user.id, &c))
            .collect();
        mst_db::two_factor::set_backup_codes(&mut tx, user.id, &hashes).await?;
        Some(codes)
    } else {
        None
    };
    tx.commit().await?;
    audit(
        &state.db,
        Some(p),
        Some(user.tenant_id),
        "user.two_factor.add",
        Some(("user", user.id.0)),
        json!({"kind": kind, "name": name}),
    )
    .await;
    Ok(FactorAdded {
        factor: factor.into(),
        backup_codes,
    })
}

/// Finishes adding an authenticator app with a code it shows.
#[utoipa::path(post, path = "/users/{id}/two-factor/totp", tag = "users",
    params(("id" = UserId, Path)), request_body = AddTotp,
    responses((status = 201, body = FactorAdded), (status = 400), (status = 403)))]
async fn add_totp(
    State(state): State<AppState>,
    p: Principal,
    headers: HeaderMap,
    Path(id): Path<UserId>,
    Json(req): Json<AddTotp>,
) -> ApiResult<(StatusCode, Json<FactorAdded>)> {
    let user = own_local(&state, &p, id).await?;
    let name = valid_name(&req.name)?;
    let id_hash = mst_pki::hash_secret(&req.token);
    let challenge =
        mst_db::two_factor::attempt_challenge(&state.db, &id_hash, "totp", ENROLL_ATTEMPTS)
            .await?
            .filter(|c| c.user_id == id)
            .ok_or_else(|| ApiError::bad_request("this took too long; start again"))?;
    let secret = open_totp(id, &challenge.data).ok_or_else(|| ApiError::internal("bad secret"))?;
    let now = Utc::now().timestamp().max(0) as u64;
    let step = totp::check(&secret, &req.code, now, None).ok_or_else(|| {
        ApiError::bad_request(
            "that code is not right; check the phone's clock, and try the next code",
        )
    })?;
    mst_db::two_factor::delete_challenge(&state.db, &id_hash).await?;
    let mut data = challenge.data;
    data["last_step"] = step.into();
    let added = add_factor(&state, &p, &headers, &user, "totp", name, &data).await?;
    Ok((StatusCode::CREATED, Json(added)))
}

#[derive(Serialize, ToSchema)]
pub struct WebauthnEnrollment {
    /// Send it back with the key's answer.
    token: String,
    /// Options for `navigator.credentials.create()`.
    #[schema(value_type = Object)]
    options: Value,
}

/// Starts adding a security key: what to ask the browser for.
#[utoipa::path(post, path = "/users/{id}/two-factor/webauthn/start", tag = "users",
    params(("id" = UserId, Path)), request_body = ConfirmPassword,
    responses((status = 200, body = WebauthnEnrollment), (status = 400), (status = 403)))]
async fn start_webauthn(
    State(state): State<AppState>,
    p: Principal,
    Extension(site): Extension<Site>,
    Extension(Https(https)): Extension<Https>,
    Path(id): Path<UserId>,
    Json(req): Json<ConfirmPassword>,
) -> ApiResult<Json<WebauthnEnrollment>> {
    let user = own_local(&state, &p, id).await?;
    let rp = relying_party(&site, https).ok_or_else(|| {
        ApiError::bad_request(
            "browsers use security keys only at a domain name over HTTPS (or at localhost)",
        )
    })?;
    confirm_password(&state, &user, &req.password).await?;
    let factors = mst_db::two_factor::factors(&state.db, id).await?;
    let existing: Vec<_> = credentials(&factors)
        .into_iter()
        .map(|(_, c)| c)
        .filter(|c| c.rp_id == rp.id)
        .collect();
    let challenge = webauthn::challenge();
    let display = if user.display_name.is_empty() {
        &user.username
    } else {
        &user.display_name
    };
    let options = webauthn::creation_options(
        &rp,
        &rp.id,
        user.id.0.as_bytes(),
        &user.username,
        display,
        &challenge,
        &existing,
    );
    let token = mst_pki::random_hex(32);
    mst_db::two_factor::create_challenge(
        &state.db,
        &mst_pki::hash_secret(&token),
        id,
        "webauthn",
        &json!({"challenge": webauthn::b64(&challenge), "rp_id": rp.id, "origin": rp.origin}),
        ENROLL_TTL,
    )
    .await?;
    Ok(Json(WebauthnEnrollment { token, options }))
}

#[derive(Deserialize, ToSchema)]
pub struct AddWebauthn {
    token: String,
    /// What `navigator.credentials.create()` gave, as its `toJSON()`.
    credential: webauthn::RegistrationResponse,
    /// What to call it (`Blue key`).
    name: String,
}

/// Finishes adding a security key with its answer.
#[utoipa::path(post, path = "/users/{id}/two-factor/webauthn", tag = "users",
    params(("id" = UserId, Path)), request_body = AddWebauthn,
    responses((status = 201, body = FactorAdded), (status = 400), (status = 403)))]
async fn add_webauthn(
    State(state): State<AppState>,
    p: Principal,
    headers: HeaderMap,
    Path(id): Path<UserId>,
    Json(req): Json<AddWebauthn>,
) -> ApiResult<(StatusCode, Json<FactorAdded>)> {
    let user = own_local(&state, &p, id).await?;
    let name = valid_name(&req.name)?;
    let id_hash = mst_pki::hash_secret(&req.token);
    let challenge =
        mst_db::two_factor::attempt_challenge(&state.db, &id_hash, "webauthn", ENROLL_ATTEMPTS)
            .await?
            .filter(|c| c.user_id == id)
            .ok_or_else(|| ApiError::bad_request("this took too long; start again"))?;
    let d = &challenge.data;
    let rp = webauthn::Rp {
        id: d["rp_id"].as_str().unwrap_or_default().to_owned(),
        origin: d["origin"].as_str().unwrap_or_default().to_owned(),
    };
    let expected =
        webauthn::unb64(d["challenge"].as_str().unwrap_or_default()).map_err(ApiError::internal)?;
    let cred = webauthn::register(&rp, &expected, &req.credential)
        .map_err(|e| ApiError::bad_request(format!("the key's answer was refused: {e:#}")))?;
    let factors = mst_db::two_factor::factors(&state.db, id).await?;
    if credentials(&factors)
        .iter()
        .any(|(_, c)| c.credential_id == cred.credential_id)
    {
        return Err(ApiError::conflict("that key is already registered"));
    }
    mst_db::two_factor::delete_challenge(&state.db, &id_hash).await?;
    let added = add_factor(&state, &p, &headers, &user, "webauthn", name, &json!(cred)).await?;
    Ok((StatusCode::CREATED, Json(added)))
}

#[derive(Deserialize, ToSchema)]
pub struct AddYubicoOtp {
    password: String,
    /// An OTP the YubiKey typed.
    otp: String,
    /// What to call it (`Keyring YubiKey`).
    name: String,
}

/// Adds a YubiKey by an OTP it typed.
#[utoipa::path(post, path = "/users/{id}/two-factor/yubico-otp", tag = "users",
    params(("id" = UserId, Path)), request_body = AddYubicoOtp,
    responses((status = 201, body = FactorAdded), (status = 400), (status = 403),
        (status = 503, description = "The Yubico OTP service did not answer")))]
async fn add_yubico_otp(
    State(state): State<AppState>,
    p: Principal,
    headers: HeaderMap,
    Path(id): Path<UserId>,
    Json(req): Json<AddYubicoOtp>,
) -> ApiResult<(StatusCode, Json<FactorAdded>)> {
    let user = own_local(&state, &p, id).await?;
    let name = valid_name(&req.name)?;
    let Some(validator) = &state.yubico else {
        return Err(ApiError::bad_request(
            "this installation has no Yubico OTP service configured",
        ));
    };
    confirm_password(&state, &user, &req.password).await?;
    let otp = req.otp.trim();
    if !yubico::looks_like(otp) {
        return Err(ApiError::bad_request(
            "that is not a YubiKey OTP; touch the key with the cursor in the field",
        ));
    }
    let public_id = yubico::public_id(otp);
    let factors = mst_db::two_factor::factors(&state.db, id).await?;
    if factors
        .iter()
        .any(|f| f.kind == "yubico_otp" && f.data["public_id"].as_str() == Some(public_id))
    {
        return Err(ApiError::conflict("that YubiKey is already registered"));
    }
    match validator.verify(otp).await {
        Ok(true) => {}
        Ok(false) => {
            return Err(ApiError::bad_request(
                "the Yubico service refused that OTP; touch the key again",
            ));
        }
        Err(_) => return Err(yubico_unavailable()),
    }
    let added = add_factor(
        &state,
        &p,
        &headers,
        &user,
        "yubico_otp",
        name,
        &json!({"public_id": public_id}),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(added)))
}

#[derive(Serialize, ToSchema)]
pub struct BackupCodes {
    /// Each works once. Shown this once.
    codes: Vec<String>,
}

/// Replaces the backup codes with new ones.
#[utoipa::path(post, path = "/users/{id}/two-factor/backup-codes", tag = "users",
    params(("id" = UserId, Path)), request_body = ConfirmPassword,
    responses((status = 200, body = BackupCodes), (status = 400), (status = 403)))]
async fn new_backup_codes(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<UserId>,
    Json(req): Json<ConfirmPassword>,
) -> ApiResult<Json<BackupCodes>> {
    let user = own_local(&state, &p, id).await?;
    confirm_password(&state, &user, &req.password).await?;
    if !mst_db::two_factor::has_factor(&state.db, id).await? {
        return Err(ApiError::bad_request("add a second factor first"));
    }
    let codes = tf::new_backup_codes();
    let hashes: Vec<String> = codes
        .iter()
        .filter_map(|c| tf::normalize_backup_code(c))
        .map(|c| tf::hash_backup_code(id, &c))
        .collect();
    let mut conn = state.db.acquire().await?;
    mst_db::two_factor::set_backup_codes(&mut conn, id, &hashes).await?;
    audit(
        &state.db,
        Some(&p),
        Some(user.tenant_id),
        "user.two_factor.backup_codes",
        Some(("user", id.0)),
        json!({}),
    )
    .await;
    Ok(Json(BackupCodes { codes }))
}

#[derive(Deserialize, ToSchema, Default)]
pub struct RemoveFactor {
    /// The caller's password, when removing their own.
    #[serde(default)]
    password: Option<String>,
}

/// Removes a second factor: one's own (with one's password), or, for an
/// administrator, someone else's. With the last one go the backup codes.
#[utoipa::path(delete, path = "/users/{id}/two-factor/factors/{factor_id}", tag = "users",
    params(("id" = UserId, Path), ("factor_id" = Uuid, Path)), request_body = RemoveFactor,
    responses((status = 204), (status = 403), (status = 404)))]
async fn remove_factor(
    State(state): State<AppState>,
    p: Principal,
    Path((id, factor_id)): Path<(UserId, Uuid)>,
    body: Option<Json<RemoveFactor>>,
) -> ApiResult<StatusCode> {
    let req = body.map(|b| b.0).unwrap_or_default();
    let user = mst_db::users::get(&state.db, id).await?;
    if id == p.user_id && !p.limited {
        let password = req
            .password
            .ok_or_else(|| ApiError::bad_request("give your password"))?;
        confirm_password(&state, &user, &password).await?;
    } else {
        authorize_resource(&state.db, &p, Perm::UserWrite, user.tenant_id, "user").await?;
    }
    let mut tx = state.db.begin().await?;
    if !mst_db::two_factor::remove(&mut *tx, id, factor_id).await? {
        return Err(ApiError::not_found("second factor"));
    }
    if !mst_db::two_factor::has_factor(&mut *tx, id).await? {
        mst_db::two_factor::reset(&mut tx, id).await?;
    }
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(user.tenant_id),
        "user.two_factor.remove",
        Some(("user", id.0)),
        json!({"factor": factor_id}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// Removes all of a user's second factors and backup codes, for someone
/// who lost them, and signs them out everywhere. For administrators.
#[utoipa::path(delete, path = "/users/{id}/two-factor", tag = "users", params(("id" = UserId, Path)),
    responses((status = 204), (status = 400), (status = 403), (status = 404)))]
async fn reset(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<UserId>,
) -> ApiResult<StatusCode> {
    let user = mst_db::users::get(&state.db, id).await?;
    if id == p.user_id {
        return Err(ApiError::bad_request(
            "remove your own second factors one by one",
        ));
    }
    authorize_resource(&state.db, &p, Perm::UserWrite, user.tenant_id, "user").await?;
    let mut tx = state.db.begin().await?;
    let n = mst_db::two_factor::reset(&mut tx, id).await?;
    mst_db::sessions::delete_for_user(&mut *tx, id, None).await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(user.tenant_id),
        "user.two_factor.reset",
        Some(("user", id.0)),
        json!({"username": user.username, "factors": n}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

// ----------------------------------------------------------------- policy

#[derive(Serialize, ToSchema)]
pub struct TenantRef {
    id: TenantId,
    name: String,
}

#[derive(Serialize, ToSchema)]
pub struct TwoFactorPolicy {
    /// The tenant requires a second factor of its password accounts, and of
    /// those of its sub-tenants.
    required: bool,
    /// The nearest tenant above that requires it (so this tenant does,
    /// whatever its own setting).
    required_by: Option<TenantRef>,
}

#[utoipa::path(get, path = "/tenants/{id}/two-factor-policy", tag = "tenants", params(("id" = TenantId, Path)),
    responses((status = 200, body = TwoFactorPolicy), (status = 403), (status = 404)))]
async fn get_policy(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TenantId>,
) -> ApiResult<Json<TwoFactorPolicy>> {
    authorize(&state.db, &p, Perm::UserRead, id).await?;
    policy_view(&state, id).await.map(Json)
}

async fn policy_view(state: &AppState, id: TenantId) -> ApiResult<TwoFactorPolicy> {
    let (required, above) = mst_db::two_factor::policy(&state.db, id).await?;
    Ok(TwoFactorPolicy {
        required,
        required_by: above.map(|(id, name)| TenantRef { id, name }),
    })
}

#[derive(Deserialize, ToSchema)]
pub struct SetTwoFactorPolicy {
    required: bool,
}

/// Requires (or stops requiring) a second factor of the tenant's password
/// accounts and its sub-tenants'. Those without one can then only add one,
/// until they do.
#[utoipa::path(put, path = "/tenants/{id}/two-factor-policy", tag = "tenants", params(("id" = TenantId, Path)),
    request_body = SetTwoFactorPolicy,
    responses((status = 200, body = TwoFactorPolicy), (status = 403), (status = 404)))]
async fn set_policy(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TenantId>,
    Json(req): Json<SetTwoFactorPolicy>,
) -> ApiResult<Json<TwoFactorPolicy>> {
    authorize(&state.db, &p, Perm::UserWrite, id).await?;
    mst_db::tenants::get(&state.db, id).await?;
    mst_db::two_factor::set_required(&state.db, id, req.required).await?;
    audit(
        &state.db,
        Some(&p),
        Some(id),
        "tenant.two_factor_policy",
        Some(("tenant", id.0)),
        json!({"required": req.required}),
    )
    .await;
    policy_view(&state, id).await.map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrollment_reaches_only_its_own() {
        let me = UserId::new();
        assert!(enrollment_path("/api/v1/auth/me", me));
        assert!(enrollment_path(
            &format!("/api/v1/users/{me}/two-factor"),
            me
        ));
        assert!(enrollment_path(
            &format!("/api/v1/users/{me}/two-factor/totp/start"),
            me
        ));
        assert!(enrollment_path(
            &format!("/users/{me}/two-factor/webauthn"),
            me
        ));
        assert!(!enrollment_path(
            &format!("/api/v1/users/{}/two-factor", UserId::new()),
            me
        ));
        assert!(!enrollment_path(
            &format!("/api/v1/users/{me}/two-factorx"),
            me
        ));
        assert!(!enrollment_path(&format!("/api/v1/users/{me}"), me));
        assert!(!enrollment_path("/api/v1/vms", me));
        assert!(!enrollment_path("/api/v1/tokens", me));
    }

    #[test]
    fn relying_party_needs_a_name_and_https() {
        let rp = |url: &str, https| relying_party(&Site(Some(url.into())), https).map(|r| r.id);
        assert_eq!(
            rp("https://mst.example.com", true).as_deref(),
            Some("mst.example.com")
        );
        assert_eq!(
            rp("http://localhost:18080", false).as_deref(),
            Some("localhost")
        );
        assert_eq!(rp("http://mst.example.com", false), None);
        assert_eq!(rp("https://10.0.0.10:8443", true), None);
        assert_eq!(rp("https://[::1]", true), None);
    }
}
