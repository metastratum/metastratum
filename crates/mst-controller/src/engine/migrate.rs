//! Live migration and host evacuation tasks.
//!
//! A migration runs in states recorded in the `migrations` row, so a task
//! resumed by another controller (or after a crash) continues where it was:
//!
//! - `preparing`: the destination gets the VM's local volumes (blank, to be
//!   copied whole), its storage pools and its cloud-init seed.
//! - `running`: the VM's port may bind on both hosts, and the VM is
//!   migrated, copying memory and local disks while the guest runs.
//!   Progress is polled from the source; a cancel aborts the job.
//!
//!   The controller drives the migration: libvirt's Begin, Prepare,
//!   Perform, Finish and Confirm, each on its own host's libvirt, carrying
//!   the cookies between them and recording the step reached (`handoff`).
//!   Hosts' libvirt daemons never talk to each other: only QEMU sends the
//!   VM over, with TLS credentials made for this migration alone.
//! - `committing`: the VM runs on the destination; the source's copies are
//!   removed and the database records the move.
//!
//! Before acting on uncertainty (resuming, or after an error) the task asks
//! both hosts where the VM is, and only ever removes a host's copy when the
//! other host demonstrably has the VM.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use mst_core::ids::{HostId, VmId};
use mst_db::migrations::Migration;
use mst_proto::command::Cmd;
use mst_proto::migration_step::Step;
use mst_proto::{
    AbortMigration, DefineVm, DeleteVolume, DomainInfo, EnsureVolume, MigrationStats,
    MigrationStep, PrepareMigration, UndefineVm,
};
use serde::{Deserialize, Serialize};

use super::TaskCtx;

const QUICK: Duration = Duration::from_secs(60);
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
    match mst_db::migrations::get(&ctx.state.db, vm).await? {
        Some(m) if m.task_id == ctx.task.id => run(ctx, m, Span(0.0, 1.0)).await,
        Some(_) => bail!("another migration of this VM is in progress"),
        // Finished before a restart: committed, or rolled back.
        None => {
            let dest: Option<HostId> = ctx
                .task
                .params
                .get("dest_host_id")
                .and_then(|d| serde_json::from_value(d.clone()).ok());
            let now = mst_db::vms::get(&ctx.state.db, vm).await?.host_id;
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
    for m in mst_db::migrations::of_task(db, ctx.task.id).await? {
        if let Err(e) = run(ctx, m.clone(), Span(0.0, 0.05)).await {
            failures.push(format!("{}: {e:#}", m.vm_id));
        }
    }
    let vms: Vec<_> = mst_db::vms::list_on_host(db, host)
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
                let m = mst_db::migrations::get(db, vm.id)
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
    // Images only it holds go elsewhere too (none is downloaded again).
    let images = super::image::keep_elsewhere(ctx, host).await?;
    if !failures.is_empty() || !images.is_empty() {
        let mut why = vec![];
        if !failures.is_empty() {
            why.push(format!(
                "{} VM(s) could not be moved: {}",
                failures.len(),
                failures.join("; ")
            ));
        }
        if !images.is_empty() {
            why.push(format!(
                "{} image(s) only it holds could not be copied elsewhere: {}",
                images.len(),
                images.join("; ")
            ));
        }
        bail!("{}", why.join("; "));
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
    mst_db::migrations::get(&ctx.state.db, m.vm_id)
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
                mst_db::migrations::set_state(&ctx.state.db, m.vm_id, "running").await?;
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
                mst_db::migrations::set_state(&ctx.state.db, m.vm_id, "committing").await?;
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
    vm: &mst_db::vms::Vm,
) -> Result<Vec<(String, mst_core::vm::DiskSpec, mst_db::storage_pools::Pool)>> {
    let mut out = vec![];
    for (d, slot) in vm.spec.0.disks.iter().zip(vm.spec.0.disk_slots()) {
        let pool = mst_db::storage_pools::get(&ctx.state.db, d.pool_id).await?;
        if pool.kind == "local" {
            out.push((slot.target, d.clone(), pool));
        }
    }
    Ok(out)
}

async fn prepare(ctx: &TaskCtx, m: &Migration) -> Result<()> {
    let db = &ctx.state.db;
    let vm = mst_db::vms::get(db, m.vm_id).await?;
    let agents = &ctx.state.agents;
    let dest = m.dest_host_id;
    // Blank: the local disks are copied in whole while the VM runs.
    for (_, d, pool) in local_disks(ctx, &vm).await? {
        let cmd = EnsureVolume {
            volume_id: d.volume_id.to_string(),
            size_bytes: d.size_bytes,
            pool: Some(crate::api::agent_pool(&pool)?),
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
                &mst_db::storage_pools::get(db, d.pool_id).await?,
            )?);
        }
    }
    let define = DefineVm {
        vm_id: vm.id.to_string(),
        tenant_id: vm.tenant_id.to_string(),
        name: vm.name.clone(),
        generation: vm.generation as u64,
        spec_json: crate::guest::host_spec_json(vm.id, &vm.spec.0)?,
        pools: pools.into_values().collect(),
        disk_limits: crate::limits::disk_limits(db, dest).await?,
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
    transfer_driven(ctx, m, span).await
}

/// How long a migration's credentials are valid: QEMU checks them when the
/// copy starts; this only bounds a migration's wait to start.
const OWN_TLS_LIFETIME: Duration = Duration::from_secs(86400);

/// What an agent hands back from a migration step.
#[derive(Debug, Default, Deserialize)]
struct StepOut {
    /// Base64.
    cookie: String,
    xml: Option<String>,
    uri: Option<String>,
}

/// How far a driven migration has got (the migration's `handoff`).
#[derive(Debug, Default, Serialize, Deserialize)]
struct Handoff {
    /// "" before anything; "preparing" while the destination readies an
    /// incoming domain; "performing" while the source sends it;
    /// "performed" once that ended; "finished" once the destination ran it
    /// (or threw it away).
    #[serde(default)]
    phase: String,
    /// From the last step, base64.
    #[serde(default)]
    cookie: String,
    #[serde(default)]
    uri: String,
    #[serde(default)]
    name: String,
    /// The migration is being undone.
    #[serde(default)]
    cancelled: bool,
}

/// Migrates with each host's libvirt doing its own part, driven from here.
/// Resumes from the recorded step after a controller restart: a copy cut
/// off mid-way is undone on both hosts (its outcome went with the replica
/// that waited for it), anything later is carried through.
async fn transfer_driven(ctx: &TaskCtx, m: &Migration, span: Span) -> Result<()> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    let (src, dst, vm_id) = (m.source_host_id, m.dest_host_id, m.vm_id);
    let db = &ctx.state.db;
    let vm = mst_db::vms::get(db, vm_id).await?;
    let dest = mst_db::hosts::get(db, dst).await?;
    let base = MigrationStep {
        vm_id: vm_id.to_string(),
        dest_address: dest.address.clone(),
        copy_disks: local_disks(ctx, &vm)
            .await?
            .into_iter()
            .map(|(t, _, _)| t)
            .collect(),
        ..MigrationStep::default()
    };
    let run = |host: HostId, step: Step, fill: &dyn Fn(&mut MigrationStep), timeout: Duration| {
        let mut cmd = base.clone();
        cmd.step = step as i32;
        fill(&mut cmd);
        async move {
            ctx.state
                .agents
                .query::<StepOut>(host, Cmd::MigrationStep(cmd), timeout)
                .await
                .map_err(|e| anyhow!(e))
        }
    };
    let cookie = |c: &str| b64.decode(c).unwrap_or_default();
    let mut h: Handoff = serde_json::from_value(
        mst_db::migrations::get(db, vm_id)
            .await?
            .map(|m| m.handoff.0)
            .unwrap_or_default(),
    )
    .unwrap_or_default();
    let save = |h: &Handoff| {
        let v = serde_json::to_value(h).expect("serializable");
        async move { mst_db::migrations::set_handoff(db, vm_id, &v).await }
    };
    let mut failure: Option<anyhow::Error> = None;

    if h.phase == "preparing" {
        // Cut off while the destination readied the domain: throw away
        // whatever it readied, and start over.
        let name = h.name.clone();
        let _ = run(
            dst,
            Step::Finish,
            &|c| {
                c.name = name.clone();
                c.cancelled = true;
            },
            QUICK,
        )
        .await;
        h = Handoff::default();
        save(&h).await?;
    }
    if h.phase == "performing" {
        // Cut off while the source sent it, and its answer went with the
        // replica that waited for it: undo it on both hosts. Stop a copy
        // still going, and wait until it is over, or done and waiting for
        // Confirm (libvirt then holds the job, the domain paused).
        let abort = AbortMigration {
            vm_id: vm_id.to_string(),
        };
        let _ = ctx
            .state
            .agents
            .call(src, Cmd::AbortMigration(abort), QUICK)
            .await;
        for _ in 0..300 {
            match info(ctx, src, vm_id).await {
                Ok(w) if !w.job_active || w.power == "paused" => break,
                _ => tokio::time::sleep(POLL).await,
            }
        }
        h.phase = "performed".into();
        h.cookie.clear();
        h.cancelled = true;
        failure = Some(anyhow!(
            "a controller stopped during the copy; it was undone"
        ));
        save(&h).await?;
    }
    if h.phase.is_empty() {
        let begin = run(src, Step::Begin, &|_| {}, QUICK)
            .await
            .context("starting on the source")?;
        let xml = begin.xml.context("the source returned no domain XML")?;
        h.name = domain_name(&xml).context("the domain XML has no name")?;
        h.phase = "preparing".into();
        save(&h).await?;
        // Credentials for this migration alone: the destination accepts the
        // VM from this source only (the CA's other certificates, other
        // hosts' among them, open nothing), and the source sends it to this
        // destination only.
        let own_tls = mst_pki::MigrationCredentials::generate(
            &[dest.address.clone(), dest.name.clone()],
            &src.to_string(),
            OWN_TLS_LIFETIME,
        )?;
        let tls = |server: bool| {
            let t = &own_tls;
            Some(mst_proto::MigrationTls {
                ca_pem: t.ca_pem.clone(),
                cert_pem: if server {
                    &t.server_cert_pem
                } else {
                    &t.client_cert_pem
                }
                .clone(),
                key_pem: if server {
                    &t.server_key_pem
                } else {
                    &t.client_key_pem
                }
                .clone(),
            })
        };
        let begin_cookie = cookie(&begin.cookie);
        let prepared = run(
            dst,
            Step::Prepare,
            &|c| {
                c.cookie = begin_cookie.clone();
                c.xml = xml.clone();
                c.tls = tls(true);
            },
            QUICK,
        )
        .await;
        let prepared = match prepared {
            Ok(p) => p,
            Err(e) => {
                h = Handoff::default();
                save(&h).await?;
                return Err(e.context("preparing the incoming domain"));
            }
        };
        h.uri = prepared.uri.context("the destination returned no URI")?;
        h.phase = "performing".into();
        save(&h).await?;
        tracing::info!(vm = %vm_id, to = %dest.name, uri = %h.uri, "live-migrating (driven)");
        let (prepared_cookie, uri) = (cookie(&prepared.cookie), h.uri.clone());
        let perform = run(
            src,
            Step::Perform,
            &|c| {
                c.cookie = prepared_cookie.clone();
                c.uri = uri.clone();
                c.tls = tls(false);
            },
            TRANSFER,
        );
        let performed = tokio::select! {
            r = perform => r,
            r = watch(ctx, m, span, false) => r.and(Err(anyhow!("the migration job ended without an answer"))),
        };
        match performed {
            Ok(p) => h.cookie = p.cookie,
            Err(e) => {
                h.cookie.clear();
                h.cancelled = true;
                failure = Some(e.context("migrating"));
            }
        }
        h.phase = "performed".into();
        save(&h).await?;
    }
    if h.phase == "performed" {
        let (c, uri, name, cancelled) = (
            cookie(&h.cookie),
            h.uri.clone(),
            h.name.clone(),
            h.cancelled,
        );
        let finished = run(
            dst,
            Step::Finish,
            &|s| {
                s.cookie = c.clone();
                s.uri = uri.clone();
                s.name = name.clone();
                s.cancelled = cancelled;
            },
            QUICK,
        )
        .await;
        match finished {
            Ok(f) => h.cookie = f.cookie,
            Err(e) => {
                h.cookie.clear();
                if !h.cancelled {
                    h.cancelled = true;
                    failure = Some(e.context("starting the VM on the destination"));
                }
            }
        }
        h.phase = "finished".into();
        save(&h).await?;
    }
    if h.phase == "finished" {
        let (c, cancelled) = (cookie(&h.cookie), h.cancelled);
        let confirmed = run(
            src,
            Step::Confirm,
            &|s| {
                s.cookie = c.clone();
                s.cancelled = cancelled;
            },
            QUICK,
        )
        .await;
        match confirmed {
            Ok(_) => {}
            // Undoing: the source may hold no migration to confirm (it
            // failed before it began).
            Err(e) if cancelled => {
                tracing::warn!(vm = %vm_id, error = %format!("{e:#}"), "undoing on the source")
            }
            Err(e) => return Err(e.context("completing on the source")),
        }
        save(&Handoff::default()).await?;
    }
    match failure {
        Some(e) => Err(e),
        None if h.cancelled => Err(anyhow!("the migration was undone")),
        None => Ok(()),
    }
}

/// The `<name>` of a domain XML.
fn domain_name(xml: &str) -> Option<String> {
    let start = xml.find("<name>")? + "<name>".len();
    let end = start + xml[start..].find("</name>")?;
    Some(xml[start..end].trim().to_owned())
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
    // A driven migration's job stays open, the domain paused, until it is
    // confirmed; that is not one still copying.
    if here
        .as_ref()
        .is_ok_and(|h| h.job_active && h.power != "paused")
    {
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
    let vm = mst_db::vms::get(&ctx.state.db, vm_id).await?;
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
                    pool: Some(crate::api::agent_pool(&pool)?),
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
    mst_db::migrations::committed(&mut tx, vm_id, m.dest_host_id).await?;
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
    let vm = mst_db::vms::get(&ctx.state.db, vm_id).await?;
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
                    pool: Some(crate::api::agent_pool(&pool)?),
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
    mst_db::migrations::rolled_back(&mut tx, vm_id).await?;
    tx.commit().await?;
    tracing::info!(vm = %vm_id, "migration rolled back");
    if let Err(e) = ctx.state.net.sync().await {
        tracing::warn!(error = %format!("{e:#}"), "updating the network after a rollback");
        ctx.state.net.kick();
    }
    Ok(())
}
