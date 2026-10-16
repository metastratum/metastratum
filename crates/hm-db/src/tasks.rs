use std::time::Duration;

use chrono::{DateTime, Utc};
use hm_core::ids::{TaskId, TenantId, UserId};
use hm_core::task::TaskState;
use serde_json::Value;
use sqlx::PgExecutor;
use uuid::Uuid;

use crate::Result;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Task {
    pub id: TaskId,
    pub tenant_id: TenantId,
    pub kind: String,
    pub target_kind: Option<String>,
    pub target_id: Option<Uuid>,
    pub state: TaskState,
    pub params: Value,
    pub step: Option<String>,
    pub step_state: Value,
    pub progress: Option<f32>,
    pub error: Option<String>,
    pub created_by: Option<UserId>,
    pub attempts: i32,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

macro_rules! task_columns {
    () => {
        "id, tenant_id, kind, target_kind, target_id, state, params, step, \
    step_state, progress, error, created_by, attempts, created_at, started_at, finished_at"
    };
}

pub struct NewTask<'a> {
    pub tenant_id: TenantId,
    pub kind: &'a str,
    pub target: Option<(&'a str, Uuid)>,
    pub params: Value,
    pub created_by: Option<UserId>,
}

pub async fn enqueue(db: impl PgExecutor<'_>, new: NewTask<'_>) -> Result<Task> {
    Ok(sqlx::query_as(concat!(
        "INSERT INTO tasks (id, tenant_id, kind, target_kind, target_id, params, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING ",
        task_columns!()
    ))
    .bind(TaskId::new())
    .bind(new.tenant_id)
    .bind(new.kind)
    .bind(new.target.map(|t| t.0))
    .bind(new.target.map(|t| t.1))
    .bind(new.params)
    .bind(new.created_by)
    .fetch_one(db)
    .await?)
}

pub async fn get(db: impl PgExecutor<'_>, id: TaskId) -> Result<Task> {
    sqlx::query_as(concat!(
        "SELECT ",
        task_columns!(),
        " FROM tasks WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(db)
    .await?
    .ok_or_else(|| {
        hm_core::Error::NotFound {
            kind: "task",
            id: id.to_string(),
        }
        .into()
    })
}

/// Claims the oldest runnable task: a pending one, or a running one whose
/// lease has expired (its previous owner is presumed dead).
/// Tasks for the same target run in creation order: a task is not claimable
/// while an older one for its target is unfinished.
pub async fn claim(db: impl PgExecutor<'_>, owner: &str, lease: Duration) -> Result<Option<Task>> {
    Ok(sqlx::query_as(concat!("UPDATE tasks SET state = 'running', lease_owner = $1,
                lease_expires_at = now() + $2 * interval '1 millisecond',
                attempts = attempts + 1, started_at = coalesce(started_at, now()), updated_at = now()
         WHERE id = (
             SELECT t.id FROM tasks t
             WHERE (t.state = 'pending' OR (t.state = 'running' AND t.lease_expires_at < now()))
               AND NOT EXISTS (
                   SELECT 1 FROM tasks o
                   WHERE t.target_id IS NOT NULL AND o.target_id = t.target_id
                     AND (o.created_at, o.id) < (t.created_at, t.id)
                     AND o.state IN ('pending', 'running'))
             ORDER BY t.created_at, t.id
             FOR UPDATE SKIP LOCKED
             LIMIT 1)
         RETURNING ", task_columns!()))
    .bind(owner)
    .bind(lease.as_millis() as f64)
    .fetch_optional(db)
    .await?)
}

/// Records step progress and extends the lease. Returns false if the caller no
/// longer owns the task, in which case it must stop working on it.
pub async fn checkpoint(
    db: impl PgExecutor<'_>,
    id: TaskId,
    owner: &str,
    lease: Duration,
    step: &str,
    step_state: &Value,
    progress: Option<f32>,
) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE tasks SET step = $3, step_state = $4, progress = $5,
                lease_expires_at = now() + $6 * interval '1 millisecond', updated_at = now()
         WHERE id = $1 AND lease_owner = $2 AND state = 'running'",
    )
    .bind(id)
    .bind(owner)
    .bind(step)
    .bind(step_state)
    .bind(progress)
    .bind(lease.as_millis() as f64)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

pub async fn finish(
    db: impl PgExecutor<'_>,
    id: TaskId,
    owner: &str,
    outcome: Result<(), String>,
) -> Result<bool> {
    let (state, error) = match outcome {
        Ok(()) => (TaskState::Succeeded, None),
        Err(e) => (TaskState::Failed, Some(e)),
    };
    let res = sqlx::query(
        "UPDATE tasks SET state = $3, error = $4, finished_at = now(), updated_at = now(),
                progress = CASE WHEN $3 = 'succeeded' THEN 1 ELSE progress END,
                lease_owner = NULL, lease_expires_at = NULL
         WHERE id = $1 AND lease_owner = $2 AND state = 'running'",
    )
    .bind(id)
    .bind(owner)
    .bind(state)
    .bind(error)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Recent tasks in a tenant's subtree, newest first, optionally for one target.
pub async fn list(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    target_id: Option<Uuid>,
    limit: i64,
) -> Result<Vec<Task>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        task_columns!(),
        " FROM tasks
         WHERE tenant_id IN (SELECT id FROM tenants WHERE path <@ (SELECT path FROM tenants WHERE id = $1))
           AND ($2::uuid IS NULL OR target_id = $2)
         ORDER BY created_at DESC LIMIT $3"
    ))
    .bind(tenant_id)
    .bind(target_id)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// Extends the lease of a task the caller owns. Returns false if ownership
/// was lost (the lease expired and another worker claimed it).
pub async fn renew_lease(
    db: impl PgExecutor<'_>,
    id: TaskId,
    owner: &str,
    lease: Duration,
) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE tasks SET lease_expires_at = now() + $3 * interval '1 millisecond'
         WHERE id = $1 AND lease_owner = $2 AND state = 'running'",
    )
    .bind(id)
    .bind(owner)
    .bind(lease.as_millis() as f64)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Updates progress without touching step state.
pub async fn set_progress(
    db: impl PgExecutor<'_>,
    id: TaskId,
    step: &str,
    progress: f32,
) -> Result<()> {
    sqlx::query("UPDATE tasks SET step = $2, progress = $3, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(step)
        .bind(progress)
        .execute(db)
        .await?;
    Ok(())
}

/// The unfinished task for each target that has one (the oldest, which runs
/// first), as `(target_id, kind)`.
pub async fn active_by_target(db: impl PgExecutor<'_>) -> Result<Vec<(Uuid, String)>> {
    Ok(sqlx::query_as(
        "SELECT DISTINCT ON (target_id) target_id, kind FROM tasks
         WHERE state IN ('pending', 'running') AND target_id IS NOT NULL
         ORDER BY target_id, created_at, id",
    )
    .fetch_all(db)
    .await?)
}
