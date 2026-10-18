//! Durable task execution.
//!
//! Every replica runs a worker that claims tasks from Postgres. A claimed
//! task's lease is renewed while its handler runs; if the replica dies, the
//! lease lapses and another replica resumes the task. Handlers are therefore
//! written as sequences of idempotent steps.

mod backup;
mod disk;
mod image;
mod migrate;
mod recover;
mod snapshot;
mod vm;

pub use backup::{INCREMENTAL as INCREMENTAL_BACKUP, schedule as backup_schedule};
pub use disk::sweep as sweep_disks;
pub use image::{deleted as image_deleted, prune as prune_images};

use std::sync::Arc;
use std::time::Duration;

use mst_db::tasks::Task;
use tokio::sync::Semaphore;

use crate::state::AppState;

const LEASE: Duration = Duration::from_secs(60);
const RENEW_EVERY: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(500);
const MAX_CONCURRENT: usize = 16;
/// Downloads run beside other tasks, in slots of their own: slow ones never
/// hold up VMs' lifecycles, migrations or recovery.
const MAX_DOWNLOADS: usize = 4;
const DOWNLOADS: &[&str] = &["image.download"];

/// Whether an image was published by the provider (the root tenant), whose
/// downloads may come from internal addresses; tenants' may not.
pub(crate) async fn image_trusted(
    db: &mst_db::sqlx::PgPool,
    image: &mst_db::images::Image,
) -> anyhow::Result<bool> {
    Ok(mst_db::tenants::get(db, image.tenant_id)
        .await?
        .parent_id
        .is_none())
}

/// Context handed to task handlers.
pub struct TaskCtx {
    pub state: AppState,
    pub task: Task,
    /// The task's slot among those running here, and where it came from.
    slot: tokio::sync::Mutex<Option<tokio::sync::OwnedSemaphorePermit>>,
    slots: Arc<Semaphore>,
}

impl TaskCtx {
    /// Waits for `fut` without holding a slot: a task waiting on something
    /// else (an image downloading) does not keep others from running.
    pub async fn idle<T>(&self, fut: impl std::future::Future<Output = T>) -> T {
        drop(self.slot.lock().await.take());
        let out = fut.await;
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore never closes");
        *self.slot.lock().await = Some(permit);
        out
    }

    /// Records the current step for display. Failures are logged, not fatal.
    pub async fn step(&self, step: &str, progress: f32) {
        if let Err(e) =
            mst_db::tasks::set_progress(&self.state.db, self.task.id, step, progress).await
        {
            tracing::warn!(task = %self.task.id, error = %e, "recording task progress");
        }
    }
}

pub async fn run(state: AppState) {
    tokio::spawn(work(state.clone(), MAX_DOWNLOADS, DOWNLOADS, true));
    work(state, MAX_CONCURRENT, DOWNLOADS, false).await;
}

/// Claims and runs tasks of `kinds` (`only`), or of any other kind, up to
/// `max` at once.
async fn work(state: AppState, max: usize, kinds: &'static [&'static str], only: bool) {
    let slots = Arc::new(Semaphore::new(max));
    loop {
        let permit = slots
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore never closes");
        let claimed = mst_db::tasks::claim(&state.db, &state.replica, LEASE, kinds, only).await;
        let task = match claimed {
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
        let slots = slots.clone();
        tokio::spawn(async move {
            execute(state, task, permit, slots).await;
        });
    }
}

async fn execute(
    state: AppState,
    task: Task,
    permit: tokio::sync::OwnedSemaphorePermit,
    slots: Arc<Semaphore>,
) {
    let (id, kind) = (task.id, task.kind.clone());
    let started = std::time::Instant::now();
    tracing::info!(task = %id, %kind, attempt = task.attempts, "running task");

    let renew = {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(RENEW_EVERY).await;
                match mst_db::tasks::renew_lease(&state.db, id, &state.replica, LEASE).await {
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
        slot: tokio::sync::Mutex::new(Some(permit)),
        slots,
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
        "backup.create" => backup::create(&ctx).await,
        "backup.delete" => backup::delete(&ctx).await,
        "snapshot.create" => snapshot::create(&ctx).await,
        "snapshot.revert" => snapshot::revert(&ctx).await,
        "snapshot.delete" => snapshot::delete(&ctx).await,
        "disk.move" => disk::move_disk(&ctx).await,
        "disk.attach" => disk::attach(&ctx).await,
        "disk.detach" => disk::detach(&ctx).await,
        "image.download" => image::download(&ctx).await,
        other => Err(anyhow::anyhow!("unknown task kind {other}")),
    };
    renew.abort();

    let outcome = outcome.map_err(|e| format!("{e:#}"));
    crate::metrics::task_finished(&kind, outcome.is_ok(), started);
    match &outcome {
        Ok(()) => tracing::info!(task = %id, %kind, "task succeeded"),
        Err(e) => tracing::warn!(task = %id, %kind, error = %e, "task failed"),
    }
    match mst_db::tasks::finish(&state.db, id, &state.replica, outcome).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(task = %id, "task finished after losing its lease; result discarded")
        }
        Err(e) => tracing::error!(task = %id, error = %e, "recording task result"),
    }
}
