//! Durable task execution.
//!
//! Every replica runs a worker that claims tasks from Postgres. A claimed
//! task's lease is renewed while its handler runs; if the replica dies, the
//! lease lapses and another replica resumes the task. Handlers are therefore
//! written as sequences of idempotent steps.

mod migrate;
pub use migrate::DRIVEN_PROTOCOL;
mod recover;
mod vm;

use std::sync::Arc;
use std::time::Duration;

use hm_db::tasks::Task;
use tokio::sync::Semaphore;

use crate::state::AppState;

const LEASE: Duration = Duration::from_secs(60);
const RENEW_EVERY: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(500);
const MAX_CONCURRENT: usize = 16;

/// Whether an image was published by the provider (the root tenant), whose
/// downloads may come from internal addresses; tenants' may not.
pub(crate) async fn image_trusted(
    db: &hm_db::sqlx::PgPool,
    image: &hm_db::images::Image,
) -> anyhow::Result<bool> {
    Ok(hm_db::tenants::get(db, image.tenant_id)
        .await?
        .parent_id
        .is_none())
}

/// Context handed to task handlers.
pub struct TaskCtx {
    pub state: AppState,
    pub task: Task,
}

impl TaskCtx {
    /// Records the current step for display. Failures are logged, not fatal.
    pub async fn step(&self, step: &str, progress: f32) {
        if let Err(e) =
            hm_db::tasks::set_progress(&self.state.db, self.task.id, step, progress).await
        {
            tracing::warn!(task = %self.task.id, error = %e, "recording task progress");
        }
    }
}

pub async fn run(state: AppState) {
    let slots = Arc::new(Semaphore::new(MAX_CONCURRENT));
    loop {
        let permit = slots
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore never closes");
        let task = match hm_db::tasks::claim(&state.db, &state.replica, LEASE).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                drop(permit);
                tokio::time::sleep(POLL).await;
                continue;
            }
            Err(e) => {
                tracing::error!(error = %e, "claiming task");
                drop(permit);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            execute(state, task).await;
            drop(permit);
        });
    }
}

async fn execute(state: AppState, task: Task) {
    let (id, kind) = (task.id, task.kind.clone());
    let started = std::time::Instant::now();
    tracing::info!(task = %id, %kind, attempt = task.attempts, "running task");

    let renew = {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(RENEW_EVERY).await;
                match hm_db::tasks::renew_lease(&state.db, id, &state.replica, LEASE).await {
                    Ok(true) => {}
                    Ok(false) => return tracing::warn!(task = %id, "lost task lease"),
                    Err(e) => tracing::warn!(task = %id, error = %e, "renewing task lease"),
                }
            }
        })
    };

    let ctx = TaskCtx {
        state: state.clone(),
        task,
    };
    let outcome = match kind.as_str() {
        "vm.create" => vm::create(&ctx).await,
        "vm.start" => vm::start(&ctx).await,
        "vm.stop" => vm::stop(&ctx).await,
        "vm.reboot" => vm::reboot(&ctx).await,
        "vm.delete" => vm::delete(&ctx).await,
        "vm.migrate" => migrate::migrate(&ctx).await,
        "host.evacuate" => migrate::evacuate(&ctx).await,
        "host.recover" => recover::recover(&ctx).await,
        other => Err(anyhow::anyhow!("unknown task kind {other}")),
    };
    renew.abort();

    let outcome = outcome.map_err(|e| format!("{e:#}"));
    crate::metrics::task_finished(&kind, outcome.is_ok(), started);
    match &outcome {
        Ok(()) => tracing::info!(task = %id, %kind, "task succeeded"),
        Err(e) => tracing::warn!(task = %id, %kind, error = %e, "task failed"),
    }
    match hm_db::tasks::finish(&state.db, id, &state.replica, outcome).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(task = %id, "task finished after losing its lease; result discarded")
        }
        Err(e) => tracing::error!(task = %id, error = %e, "recording task result"),
    }
}
