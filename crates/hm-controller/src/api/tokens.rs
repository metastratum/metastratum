//! API tokens: long-lived credentials for automation, acting as their user.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use super::TOKEN_PREFIX;
use crate::authz::{Principal, audit};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_tokens, create_token))
        .routes(routes!(delete_token))
}

#[derive(Serialize, ToSchema)]
pub struct TokenView {
    id: Uuid,
    name: String,
    created_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    last_used_at: Option<DateTime<Utc>>,
}

impl From<hm_db::rbac::ApiToken> for TokenView {
    fn from(t: hm_db::rbac::ApiToken) -> Self {
        Self {
            id: t.id,
            name: t.name,
            created_at: t.created_at,
            expires_at: t.expires_at,
            last_used_at: t.last_used_at,
        }
    }
}

/// The caller's own tokens.
#[utoipa::path(get, path = "/tokens", tag = "auth", responses((status = 200, body = Vec<TokenView>)))]
async fn list_tokens(
    State(state): State<AppState>,
    p: Principal,
) -> ApiResult<Json<Vec<TokenView>>> {
    Ok(Json(
        hm_db::rbac::tokens_of(&state.db, p.user_id)
            .await?
            .into_iter()
            .map(Into::into)
            .collect(),
    ))
}

#[derive(Deserialize, ToSchema)]
pub struct CreateToken {
    name: String,
    /// Days until it expires; omit for a token that does not expire.
    expires_in_days: Option<u32>,
}

#[derive(Serialize, ToSchema)]
pub struct NewToken {
    /// Send as `Authorization: Bearer <token>`. Shown only once.
    token: String,
    #[serde(flatten)]
    info: TokenView,
}

#[utoipa::path(post, path = "/tokens", tag = "auth", request_body = CreateToken,
    responses((status = 201, body = NewToken), (status = 400)))]
async fn create_token(
    State(state): State<AppState>,
    p: Principal,
    Json(req): Json<CreateToken>,
) -> ApiResult<(StatusCode, Json<NewToken>)> {
    let name = req.name.trim();
    if name.is_empty() || name.len() > 128 {
        return Err(ApiError::bad_request("name must be 1-128 characters"));
    }
    let expires_at = req
        .expires_in_days
        .map(|d| Utc::now() + chrono::Duration::days(i64::from(d.clamp(1, 3650))));
    let token = format!("{TOKEN_PREFIX}{}", hm_pki::random_hex(32));
    let row = hm_db::rbac::create_token(
        &state.db,
        p.user_id,
        name,
        &hm_pki::hash_secret(&token),
        expires_at,
    )
    .await?;
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "token.create",
        Some(("token", row.id)),
        json!({"name": name}),
    )
    .await;
    Ok((
        StatusCode::CREATED,
        Json(NewToken {
            token,
            info: row.into(),
        }),
    ))
}

#[utoipa::path(delete, path = "/tokens/{id}", tag = "auth", params(("id" = Uuid, Path)), responses((status = 204), (status = 404)))]
async fn delete_token(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    if !hm_db::rbac::delete_token(&state.db, id, p.user_id).await? {
        return Err(ApiError::not_found("token"));
    }
    audit(
        &state.db,
        Some(&p),
        Some(p.tenant_id),
        "token.delete",
        Some(("token", id)),
        json!({}),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}
