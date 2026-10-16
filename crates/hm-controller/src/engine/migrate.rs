//! Live migration and host evacuation tasks.
//!
//! A migration runs in states recorded in the `migrations` row, so a task
//! resumed by another controller (or after a crash) continues where it was:
//!
//! - `preparing`: the destination gets the VM's local volumes (as fresh
//!   copies of its image; only the changed top layer is copied later), its
//!   storage pools and its cloud-init seed.
//! - `running`: the VM's port may bind on both hosts, and the source's
//!   libvirt migrates the VM, copying memory and local disks while the guest
//!   runs. Progress is polled from the source; a cancel aborts the job.
//! - `committing`: the VM runs on the destination; the source's copies are
//!   removed and the database records the move.
//!
//! Before acting on uncertainty (resuming, or after an error) the task asks
//! both hosts where the VM is, and only ever removes a host's copy when the
//! other host demonstrably has the VM.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use hm_core::ids::{HostId, VmId};
use hm_db::migrations::Migration;
use hm_proto::command::Cmd;
use hm_proto::{
    AbortMigration, DefineVm, DeleteVolume, DomainInfo, EnsureImage, EnsureVolume, MigrateVm,
    MigrationStats, PrepareMigration, UndefineVm,
};
use serde::Deserialize;

use super::TaskCtx;

const QUICK: Duration = Duration::from_secs(60);
const IMAGE: Duration = Duration::from_secs(30 * 60);
/// The agent's migrate call returns when libvirt is done; progress is
/// watched separately, so this only bounds a hung host.
const TRANSFER: Duration = Duration::from_secs(6 * 3600);
const POLL: Duration = Duration::from_secs(1);

#[derive(Debug, Default, Deserialize)]
struct Where {
    defined: bool,
    power: String,
    job_active: bool,
}

#[derive(Debug, Default, Deserialize)]
struct Stats {
    active: bool,
    data_total: u64,
    data_processed: u64,
}

/// Where a VM turned out to be.
#[derive(Debug, PartialEq)]
enum Settled {
    Source,
    Destination,
}

/// Progress within the share of the task this migration has.
#[derive(Clone, Copy)]
struct Span(f32, f32);

impl Span {
    fn at(self, f: f32) -> f32 {
        self.0 + (self.1 - self.0) * f.clamp(0.0, 1.0)
    }
}

pub async fn migrate(ctx: &TaskCtx) -> Result<()> {
    let vm = VmId(ctx.task.target_id.context("task has no target VM")?);
    match hm_db::migrations::get(&ctx.state.db, vm).await? {
        Some(m) if m.task_id == ctx.task.id => run(ctx, m, Span(0.0, 1.0)).await,
        Some(_) => bail!("another migration of this VM is in progress"),
        // Finished before a restart: committed, or rolled back.
        None => {
            let dest: Option<HostId> = ctx
                .task
                .params
                .get("dest_host_id")
                .and_then(|d| serde_json::from_value(d.clone()).ok());
            let now = hm_db::vms::get(&ctx.state.db, vm).await?.host_id;
            if dest.is_some() && now == dest {
                Ok(())
            } else {
                bail!("the migration was rolled back")
            }
        }
    }
}

/// Moves every running VM off a host (put in maintenance first, so nothing
/// new lands there), one at a time.
pub async fn evacuate(ctx: &TaskCtx) -> Result<()> {
    let db = &ctx.state.db;
    let host = HostId(ctx.task.target_id.context("task has no target host")?);
    let mut failures = vec![];
    // Migrations this task started before a restart.
    for m in hm_db::migrations::of_task(db, ctx.task.id).await? {
        if let Err(e) = run(ctx, m.clone(), Span(0.0, 0.05)).await {
            failures.push(format!("{}: {e:#}", m.vm_id));
        }
    }
    let vms: Vec<_> = hm_db::vms::list_on_host(db, host)
        .await?
        .into_iter()
        .filter(|v| v.status == "ready")
        .collect();
    let (running, stopped): (Vec<_>, Vec<_>) = vms.into_iter().partition(|v| v.power == "running");
    // VMs bound to the host stay, like stopped ones; they keep running.
    let (running, bound): (Vec<_>, Vec<_>) = running.into_iter().partition(|v| v.spec.0.migratable);
    let n = running.len().max(1) as f32;
    for (i, vm) in running.iter().enumerate() {
        let span = Span(i as f32 / n, (i + 1) as f32 / n);
        ctx.step(
            &format!("moving {} ({} of {})", vm.name, i + 1, running.len()),
            span.at(0.0),
        )
        .await;
        let mut tx = db.begin().await?;
        match crate::migration::plan(&mut tx, vm, None, &[host], ctx.task.id).await {
            Ok((dest, name)) => {
                tx.commit().await?;
                tracing::info!(vm = %vm.id, to = %name, "evacuating");
                let m = hm_db::migrations::get(db, vm.id)
                    .await?
                    .context("migration vanished")?;
                debug_assert_eq!(m.dest_host_id, dest);
                if let Err(e) = run(ctx, m, span).await {
                    failures.push(format!("{}: {e:#}", vm.name));
                }
            }
            Err(e) => failures.push(format!("{}: {e}", vm.name)),
        }
    }
    if !failures.is_empty() {
        bail!(
            "{} VM(s) could not be moved: {}",
            failures.len(),
            failures.join("; ")
        );
    }
    let staying: Vec<String> = bound
        .iter()
        .map(|v| format!("{} (not migratable)", v.name))
        .chain(stopped.iter().map(|v| format!("{} (stopped)", v.name)))
        .collect();
    if !staying.is_empty() {
        tracing::info!(%host, staying = ?staying, "VMs that stay on the evacuated host");
        ctx.step(&format!("staying: {}", staying.join(", ")), 1.0)
            .await;
    }
    Ok(())
}

async fn run(ctx: &TaskCtx, mut m: Migration, span: Span) -> Result<()> {
    match drive(ctx, &mut m, span).await {
        Ok(()) => Ok(()),
        Err(e) => {
            let e = if latest(ctx, &m).await.is_some_and(|m| m.cancel_requested) {
                anyhow!("the migration was cancelled")
            } else {
                e
            };
            // The error may have come after the VM moved (a lost answer, a
            // failure while committing): find out before undoing anything.
            match settle(ctx, &m).await {
                Ok(Settled::Destination) => {
                    tracing::info!(vm = %m.vm_id, "migration completed despite: {e:#}");
                    commit(ctx, &m).await
                }
                Ok(Settled::Source) => {
                    rollback(ctx, &m).await.context("rolling back")?;
                    Err(e)
                }
                Err(why) => Err(e.context(format!(
                    "the VM's location is unclear ({why:#}); it stays marked as migrating"
                ))),
            }
        }
    }
}

async fn latest(ctx: &TaskCtx, m: &Migration) -> Option<Migration> {
    hm_db::migrations::get(&ctx.state.db, m.vm_id)
        .await
        .ok()
        .flatten()
}

async fn drive(ctx: &TaskCtx, m: &mut Migration, span: Span) -> Result<()> {
    loop {
        match m.state.as_str() {
            "preparing" => {
                ctx.step("preparing the destination", span.at(0.02)).await;
                prepare(ctx, m).await.context("preparing the destination")?;
                hm_db::migrations::set_state(&ctx.state.db, m.vm_id, "running").await?;
                m.state = "running".into();
                // Let the destination bind the VM's port alongside the source.
                ctx.state.net.sync().await.context("updating the network")?;
            }
            "running" => {
                // Cancelled while preparing: stop before copying anything.
                if latest(ctx, m).await.is_some_and(|m| m.cancel_requested) {
                    bail!("the migration was cancelled");
                }
                transfer(ctx, m, span).await?;
                hm_db::migrations::set_state(&ctx.state.db, m.vm_id, "committing").await?;
                m.state = "committing".into();
            }
            "committing" => {
                ctx.step("finishing", span.at(0.98)).await;
                return commit(ctx, m).await;
            }
            other => bail!("unknown migration state {other}"),
        }
    }
}

async fn info(ctx: &TaskCtx, host: HostId, vm: VmId) -> Result<Where> {
    ctx.state
        .agents
        .query(
            host,
            Cmd::DomainInfo(DomainInfo {
                vm_id: vm.to_string(),
            }),
            QUICK,
        )
        .await
        .map_err(|e| anyhow!(e))
}

/// The VM's local disks: (target device, volume, pool).
async fn local_disks(
    ctx: &TaskCtx,
    vm: &hm_db::vms::Vm,
) -> Result<Vec<(String, hm_core::vm::DiskSpec, hm_db::storage_pools::Pool)>> {
    let mut out = vec![];
    for (i, d) in vm.spec.0.disks.iter().enumerate() {
        let pool = hm_db::storage_pools::get(&ctx.state.db, d.pool_id).await?;
        if pool.kind == "local" {
            out.push((format!("vd{}", (b'a' + i as u8) as char), d.clone(), pool));
        }
    }
    Ok(out)
}

async fn prepare(ctx: &TaskCtx, m: &Migration) -> Result<()> {
    let db = &ctx.state.db;
    let vm = hm_db::vms::get(db, m.vm_id).await?;
    let agents = &ctx.state.agents;
    let dest = m.dest_host_id;
    for (_, d, pool) in local_disks(ctx, &vm).await? {
        let p = crate::api::agent_pool(&pool);
        if let Some(image) = d.image_id {
            let image = hm_db::images::get(db, image).await?;
            let cmd = EnsureImage {
                image_id: image.id.to_string(),
                source_url: image.source_url,
                format: image.format,
                pool: Some(p.clone()),
            };
            agents
                .call(dest, Cmd::EnsureImage(cmd), IMAGE)
                .await
                .map_err(|e| anyhow!(e))
                .context("fetching the image")?;
        }
        let cmd = EnsureVolume {
            volume_id: d.volume_id.to_string(),
            size_bytes: d.size_bytes,
            image_id: d.image_id.map(|i| i.to_string()).unwrap_or_default(),
            pool: Some(p),
        };
        agents
            .call(dest, Cmd::EnsureVolume(cmd), QUICK)
            .await
            .map_err(|e| anyhow!(e))
            .context("creating a volume")?;
    }
    let mut pools = std::collections::BTreeMap::new();
    for d in &vm.spec.0.disks {
        if let std::collections::btree_map::Entry::Vacant(e) = pools.entry(d.pool_id) {
            e.insert(crate::api::agent_pool(
                &hm_db::storage_pools::get(db, d.pool_id).await?,
            ));
        }
    }
    let define = DefineVm {
        vm_id: vm.id.to_string(),
        tenant_id: vm.tenant_id.to_string(),
        name: vm.name.clone(),
        generation: vm.generation as u64,
        spec_json: serde_json::to_string(&vm.spec.0)?,
        pools: pools.into_values().collect(),
    };
    agents
        .call(
            dest,
            Cmd::PrepareMigration(PrepareMigration { vm: Some(define) }),
            QUICK,
        )
        .await
        .map_err(|e| anyhow!(e))
}

async fn transfer(ctx: &TaskCtx, m: &Migration, span: Span) -> Result<()> {
    let (src, dst, vm_id) = (m.source_host_id, m.dest_host_id, m.vm_id);
    // A migration started before a restart may still be going.
    if info(ctx, src, vm_id)
        .await
        .context("asking the source")?
        .job_active
    {
        watch(ctx, m, span, true).await?;
    }
    let here = info(ctx, src, vm_id).await.context("asking the source")?;
    if !here.defined {
        let there = info(ctx, dst, vm_id)
            .await
            .context("asking the destination")?;
        if there.defined && there.power == "running" {
            return Ok(());
        }
        bail!("the VM is on neither host");
    }
    let vm = hm_db::vms::get(&ctx.state.db, vm_id).await?;
    let dest = hm_db::hosts::get(&ctx.state.db, dst).await?;
    let cmd = MigrateVm {
        vm_id: vm_id.to_string(),
        dest_address: dest.address.clone(),
        copy_disks: local_disks(ctx, &vm)
            .await?
            .into_iter()
            .map(|(t, _, _)| t)
            .collect(),
    };
    tracing::info!(vm = %vm_id, to = %dest.name, "live-migrating");
    let call = ctx.state.agents.call(src, Cmd::MigrateVm(cmd), TRANSFER);
    tokio::select! {
        r = call => r.map_err(|e| anyhow!(e)).context("migrating"),
        r = watch(ctx, m, span, false) => r.and(Err(anyhow!("the migration job ended without an answer"))),
    }
}

/// Reports a running migration's progress and aborts it if cancelled.
/// Returns once the job is over if `until_done`; otherwise runs until
/// dropped.
async fn watch(ctx: &TaskCtx, m: &Migration, span: Span, until_done: bool) -> Result<()> {
    let mut aborted = false;
    loop {
        tokio::time::sleep(POLL).await;
        let st: Stats = match ctx
            .state
            .agents
            .query(
                m.source_host_id,
                Cmd::MigrationStats(MigrationStats {
                    vm_id: m.vm_id.to_string(),
                }),
                QUICK,
            )
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(vm = %m.vm_id, error = %e, "migration progress");
                continue;
            }
        };
        if st.active {
            let frac = if st.data_total > 0 {
                st.data_processed as f32 / st.data_total as f32
            } else {
                0.0
            };
            ctx.step(
                &format!(
                    "copying: {} of {}",
                    gib(st.data_processed),
                    gib(st.data_total)
                ),
                span.at(0.05 + 0.9 * frac),
            )
            .await;
        } else if until_done {
            return Ok(());
        }
        if !aborted && latest(ctx, m).await.is_some_and(|m| m.cancel_requested) {
            tracing::info!(vm = %m.vm_id, "cancelling migration");
            let cmd = AbortMigration {
                vm_id: m.vm_id.to_string(),
            };
            if ctx
                .state
                .agents
                .call(m.source_host_id, Cmd::AbortMigration(cmd), QUICK)
                .await
                .is_ok()
            {
                aborted = true;
            }
        }
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
}

/// Where the VM is now, judged by the hosts themselves.
async fn settle(ctx: &TaskCtx, m: &Migration) -> Result<Settled> {
    let (src, dst, vm) = (m.source_host_id, m.dest_host_id, m.vm_id);
    let mut here = info(ctx, src, vm).await;
    if here.as_ref().is_ok_and(|h| h.job_active) {
        // Still migrating (e.g. our call timed out): let it finish.
        watch(ctx, m, Span(0.0, 1.0), true).await?;
        here = info(ctx, src, vm).await;
    }
    let there = info(ctx, dst, vm).await;
    match (here, there) {
        (Ok(h), Ok(t)) if !h.defined && t.defined && t.power == "running" => {
            Ok(Settled::Destination)
        }
        (Ok(h), _) if h.defined && matches!(h.power.as_str(), "running" | "paused") => {
            Ok(Settled::Source)
        }
        (Ok(h), Ok(t)) => bail!("source: {}, destination: {}", h.power, t.power),
        (Err(e), _) => Err(e.context("the source host does not answer")),
        (Ok(_), Err(e)) => Err(e.context("the destination host does not answer")),
    }
}

/// The VM runs on the destination: remove the source's copies and record
/// the move.
async fn commit(ctx: &TaskCtx, m: &Migration) -> Result<()> {
    let (src, vm_id) = (m.source_host_id, m.vm_id);
    let vm = hm_db::vms::get(&ctx.state.db, vm_id).await?;
    // Only once the source demonstrably no longer has the domain.
    match info(ctx, src, vm_id).await {
        Ok(w) if !w.defined => {
            let agents = &ctx.state.agents;
            if let Err(e) = agents
                .call(
                    src,
                    Cmd::UndefineVm(UndefineVm {
                        vm_id: vm_id.to_string(),
                    }),
                    QUICK,
                )
                .await
            {
                tracing::warn!(vm = %vm_id, error = %e, "removing the source's seed");
            }
            for (_, d, pool) in local_disks(ctx, &vm).await? {
                let cmd = DeleteVolume {
                    volume_id: d.volume_id.to_string(),
                    pool: Some(crate::api::agent_pool(&pool)),
                };
                if let Err(e) = agents.call(src, Cmd::DeleteVolume(cmd), QUICK).await {
                    tracing::warn!(vm = %vm_id, error = %e, "deleting the source's copy of a volume; it is left behind");
                }
            }
        }
        Ok(_) => bail!("the source still has the VM"),
        Err(e) => {
            tracing::warn!(vm = %vm_id, error = %e, "the source does not answer; its copies are left behind")
        }
    }
    let mut tx = ctx.state.db.begin().await?;
    hm_db::migrations::committed(&mut tx, vm_id, m.dest_host_id).await?;
    tx.commit().await?;
    tracing::info!(vm = %vm_id, host = %m.dest_host_id, "migration committed");
    if let Err(e) = ctx.state.net.sync().await {
        tracing::warn!(error = %format!("{e:#}"), "updating the network after a migration");
        ctx.state.net.kick();
    }
    Ok(())
}

/// The VM stays on the source: remove what the destination got.
async fn rollback(ctx: &TaskCtx, m: &Migration) -> Result<()> {
    let (dst, vm_id) = (m.dest_host_id, m.vm_id);
    let vm = hm_db::vms::get(&ctx.state.db, vm_id).await?;
    let agents = &ctx.state.agents;
    match info(ctx, dst, vm_id).await {
        Ok(w) if w.power == "running" => bail!("the destination runs the VM too"),
        Ok(_) => {
            // An incoming domain libvirt left behind, and the seed.
            if let Err(e) = agents
                .call(
                    dst,
                    Cmd::UndefineVm(UndefineVm {
                        vm_id: vm_id.to_string(),
                    }),
                    QUICK,
                )
                .await
            {
                tracing::warn!(vm = %vm_id, error = %e, "cleaning up the destination");
            }
            for (_, d, pool) in local_disks(ctx, &vm).await? {
                let cmd = DeleteVolume {
                    volume_id: d.volume_id.to_string(),
                    pool: Some(crate::api::agent_pool(&pool)),
                };
                if let Err(e) = agents.call(dst, Cmd::DeleteVolume(cmd), QUICK).await {
                    tracing::warn!(vm = %vm_id, error = %e, "deleting the destination's copy of a volume");
                }
            }
        }
        Err(e) => {
            tracing::warn!(vm = %vm_id, error = %e, "the destination does not answer; its copies are left behind")
        }
    }
    let mut tx = ctx.state.db.begin().await?;
    hm_db::migrations::rolled_back(&mut tx, vm_id).await?;
    tx.commit().await?;
    tracing::info!(vm = %vm_id, "migration rolled back");
    if let Err(e) = ctx.state.net.sync().await {
        tracing::warn!(error = %format!("{e:#}"), "updating the network after a rollback");
        ctx.state.net.kick();
    }
    Ok(())
}
