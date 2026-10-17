//! Users and the roles they hold.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use mst_core::ids::{TenantId, UserId};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use super::auth::hash_password;
use crate::authz::{Perm, Principal, Role, audit, authorize, authorize_resource};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_users, create_user))
        .routes(routes!(update_user, delete_user))
        .routes(routes!(user_bindings))
        .routes(routes!(tenant_bindings))
        .routes(routes!(create_binding))
        .routes(routes!(delete_binding))
}

pub const MIN_PASSWORD: usize = 10;

#[derive(Serialize, ToSchema)]
pub struct UserView {
    id: UserId,
    tenant_id: TenantId,
    username: String,
    display_name: String,
    email: Option<String>,
    disabled: bool,
    /// Signs in with a password (rather than through an identity provider).
    local: bool,
    /// Signs in with a second factor too.
    two_factor: bool,
    created_at: DateTime<Utc>,
}

impl From<mst_db::users::User> for UserView {
    fn from(u: mst_db::users::User) -> Self {
        Self {
            id: u.id,
            tenant_id: u.tenant_id,
            local: u.password_hash.is_some(),
            two_factor: false,
            username: u.username,
            display_name: u.display_name,
            email: u.email,
            disabled: u.disabled,
            created_at: u.created_at,
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct BindingView {
    id: Uuid,
    user_id: UserId,
    username: String,
    tenant_id: TenantId,
    tenant_name: String,
    #[schema(value_type = String)]
    role: String,
    /// `manual`, or `oidc` when the identity provider's groups gave it (it
    /// is then re-synced at each sign-in).
    source: String,
    created_at: DateTime<Utc>,
}

impl From<mst_db::rbac::RoleBinding> for BindingView {
    fn from(b: mst_db::rbac::RoleBinding) -> Self {
        Self {
            id: b.id,
            user_id: b.user_id,
            username: b.username,
            tenant_id: b.tenant_id,
            tenant_name: b.tenant_name,
            role: b.role,
            source: b.source,
            created_at: b.created_at,
        }
    }
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ListQuery {
    /// Defaults to the caller's tenant.
    tenant_id: Option<TenantId>,
    /// Include users of all sub-tenants.
    #[serde(default)]
    descendants: bool,
}

#[utoipa::path(get, path = "/users", tag = "users", params(ListQuery), responses((status = 200, body = Vec<UserView>)))]
async fn list_users(
    State(state): State<AppState>,
    p: Principal,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<UserView>>> {
    let tenant = q.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::UserRead, tenant).await?;
    let users = mst_db::users::list(&state.db, tenant, q.descendants).await?;
    let ids: Vec<UserId> = users.iter().map(|u| u.id).collect();
    let with = mst_db::two_factor::with_factor(&state.db, &ids).await?;
    Ok(Json(
        users
            .into_iter()
            .map(|u| {
                let two_factor = with.contains(&u.id);
                UserView {
                    two_factor,
                    ..u.into()
                }
            })
            .collect(),
    ))
}

fn check_password(pw: &str) -> ApiResult<()> {
    if pw.chars().count() < MIN_PASSWORD {
        return Err(ApiError::bad_request(format!(
            "passwords need at least {MIN_PASSWORD} characters"
        )));
    }
    Ok(())
}

async fn hash(pw: String) -> ApiResult<String> {
    tokio::task::spawn_blocking(move || hash_password(&pw))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)
}

#[derive(Deserialize, ToSchema)]
pub struct CreateUser {
    tenant_id: TenantId,
    username: String,
    #[serde(default)]
    display_name: String,
    email: Option<String>,
    password: String,
    /// Role to hold at `tenant_id` (default `viewer`).
    #[schema(value_type = Option<String>)]
    role: Option<Role>,
}

#[utoipa::path(post, path = "/users", tag = "users", request_body = CreateUser,
    responses((status = 201, body = UserView), (status = 400), (status = 403), (status = 409)))]
async fn create_user(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<CreateUser>,
) -> ApiResult<(StatusCode, Json<UserView>)> {
    authorize(&state.db, &p, Perm::UserWrite, req.tenant_id).await?;
    check_password(&req.password)?;
    let role = req.role.unwrap_or(Role::Viewer);
    let password_hash = hash(req.password).await?;
    let mut tx = state.db.begin().await?;
    let user = mst_db::users::create(
        &mut *tx,
        mst_db::users::NewUser {
            tenant_id: req.tenant_id,
            username: req.username.trim(),
            display_name: req.display_name.trim(),
            email: req
                .email
                .as_deref()
                .map(str::trim)
                .filter(|e| !e.is_empty()),
            password_hash: Some(&password_hash),
        },
    )
    .await
    .map_err(|e| match e {
        mst_db::Error::Sqlx(mst_db::sqlx::Error::Database(d)) if d.is_check_violation() => {
            ApiError::bad_request(
                "usernames are 1-128 letters, digits, dots, hyphens, underscores or @",
            )
        }
        e => e.into(),
    })?;
    mst_db::rbac::create_binding(
        &mut *tx,
        user.id,
        req.tenant_id,
        role.as_str(),
        Some(p.user_id),
    )
    .await?;
    tx.commit().await?;
    audit(
        &state.db,
        Some(&p),
        Some(user.tenant_id),
        "user.create",
        Some(("user", user.id.0)),
        json!({"username": user.username, "role": role}),
    )
    .await;
    Ok((StatusCode::CREATED, Json(user.into())))
}

/// Users may change their own name, email and password; administrators may
/// change anyone's and disable accounts.
#[derive(Deserialize, ToSchema)]
pub struct UpdateUser {
    display_name: Option<String>,
    /// `null` clears it.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(value_type = Option<String>)]
    email: Option<Option<String>>,
    password: Option<String>,
    /// The password being replaced: needed to change one's own.
    current_password: Option<String>,
    disabled: Option<bool>,
}

fn double_option<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Option<String>>, D::Error> {
    Ok(Some(Option::deserialize(d)?))
}

/// Refuses changes that would leave the provider without an administrator.
async fn keep_a_root_admin(state: &AppState, losing: UserId) -> ApiResult<()> {
    let root = mst_db::tenants::root(&state.db)
        .await?
        .ok_or_else(|| ApiError::internal("no root tenant"))?;
    let is_root_admin = mst_db::rbac::bindings_of(&state.db, losing)
        .await?
        .iter()
        .any(|b| b.tenant_id == root.id && b.role == "admin");
    if is_root_admin && mst_db::rbac::root_admin_count(&state.db).await? <= 1 {
        return Err(ApiError::conflict(
            "this is the last provider administrator",
        ));
    }
    Ok(())
}

#[utoipa::path(patch, path = "/users/{id}", tag = "users", params(("id" = UserId, Path)), request_body = UpdateUser,
    responses((status = 200, body = UserView), (status = 400), (status = 403), (status = 404)))]
async fn update_user(
    State(state): State<AppState>,
    p: Principal,
    headers: axum::http::HeaderMap,
    Path(id): Path<UserId>,
    Json(req): Json<UpdateUser>,
) -> ApiResult<Json<UserView>> {
    let user = mst_db::users::get(&state.db, id).await?;
    // A scoped token acts within its scopes only, not as its user.
    let is_self = id == p.user_id && !p.limited;
    if !is_self || req.disabled.is_some() {
        authorize_resource(&state.db, &p, Perm::UserWrite, user.tenant_id, "user").await?;
    }
    if req.disabled == Some(true) {
        if is_self {
            return Err(ApiError::bad_request("you cannot disable your own account"));
        }
        keep_a_root_admin(&state, id).await?;
    }
    let password_hash = match req.password {
        Some(pw) => {
            check_password(&pw)?;
            if is_self && user.password_hash.is_some() {
                let current = req
                    .current_password
                    .as_deref()
                    .ok_or_else(|| ApiError::bad_request("give your current password too"))?;
                super::two_factor::confirm_password(&state, &user, current).await?;
            }
            Some(hash(pw).await?)
        }
        None => None,
    };
    let email = req
        .email
        .map(|e| e.map(|e| e.trim().to_owned()).filter(|e| !e.is_empty()));
    mst_db::users::update(
        &state.db,
        id,
        mst_db::users::Changes {
            display_name: req.display_name.as_deref().map(str::trim),
            email: email.as_ref().map(|e| e.as_deref()),
            password_hash: password_hash.as_deref(),
            disabled: req.disabled,
        },
    )
    .await?;
    if password_hash.is_some() {
        // A new password signs the account out everywhere else.
        let current = super::session_secret(&headers)
            .filter(|_| is_self)
            .map(|s| mst_pki::hash_secret(&s));
        mst_db::sessions::delete_for_user(&state.db, id, current.as_deref()).await?;
    }
    let changed: Vec<&str> = [
        ("display_name", req.display_name.is_some()),
        ("email", email.is_some()),
        ("password", password_hash.is_some()),
        ("disabled", req.disabled.is_some()),
    ]
    .into_iter()
    .filter_map(|(k, v)| v.then_some(k))
    .collect();
    audit(
        &state.db,
        Some(&p),
        Some(user.tenant_id),
        "user.update",
        Some(("user", id.0)),
        json!({"changed": changed, "disabled": req.disabled}),
    )
    .await;
    Ok(Json(UserView {
        two_factor: mst_db::two_factor::has_factor(&state.db, id).await?,
        ..mst_db::users::get(&state.db, id).await?.into()
    }))
}

#[utoipa::path(delete, path = "/users/{id}", tag = "users", params(("id" = UserId, Path)),
    responses((status = 204), (status = 403), (status = 404), (status = 409)))]
async fn delete_user(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<UserId>,
) -> ApiResult<StatusCode> {
    let user = mst_db::users::get(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::UserWrite, user.tenant_id, "user").await?;
    if id == p.user_id {
        return Err(ApiError::bad_request("you cannot delete your own account"));
    }
    keep_a_root_admin(&state, id).await?;
    mst_db::users::delete(&state.db, id).await?;
    audit(
        &state.db,
        Some(&p),
        Some(user.tenant_id),
        "user.delete",
        Some(("user", id.0)),
        json!({"username": user.username}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(get, path = "/users/{id}/role-bindings", tag = "users", params(("id" = UserId, Path)),
    responses((status = 200, body = Vec<BindingView>), (status = 404)))]
async fn user_bindings(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<UserId>,
) -> ApiResult<Json<Vec<BindingView>>> {
    let user = mst_db::users::get(&state.db, id).await?;
    if id != p.user_id {
        authorize_resource(&state.db, &p, Perm::UserRead, user.tenant_id, "user").await?;
    }
    Ok(Json(
        mst_db::rbac::bindings_of(&state.db, id)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

#[utoipa::path(get, path = "/tenants/{id}/role-bindings", tag = "users", params(("id" = TenantId, Path)),
    responses((status = 200, body = Vec<BindingView>), (status = 404)))]
async fn tenant_bindings(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TenantId>,
) -> ApiResult<Json<Vec<BindingView>>> {
    authorize_resource(&state.db, &p, Perm::UserRead, id, "tenant").await?;
    Ok(Json(
        mst_db::rbac::bindings_at(&state.db, id)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

#[derive(Deserialize, ToSchema)]
pub struct CreateBinding {
    user_id: UserId,
    tenant_id: TenantId,
    #[schema(value_type = String)]
    role: Role,
}

/// Gives a user a role at a tenant (and so its sub-tenants). Requires
/// managing users at that tenant and being able to see the user.
#[utoipa::path(post, path = "/role-bindings", tag = "users", request_body = CreateBinding,
    responses((status = 201, body = BindingView), (status = 403), (status = 404), (status = 409)))]
async fn create_binding(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<CreateBinding>,
) -> ApiResult<(StatusCode, Json<BindingView>)> {
    authorize(&state.db, &p, Perm::UserWrite, req.tenant_id).await?;
    let user = mst_db::users::get(&state.db, req.user_id).await?;
    authorize_resource(&state.db, &p, Perm::UserRead, user.tenant_id, "user").await?;
    let id = mst_db::rbac::create_binding(
        &state.db,
        req.user_id,
        req.tenant_id,
        req.role.as_str(),
        Some(p.user_id),
    )
    .await?;
    audit(
        &state.db,
        Some(&p),
        Some(req.tenant_id),
        "role.bind",
        Some(("user", req.user_id.0)),
        json!({"username": user.username, "role": req.role}),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(mst_db::rbac::get_binding(&state.db, id).await?.into()),
    ))
}

#[utoipa::path(delete, path = "/role-bindings/{id}", tag = "users", params(("id" = Uuid, Path)),
    responses((status = 204), (status = 403), (status = 404), (status = 409)))]
async fn delete_binding(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let b = mst_db::rbac::get_binding(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::UserWrite, b.tenant_id, "role binding").await?;
    let root = mst_db::tenants::root(&state.db)
        .await?
        .ok_or_else(|| ApiError::internal("no root tenant"))?;
    if b.tenant_id == root.id
        && b.role == "admin"
        && mst_db::rbac::root_admin_count(&state.db).await? <= 1
    {
        return Err(ApiError::conflict(
            "this is the last provider administrator",
        ));
    }
    mst_db::rbac::delete_binding(&state.db, id).await?;
    audit(
        &state.db,
        Some(&p),
        Some(b.tenant_id),
        "role.unbind",
        Some(("user", b.user_id.0)),
        json!({"username": b.username, "role": b.role}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}
