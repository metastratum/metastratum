//! The network reconciler: makes OVN implement the network model.
//!
//! A pass loads the whole model from Postgres, compiles it into the rows OVN
//! should hold (`hm-netplan`), reads hypermon's rows from the northbound
//! database and applies the difference in one transaction. Passes are
//! serialized across controller replicas by an advisory lock. They run when
//! something changed (`kick`), when a task needs the network in place
//! (`sync`), and periodically to repair drift.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use hm_db::sqlx::PgPool;
use hm_netplan::{ActualRow, Table};
use hm_ovsdb::Northbound;
use tokio::sync::{Mutex, Notify};

/// Advisory lock key ("hm-net").
const LOCK: i64 = 0x686d_2d6e_6574;
const RESYNC: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct Reconciler(Arc<Inner>);

struct Inner {
    db: PgPool,
    nb: Arc<Northbound>,
    kick: Notify,
    /// One pass at a time within this replica.
    running: Mutex<Option<Vec<String>>>,
}

impl Reconciler {
    pub fn new(db: PgPool, nb: Arc<Northbound>) -> Self {
        Self(Arc::new(Inner {
            db,
            nb,
            kick: Notify::new(),
            running: Mutex::new(None),
        }))
    }

    /// Asks for a pass soon, without waiting for it.
    pub fn kick(&self) {
        self.0.kick.notify_one();
    }

    /// Runs passes forever: when kicked, and every [`RESYNC`] regardless.
    pub async fn run(self) {
        loop {
            let started = std::time::Instant::now();
            let result = self.sync().await;
            metrics::histogram!("hm_network_pass_duration_seconds")
                .record(started.elapsed().as_secs_f64());
            metrics::counter!("hm_network_passes_total", "outcome" => if result.is_ok() { "ok" } else { "failed" })
                .increment(1);
            if let Err(e) = result {
                tracing::warn!(error = %format!("{e:#}"), "network reconciliation failed");
            }
            tokio::select! {
                _ = self.0.kick.notified() => {
                    // Let a burst of changes settle into one pass.
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                _ = tokio::time::sleep(RESYNC) => {}
            }
        }
    }

    /// Runs a pass now and waits for it.
    pub async fn sync(&self) -> Result<()> {
        let mut last_warnings = self.0.running.lock().await;
        let mut tx = self.0.db.begin().await?;
        // Other replicas wait here; each pass then reads the latest model.
        hm_db::sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(LOCK)
            .execute(&mut *tx)
            .await?;
        let model = hm_db::netmodel::load(&mut tx, hm_ovsdb::nb::DEFAULT_MTU)
            .await
            .context("loading the network model")?;
        let desired = hm_netplan::compile(&model);
        if last_warnings.as_ref() != Some(&desired.warnings) {
            for w in &desired.warnings {
                tracing::warn!(warning = %w, "network configuration");
            }
            *last_warnings = Some(desired.warnings.clone());
        }
        let actual = self.actual().await.context("reading OVN")?;
        let plan = hm_netplan::plan(&desired, &actual);
        if !plan.is_empty() {
            self.0.nb.transact(plan.ops).await.context("updating OVN")?;
            for (op, n) in [
                ("insert", plan.inserts),
                ("update", plan.updates),
                ("delete", plan.deletes),
            ] {
                metrics::counter!("hm_network_changes_total", "op" => op).increment(n as u64);
            }
            tracing::info!(
                inserts = plan.inserts,
                updates = plan.updates,
                deletes = plan.deletes,
                "network updated"
            );
        }
        tx.commit().await?;
        Ok(())
    }

    async fn actual(&self) -> Result<Vec<ActualRow>> {
        let mut out = vec![];
        for t in Table::ALL {
            for mut row in self.0.nb.select_all(t.name(), t.columns()).await? {
                let Some(uuid) = row
                    .remove("_uuid")
                    .and_then(|u| u.get(1).and_then(|u| u.as_str()).map(str::to_owned))
                else {
                    continue;
                };
                out.push(ActualRow {
                    table: t,
                    uuid,
                    cols: row,
                });
            }
        }
        Ok(out)
    }
}
