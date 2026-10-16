use axum::Json;
use axum::extract::{Path, Query, State};
use chrono::{DateTime, Utc};
use hm_core::ids::{TaskId, TenantId};
use hm_core::task::TaskState;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::authz::{Perm, Principal, authorize, authorize_resource};
use crate::error::ApiResult;
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_tasks))
        .routes(routes!(get_task))
}

#[derive(Serialize, ToSchema)]
pub struct TaskView {
    id: TaskId,
    tenant_id: TenantId,
    /// E.g. `vm.create`.
    kind: String,
    target_kind: Option<String>,
    target_id: Option<Uuid>,
    state: TaskState,
    step: Option<String>,
    progress: Option<f32>,
    error: Option<String>,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
}

impl From<hm_db::tasks::Task> for TaskView {
    fn from(t: hm_db::tasks::Task) -> Self {
        Self {
            id: t.id,
            tenant_id: t.tenant_id,
            kind: t.kind,
            target_kind: t.target_kind,
            target_id: t.target_id,
            state: t.state,
            step: t.step,
            progress: t.progress,
            error: t.error,
            created_at: t.created_at,
            started_at: t.started_at,
            finished_at: t.finished_at,
        }
    }
}

#[derive(Deserialize, IntoParams)]
pub struct ListQuery {
    /// Defaults to the caller's tenant; includes descendants.
    tenant_id: Option<TenantId>,
    target_id: Option<Uuid>,
    /// Default 50, max 500.
    limit: Option<i64>,
}

#[utoipa::path(get, path = "/tasks", tag = "tasks", params(ListQuery),
    responses((status = 200, body = Vec<TaskView>)))]
async fn list_tasks(
    State(state): State<AppState>,
    p: Principal,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<TaskView>>> {
    let tenant = q.tenant_id.unwrap_or(p.tenant_id);
    authorize(&state.db, &p, Perm::TaskRead, tenant).await?;
    let tasks = hm_db::tasks::list(
        &state.db,
        tenant,
        q.target_id,
        q.limit.unwrap_or(50).clamp(1, 500),
    )
    .await?;
    Ok(Json(tasks.into_iter().map(Into::into).collect()))
}

#[utoipa::path(get, path = "/tasks/{id}", tag = "tasks", params(("id" = TaskId, Path)),
    responses((status = 200, body = TaskView), (status = 404)))]
async fn get_task(
    State(state): State<AppState>,
    p: Principal,
    Path(id): Path<TaskId>,
) -> ApiResult<Json<TaskView>> {
    let task = hm_db::tasks::get(&state.db, id).await?;
    authorize_resource(&state.db, &p, Perm::TaskRead, task.tenant_id, "task").await?;
    Ok(Json(task.into()))
}
