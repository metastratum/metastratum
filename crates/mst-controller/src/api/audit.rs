use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Utc};
use mst_core::ids::{TenantId, UserId};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::authz::{Perm, Principal, authorize};
use crate::error::ApiResult;
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(list_audit))
}

#[derive(Serialize, ToSchema)]
pub struct AuditEntry {
    id: i64,
    at: DateTime<Utc>,
    user_id: Option<UserId>,
    username: Option<String>,
    tenant_id: Option<TenantId>,
    /// E.g. `vm.create`, `role.bind`, `auth.login`.
    action: String,
    target_kind: Option<String>,
    target_id: Option<Uuid>,
    /// `success` or `failure`.
    outcome: String,
    details: serde_json::Value,
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AuditQuery {
    /// Defaults to the caller's tenant; includes sub-tenants.
    tenant_id: Option<TenantId>,
    /// Entries older than this ID (for paging).
    before: Option<i64>,
    /// Default 100, max 500.
    limit: Option<i64>,
}

#[utoipa::path(get, path = "/audit", tag = "audit", params(AuditQuery), responses((status = 200, body = Vec<AuditEntry>), (status = 403)))]
async fn list_audit(
    State(state): State<AppState>,
    p: Principal,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Json<Vec<AuditEntry>>> {
    let tenant = q.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::AuditRead, tenant).await?;
    let rows = mst_db::audit::list(
        &state.db,
        tenant,
        q.before,
        q.limit.unwrap_or(100).clamp(1, 500),
    )
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| AuditEntry {
                id: r.id,
                at: r.at,
                user_id: r.user_id,
                username: r.username,
                tenant_id: r.tenant_id,
                action: r.action,
                target_kind: r.target_kind,
                target_id: r.target_id,
                outcome: r.outcome,
                details: r.details,
            })
            .collect(),
    ))
}
