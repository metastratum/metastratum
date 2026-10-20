//! Backups: VMs' disks copied into backup repositories, and back.
//!
//! A repository is an NFS pool the host mounts; a backup is a directory in
//! it, `backups/<id>/`, with a qcow2 file per disk (`<index>.qcow2`, vda
//! being 0) and `done.json` once it is complete.
//!
//! An incremental backup's disk file holds what changed since its parent's
//! copy, which is its backing file (see `mst_storage::backups`). A running
//! VM's changes come from its libvirt checkpoint: dirty bitmaps in its disks
//! (`mst-<backup id>`), started by the backup before. Each backup taken live
//! starts one, and the VM keeps only the latest; its description is saved
//! with the backup (`checkpoint.xml`) for libvirt on another host, where the
//! VM may have moved with its bitmaps. A stopped VM's changes are found by
//! comparing its volumes with the parent's copies. Ceph disks are copied in
//! full.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mst_libvirt::{JOB_OPERATION_BACKUP, Power};
use mst_proto::{BackupDisk, BackupVm, DeleteBackup, RestoreVolume};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::session::Agent;

/// What `done.json` holds, and the command answers.
#[derive(Serialize, Deserialize)]
struct Done {
    stored_bytes: u64,
    /// The VM was stopped, or its file systems frozen for the start.
    consistent: bool,
    /// Some disk was stored as what changed since the parent.
    #[serde(default)]
    incremental: bool,
    /// Why none was, though a parent was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    full_reason: Option<String>,
}

/// How a backup is being taken, settled as it starts (`plan.json`, for a
/// retried request to answer with).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Plan {
    /// The disks (by index) stored as changes since the parent.
    incremental: Vec<u32>,
    /// The checkpoint the changes are since (a running VM's).
    since: Option<String>,
    /// The checkpoint this backup starts, if it does.
    starts: Option<String>,
    /// Why no disk is stored as changes, though a parent was given.
    full_reason: Option<String>,
}

/// One request at a time per backup: a retried one waits for the first.
static LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Mutex::default);

fn lock(id: &str) -> Arc<tokio::sync::Mutex<()>> {
    LOCKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(id.to_owned())
        .or_default()
        .clone()
}

fn check_id(id: &str) -> Result<&str> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        bail!("bad backup id {id:?}");
    }
    Ok(id)
}

/// Where a repository keeps its backups.
async fn backups_root(agent: &Agent, repository: Option<&mst_proto::Pool>) -> Result<PathBuf> {
    let mst_storage::Pool::File(pool) = agent.pool(repository).await? else {
        bail!("a backup repository must be a file system (NFS)");
    };
    Ok(pool.root().join("backups"))
}

async fn backup_dir(
    agent: &Agent,
    repository: Option<&mst_proto::Pool>,
    id: &str,
) -> Result<PathBuf> {
    Ok(backups_root(agent, repository).await?.join(check_id(id)?))
}

fn disk_file(dir: &Path, index: u32) -> PathBuf {
    dir.join(format!("{index}.qcow2"))
}

/// A parent's copy of a disk, as its child's backing file names it.
fn parent_file(parent: &str, index: u32) -> String {
    format!("../{parent}/{index}.qcow2")
}

/// The checkpoint a backup starts.
fn checkpoint_name(backup: &str) -> String {
    format!("mst-{backup}")
}

/// The disk's target device in its VM's domain, which the controller names
/// for any disk of a VM that may be running.
pub(crate) fn target(d: &BackupDisk) -> Result<&str> {
    match d.target.as_str() {
        "" => bail!("no target device named for disk {}", d.index),
        t => Ok(t),
    }
}

/// Whether a disk is a qcow2 file, which keeps dirty bitmaps; Ceph disks
/// are not.
fn tracks_changes(d: &BackupDisk) -> bool {
    d.pool.as_ref().is_some_and(|p| p.kind != "rbd")
}

/// Takes a backup (or waits for, or answers for, one already under way or
/// taken).
pub async fn backup(agent: &Agent, c: &BackupVm) -> Result<String> {
    let lock = lock(&c.backup_id);
    let _held = lock.lock().await;
    let dir = backup_dir(agent, c.repository.as_ref(), &c.backup_id).await?;
    if !c.parent_id.is_empty() {
        check_id(&c.parent_id)?;
    }
    let done = dir.join("done.json");
    if let Ok(text) = tokio::fs::read_to_string(&done).await {
        return Ok(text);
    }
    // QEMU writes a running VM's backup here: it must get through.
    if let Some(parent) = dir.parent() {
        mst_storage::private_dir(parent).await?;
    }
    mst_storage::private_dir(&dir).await?;
    let (consistent, plan) = if matches!(
        agent.lv.power(&c.vm_id).await,
        Ok(Power::Running | Power::Paused | Power::Suspended)
    ) {
        live(agent, c, &dir).await?
    } else {
        (true, offline(agent, c, &dir).await?)
    };
    let (mut stored, mut incremental) = (0, false);
    for d in &c.disks {
        let file = disk_file(&dir, d.index);
        stored += tokio::fs::metadata(&file)
            .await
            .with_context(|| format!("the backup of disk {} is missing", d.index))?
            .len();
        // What the files say, not the plan: whether the backup needs its
        // parent decides whether the parent may simply be removed.
        incremental |= mst_storage::backups::backing_of(&file).await?.is_some();
    }
    let text = serde_json::to_string(&Done {
        stored_bytes: stored,
        consistent,
        incremental,
        full_reason: plan.full_reason.filter(|_| !incremental),
    })?;
    if incremental {
        mark_child(&dir.with_file_name(&c.parent_id), &c.backup_id).await?;
    }
    let tmp = dir.join("done.json.tmp");
    tokio::fs::write(&tmp, &text).await?;
    tokio::fs::rename(&tmp, &done).await?;
    tracing::info!(vm = %c.vm_id, backup = %c.backup_id, stored, incremental, "backup taken");
    Ok(text)
}

async fn write_plan(dir: &Path, plan: &Plan) -> Result<()> {
    let tmp = dir.join("plan.json.tmp");
    tokio::fs::write(&tmp, serde_json::to_vec(plan)?).await?;
    tokio::fs::rename(&tmp, dir.join("plan.json")).await?;
    Ok(())
}

/// The plan a backup was begun with (written before it began).
async fn read_plan(dir: &Path) -> Result<Plan> {
    let b = tokio::fs::read(dir.join("plan.json"))
        .await
        .context("reading the backup's plan")?;
    Ok(serde_json::from_slice(&b)?)
}

/// Why a running VM's backup is taken in full though it has a parent.
const UNTRACKED: &str = "the VM's change tracking does not reach back to its last backup \
     (it was restarted after a crash, reverted to a snapshot, or its disks were moved)";

/// Which of a running VM's disks can be stored as the changes since the
/// parent, and since which checkpoint; or why none can.
async fn plan_live(agent: &Agent, c: &BackupVm, dir: &Path) -> Plan {
    let full = |reason: &str| Plan {
        full_reason: Some(reason.to_owned()),
        ..Plan::default()
    };
    if c.parent_id.is_empty() {
        return Plan::default();
    }
    let parent_dir = dir.with_file_name(&c.parent_id);
    let mut candidates = vec![];
    for d in &c.disks {
        if let Some(i) = d.parent_index
            && tracks_changes(d)
            && tokio::fs::try_exists(disk_file(&parent_dir, i))
                .await
                .unwrap_or(false)
        {
            candidates.push(d);
        }
    }
    if candidates.is_empty() {
        return full(if c.disks.iter().all(|d| d.parent_index.is_none()) {
            "none of its disks was in its last backup"
        } else if !c.disks.iter().any(tracks_changes) {
            "its disks are on Ceph, which is copied in full"
        } else {
            "its last backup's files are missing"
        });
    }
    let Some((since, covered)) = checkpoint_since(agent, c, dir).await else {
        return full(UNTRACKED);
    };
    // The disks with the checkpoint's bitmap, intact: a disk moved, or
    // attached since, has none (or another VM's).
    let bitmaps = disk_bitmaps(agent, &c.vm_id, &c.disks)
        .await
        .unwrap_or_default();
    let incremental: Vec<u32> = candidates
        .iter()
        .filter(|d| {
            target(d).is_ok_and(|t| covered.contains(t))
                && bitmaps
                    .get(&d.index)
                    .is_some_and(|b| b.iter().any(|b| b.name == since && !b.inconsistent))
        })
        .map(|d| d.index)
        .collect();
    if incremental.is_empty() {
        return full(UNTRACKED);
    }
    Plan {
        incremental,
        since: Some(since),
        ..Plan::default()
    }
}

/// The newest checkpoint of `c.checkpoints` libvirt knows (or can be told
/// of again, from what was saved with its backup), with the disks it
/// covers.
async fn checkpoint_since(
    agent: &Agent,
    c: &BackupVm,
    dir: &Path,
) -> Option<(String, HashSet<String>)> {
    let lv = &agent.lv;
    let known = lv.checkpoints(&c.vm_id).await.ok()?;
    for id in &c.checkpoints {
        let Ok(id) = check_id(id) else { continue };
        let name = checkpoint_name(id);
        if !known.contains(&name) {
            // Libvirt here has not heard of it (the VM came from another
            // host): told of it again if the disks' bitmaps are intact.
            let saved = dir.with_file_name(id).join("checkpoint.xml");
            let Ok(xml) = tokio::fs::read_to_string(&saved).await else {
                continue;
            };
            match lv.redefine_checkpoint(&c.vm_id, xml).await {
                Ok(()) => tracing::info!(vm = %c.vm_id, checkpoint = %name, "checkpoint redefined"),
                Err(e) => {
                    tracing::info!(vm = %c.vm_id, checkpoint = %name, error = %e, "checkpoint not usable");
                    continue;
                }
            }
        }
        let xml = lv.checkpoint_xml(&c.vm_id, &name).await.ok()?;
        let covered = mst_libvirt::xml::checkpoint_disks(&xml)
            .into_iter()
            .collect();
        return Some((name, covered));
    }
    None
}

#[derive(Debug)]
struct Bitmap {
    name: String,
    inconsistent: bool,
}

/// The dirty bitmaps in a running VM's disks, by index.
async fn disk_bitmaps(
    agent: &Agent,
    vm: &str,
    disks: &[BackupDisk],
) -> Result<HashMap<u32, Vec<Bitmap>>> {
    let nodes = disk_nodes(agent, vm, disks).await?;
    Ok(nodes
        .into_iter()
        .map(|(index, (_, node))| {
            let bitmaps = node["dirty-bitmaps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|b| {
                    Some(Bitmap {
                        name: b["name"].as_str()?.to_owned(),
                        inconsistent: b["inconsistent"].as_bool().unwrap_or(false),
                    })
                })
                .collect();
            (index, bitmaps)
        })
        .collect())
}

/// Each disk's top QEMU block node (its format layer), by index: its name
/// and QEMU's description of it.
async fn disk_nodes(
    agent: &Agent,
    vm: &str,
    disks: &[BackupDisk],
) -> Result<HashMap<u32, (String, Value)>> {
    let sources: HashMap<String, u32> = agent.lv.disk_sources(vm).await?.into_iter().collect();
    let named = crate::snapshot::qmp(
        agent,
        vm,
        json!({ "execute": "query-named-block-nodes", "arguments": { "flat": true } }),
    )
    .await?;
    let mut out = HashMap::new();
    for d in disks {
        let Some(index) = sources.get(target(d)?) else {
            continue;
        };
        let name = format!("libvirt-{index}-format");
        if let Some(node) = named
            .as_array()
            .and_then(|ns| ns.iter().find(|n| n["node-name"] == name.as_str()))
        {
            out.insert(d.index, (name, node.clone()));
        }
    }
    Ok(out)
}

/// A running VM: libvirt's backup job copies its disks as they are when it
/// starts (or what changed in them since a checkpoint), while the VM
/// carries on. Whether its file systems were frozen, and how it was taken.
async fn live(agent: &Agent, c: &BackupVm, dir: &Path) -> Result<(bool, Plan)> {
    let lv = &agent.lv;
    let ours = |st: &mst_libvirt::JobStats| st.active && st.operation == Some(JOB_OPERATION_BACKUP);
    // Whether the file systems were frozen, kept for a retried request.
    let marker = dir.join("frozen");
    // A retried request finds its backup still running, and waits.
    if !lv.job_stats(&c.vm_id).await.is_ok_and(|st| ours(&st)) {
        let targets = lv.disk_targets(&c.vm_id).await?;
        for d in &c.disks {
            let t = target(d)?;
            if !targets.iter().any(|have| have == t) {
                bail!("the VM has no disk {t}");
            }
        }
        let starts = checkpoint_name(&c.backup_id);
        // Left by an attempt that did not finish.
        match tokio::fs::remove_file(&marker).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        if lv.checkpoints(&c.vm_id).await?.contains(&starts) {
            lv.delete_checkpoint(&c.vm_id, &starts, false).await?;
        }
        let planned = plan_live(agent, c, dir).await;
        // Should libvirt refuse, the backup is taken in full; then without
        // starting a checkpoint, as before there were any.
        let tracked = c.disks.iter().any(tracks_changes);
        let mut attempts = vec![];
        if !planned.incremental.is_empty() {
            attempts.push(Plan {
                starts: Some(starts.clone()),
                ..planned.clone()
            });
        }
        let in_full = Plan {
            full_reason: planned.full_reason.clone(),
            ..Plan::default()
        };
        if tracked {
            attempts.push(Plan {
                starts: Some(starts.clone()),
                ..in_full.clone()
            });
        }
        attempts.push(in_full);
        let mut refused = None;
        for mut plan in attempts {
            if let Some(why) = refused.take()
                && plan.full_reason.is_none()
                && !c.parent_id.is_empty()
            {
                plan.full_reason = Some(format!("it could not be taken incrementally: {why}"));
            }
            // Before the job: a request retried while it runs answers by it.
            write_plan(dir, &plan).await?;
            match begin(agent, c, dir, &targets, &plan).await {
                Ok(frozen) => {
                    if frozen {
                        tokio::fs::write(&marker, b"").await?;
                    } else {
                        tracing::info!(vm = %c.vm_id, "no guest agent answered: the backup is crash-consistent");
                    }
                    if let Some(name) = &plan.starts
                        && let Err(e) = save_checkpoint(agent, &c.vm_id, name, dir).await
                    {
                        tracing::warn!(vm = %c.vm_id, error = %format!("{e:#}"), "saving the checkpoint's description");
                    }
                    refused = None;
                    break;
                }
                Err(e) => {
                    tracing::warn!(vm = %c.vm_id, ?plan, error = %format!("{e:#}"), "backup not begun");
                    refused = Some(format!("{e:#}"));
                }
            }
        }
        if let Some(e) = refused {
            bail!("starting the backup: {e}");
        }
    }
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if !ours(&lv.job_stats(&c.vm_id).await?) {
            break;
        }
    }
    let plan = read_plan(dir).await?;
    match lv.completed_job(&c.vm_id).await? {
        Some(job) if job.operation == Some(JOB_OPERATION_BACKUP) && !job.ok => {
            // What it started tracks from a backup that is not there.
            if let Some(name) = &plan.starts {
                let _ = lv.delete_checkpoint(&c.vm_id, name, false).await;
            }
            bail!(
                "the backup failed: {}",
                job.error.as_deref().unwrap_or("no reason given")
            )
        }
        _ => {}
    }
    if let Some(name) = &plan.starts {
        keep_only(agent, &c.vm_id, name, &c.disks).await;
    }
    Ok((tokio::fs::try_exists(&marker).await?, plan))
}

/// Starts the backup job as `plan` says, into target files made for it;
/// whether the guest's file systems were frozen for its start.
async fn begin(
    agent: &Agent,
    c: &BackupVm,
    dir: &Path,
    targets: &[String],
    plan: &Plan,
) -> Result<bool> {
    let lv = &agent.lv;
    let mut wanted = HashMap::new();
    for d in &c.disks {
        let file = disk_file(dir, d.index);
        // Left by an attempt that failed.
        match tokio::fs::remove_file(&file).await {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        let t = target(d)?;
        let size = lv.disk_capacity(&c.vm_id, t).await?;
        let base = match (plan.incremental.contains(&d.index), d.parent_index) {
            (true, Some(i)) => Some(parent_file(&c.parent_id, i)),
            _ => None,
        };
        mst_storage::backups::create_target(&file, size, base.as_deref()).await?;
        let since = base.and(plan.since.as_deref());
        wanted.insert(t.to_owned(), (file, since.map(str::to_owned)));
    }
    let xml = backup_xml(targets, &wanted);
    let checkpoint = plan.starts.as_deref().map(|name| {
        let tracked: HashSet<String> = c
            .disks
            .iter()
            .filter(|d| tracks_changes(d))
            .filter_map(|d| target(d).ok().map(str::to_owned))
            .collect();
        checkpoint_xml(name, targets, &tracked)
    });
    // The backup is of the moment it starts: with the guest's file systems
    // frozen then (when its agent answers), they are consistent.
    let frozen = matches!(
        tokio::time::timeout(Duration::from_secs(30), lv.fs_freeze(&c.vm_id)).await,
        Ok(Ok(_))
    );
    let begun = lv.backup_begin(&c.vm_id, xml, checkpoint, true).await;
    if frozen && let Err(e) = lv.fs_thaw(&c.vm_id).await {
        tracing::warn!(vm = %c.vm_id, error = %e, "thawing the guest's file systems");
    }
    begun?;
    Ok(frozen)
}

/// Saves a checkpoint's description with the backup that started it, for
/// libvirt on another host (`checkpoint.xml`).
async fn save_checkpoint(agent: &Agent, vm: &str, name: &str, dir: &Path) -> Result<()> {
    let xml = agent.lv.checkpoint_xml(vm, name).await?;
    let tmp = dir.join("checkpoint.xml.tmp");
    tokio::fs::write(&tmp, mst_libvirt::xml::without_parent(&xml)).await?;
    tokio::fs::rename(&tmp, dir.join("checkpoint.xml")).await?;
    Ok(())
}

/// Leaves the VM only the checkpoint `keep`: older ones, and bitmaps left by
/// checkpoints libvirt no longer knows (a QEMU killed, a disk from another
/// VM), would only slow its writes.
async fn keep_only(agent: &Agent, vm: &str, keep: &str, disks: &[BackupDisk]) {
    let lv = &agent.lv;
    for name in lv.checkpoints(vm).await.unwrap_or_default() {
        if name == keep || !name.starts_with("mst-") {
            continue;
        }
        if let Err(e) = lv.delete_checkpoint(vm, &name, false).await {
            tracing::warn!(vm, checkpoint = %name, error = %e, "deleting a checkpoint; forgetting it");
            let _ = lv.delete_checkpoint(vm, &name, true).await;
        }
    }
    let Ok(nodes) = disk_nodes(agent, vm, disks).await else {
        return;
    };
    for (node, desc) in nodes.values() {
        for b in desc["dirty-bitmaps"].as_array().into_iter().flatten() {
            let Some(name) = b["name"].as_str() else {
                continue;
            };
            if name == keep || !name.starts_with("mst-") {
                continue;
            }
            let removed = crate::snapshot::qmp(
                agent,
                vm,
                json!({ "execute": "block-dirty-bitmap-remove",
                        "arguments": { "node": node, "name": name } }),
            )
            .await;
            if let Err(e) = removed {
                tracing::warn!(vm, bitmap = name, error = %format!("{e:#}"), "removing a stale dirty bitmap");
            }
        }
    }
}

/// Forgets a VM's checkpoints (libvirt's record of them; the bitmaps are
/// the disks'), for when they no longer say what changed.
pub(crate) async fn forget_checkpoints(agent: &Agent, vm: &str) -> Result<()> {
    let names = match agent.lv.checkpoints(vm).await {
        Ok(n) => n,
        Err(mst_libvirt::conn::Error::NoDomain(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for name in names {
        agent.lv.delete_checkpoint(vm, &name, true).await?;
    }
    Ok(())
}

/// The `<domainbackup>` for the disks `wanted` (each target's file, and the
/// checkpoint its changes are since, for one stored as changes), leaving
/// the domain's others out.
fn backup_xml(targets: &[String], wanted: &HashMap<String, (PathBuf, Option<String>)>) -> String {
    use mst_libvirt::xml::escape;
    let mut xml = String::from("<domainbackup mode='push'><disks>");
    for t in targets {
        match wanted.get(t) {
            Some((path, since)) => {
                let mode = since.as_deref().map_or(String::new(), |c| {
                    format!(" backupmode='incremental' incremental='{}'", escape(c))
                });
                let _ = write!(
                    xml,
                    "<disk name='{}' backup='yes' type='file'{mode}><target file='{}'/><driver type='qcow2'/></disk>",
                    escape(t),
                    escape(&path.to_string_lossy())
                );
            }
            // The cloud-init seed, and anything else not asked for.
            None => {
                let _ = write!(xml, "<disk name='{}' backup='no'/>", escape(t));
            }
        }
    }
    xml.push_str("</disks></domainbackup>");
    xml
}

/// The `<domaincheckpoint>` a backup starts: a dirty bitmap in each of
/// `tracked`, none in the domain's other disks.
fn checkpoint_xml(name: &str, targets: &[String], tracked: &HashSet<String>) -> String {
    use mst_libvirt::xml::escape;
    let mut xml = format!("<domaincheckpoint><name>{}</name><disks>", escape(name));
    for t in targets {
        let kind = if tracked.contains(t) { "bitmap" } else { "no" };
        let _ = write!(xml, "<disk name='{}' checkpoint='{kind}'/>", escape(t));
    }
    xml.push_str("</disks></domaincheckpoint>");
    xml
}

/// A stopped VM: its volumes are copied as they are, or (those a parent has,
/// in file pools) compared with the parent's copies for what changed.
async fn offline(agent: &Agent, c: &BackupVm, dir: &Path) -> Result<Plan> {
    let parent_dir = (!c.parent_id.is_empty()).then(|| dir.with_file_name(&c.parent_id));
    let mut plan = Plan::default();
    for d in &c.disks {
        let file = disk_file(dir, d.index);
        let pool = agent.pool(d.pool.as_ref()).await?;
        let base = match (&parent_dir, d.parent_index, &pool) {
            (Some(p), Some(i), mst_storage::Pool::File(_))
                if tokio::fs::try_exists(disk_file(p, i)).await? =>
            {
                Some(parent_file(&c.parent_id, i))
            }
            _ => None,
        };
        if base.is_some() {
            plan.incremental.push(d.index);
        }
        // Files appear complete or not at all: one there is done.
        if tokio::fs::try_exists(&file).await? {
            continue;
        }
        match (base, &pool) {
            (Some(base), mst_storage::Pool::File(p)) => {
                let volume = p.volume_path(&d.volume_id)?;
                let (_, size) = p.volume_info(&d.volume_id).await?;
                mst_storage::backups::store_changes(&volume, &base, &file, size)
                    .await
                    .with_context(|| format!("comparing disk {} with the last backup", d.index))?;
            }
            _ => pool
                .export_volume(&d.volume_id, &file)
                .await
                .with_context(|| format!("copying disk {}", d.index))?,
        }
    }
    if parent_dir.is_some() && plan.incremental.is_empty() {
        plan.full_reason = Some(if c.disks.iter().all(|d| d.parent_index.is_none()) {
            "none of its disks was in its last backup".into()
        } else {
            "its disks are on Ceph, which is copied in full".into()
        });
    }
    Ok(plan)
}

/// Creates a volume from one of a backup's disks (read through the backups
/// it builds on).
pub async fn restore(agent: &Agent, c: &RestoreVolume) -> Result<()> {
    let dir = backup_dir(agent, c.repository.as_ref(), &c.backup_id).await?;
    let file = disk_file(&dir, c.index);
    if !tokio::fs::try_exists(&file).await? {
        bail!("the backup has no disk {}", c.index);
    }
    check_chain(&file).await?;
    let pool = agent.pool(c.pool.as_ref()).await?;
    pool.import_volume(&c.volume_id, c.size_bytes, &file)
        .await
        .context("restoring the volume")?;
    Ok(())
}

/// Checks that a backup's file is backed only by other backups' files in
/// its repository, as metastratum makes them (a restore reads whatever
/// the files name).
async fn check_chain(file: &Path) -> Result<()> {
    let mut file = file.to_owned();
    for _ in 0..10_000 {
        let Some(backing) = mst_storage::backups::backing_of(&file).await? else {
            return Ok(());
        };
        let next = backing
            .strip_prefix("../")
            .and_then(|b| b.split_once('/'))
            .filter(|(id, n)| {
                check_id(id).is_ok()
                    && n.strip_suffix(".qcow2")
                        .is_some_and(|i| i.parse::<u32>().is_ok())
            });
        let (Some((id, n)), Some(dir)) = (next, file.parent()) else {
            bail!(
                "{} is backed by {backing:?}, not another backup",
                file.display()
            );
        };
        file = dir.with_file_name(id).join(n);
        if !tokio::fs::try_exists(&file).await? {
            bail!("{} is missing (merged away, or deleted)", backing);
        }
    }
    bail!("the backup's chain is too long")
}

/// Notes in a backup's directory that another builds on it
/// (`child-<id>`), for deleting it to check.
async fn mark_child(parent_dir: &Path, child: &str) -> Result<()> {
    if tokio::fs::try_exists(parent_dir).await? {
        tokio::fs::write(parent_dir.join(format!("child-{child}")), b"").await?;
    }
    Ok(())
}

/// Backups whose files are still backed by backup `id`'s (in `dir`), by
/// the notes they left there.
async fn built_on(dir: &Path, id: &str) -> Result<Vec<String>> {
    let mut out = vec![];
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return Ok(out);
    };
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(child) = name.strip_prefix("child-") else {
            continue;
        };
        let Ok(child) = check_id(child) else { continue };
        // Merged away since, or gone: a note left behind.
        let Ok(mut files) = tokio::fs::read_dir(dir.with_file_name(child)).await else {
            continue;
        };
        while let Some(f) = files.next_entry().await? {
            if let Ok(Some(b)) = mst_storage::backups::backing_of(&f.path()).await
                && b.starts_with(&format!("../{id}/"))
            {
                out.push(child.to_owned());
                break;
            }
        }
    }
    Ok(out)
}

/// Removes a backup's files, after merging it into the backups built on it.
/// Answers what each of those takes up afterwards.
pub async fn delete(agent: &Agent, c: &DeleteBackup) -> Result<String> {
    let lock = lock(&c.backup_id);
    let _held = lock.lock().await;
    let dir = backup_dir(agent, c.repository.as_ref(), &c.backup_id).await?;
    // Removing what others build on would break them: only merged, as a
    // controller that knows of them asks.
    if let Some(child) = built_on(&dir, &c.backup_id)
        .await?
        .into_iter()
        .find(|b| !c.children.contains(b))
    {
        bail!(
            "backup {child} builds on it: it must be merged into that one (upgrade the controllers)"
        );
    }
    let mut sizes = serde_json::Map::new();
    for child in &c.children {
        let lock = self::lock(child);
        let _held = lock.lock().await;
        let child_dir = backup_dir(agent, c.repository.as_ref(), child).await?;
        let stored = merge(&dir, &c.backup_id, &child_dir)
            .await
            .with_context(|| format!("merging into backup {child}"))?;
        sizes.insert(child.clone(), stored.into());
    }
    match tokio::fs::remove_dir_all(&dir).await {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    Ok(json!({ "children": sizes }).to_string())
}

/// Merges backup `id` (in `dir`) into a child's files that are backed by
/// its; what the child's files take up afterwards.
async fn merge(dir: &Path, id: &str, child_dir: &Path) -> Result<u64> {
    let mut stored = 0;
    let mut files = tokio::fs::read_dir(child_dir)
        .await
        .context("the backup built on it has no files")?;
    while let Some(entry) = files.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(index) = name
            .strip_suffix(".qcow2")
            .and_then(|i| i.parse::<u32>().ok())
        else {
            continue;
        };
        let file = disk_file(child_dir, index);
        if let Some(backing) = mst_storage::backups::backing_of(&file).await?
            && let Some(base) = backing
                .strip_prefix(&format!("../{id}/"))
                .and_then(|n| n.strip_suffix(".qcow2"))
                .and_then(|i| i.parse::<u32>().ok())
        {
            let base = disk_file(dir, base);
            if !tokio::fs::try_exists(&base).await? {
                bail!("{name} is backed by {backing}, which is missing");
            }
            // What the base builds on, the child builds on once merged.
            let below = mst_storage::backups::backing_of(&base).await?;
            mst_storage::backups::merge_into_child(&file, &base).await?;
            if let Some(grandparent) = below
                .as_deref()
                .and_then(|b| b.strip_prefix("../"))
                .and_then(|b| b.split('/').next())
                && let Some(child) = child_dir.file_name()
            {
                mark_child(&dir.with_file_name(grandparent), &child.to_string_lossy()).await?;
            }
        }
        stored += tokio::fs::metadata(&file).await?.len();
    }
    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_and_checkpoint_descriptions() {
        let wanted: HashMap<String, (PathBuf, Option<String>)> = [
            (
                "vda".to_owned(),
                (
                    PathBuf::from("/r/backups/b/0.qcow2"),
                    Some("mst-a".to_owned()),
                ),
            ),
            (
                "sdb".to_owned(),
                (PathBuf::from("/r/backups/b/1.qcow2"), None),
            ),
        ]
        .into();
        let targets = ["vda".to_owned(), "sda".to_owned(), "sdb".to_owned()];
        assert_eq!(
            backup_xml(&targets, &wanted),
            "<domainbackup mode='push'><disks>\
             <disk name='vda' backup='yes' type='file' backupmode='incremental' incremental='mst-a'>\
             <target file='/r/backups/b/0.qcow2'/><driver type='qcow2'/></disk>\
             <disk name='sda' backup='no'/>\
             <disk name='sdb' backup='yes' type='file'><target file='/r/backups/b/1.qcow2'/><driver type='qcow2'/></disk>\
             </disks></domainbackup>"
        );
        let tracked: HashSet<String> = ["vda".to_owned()].into();
        assert_eq!(
            checkpoint_xml("mst-b", &targets, &tracked),
            "<domaincheckpoint><name>mst-b</name><disks>\
             <disk name='vda' checkpoint='bitmap'/><disk name='sda' checkpoint='no'/>\
             <disk name='sdb' checkpoint='no'/></disks></domaincheckpoint>"
        );
        assert_eq!(parent_file("a", 2), "../a/2.qcow2");
    }

    /// Merging a deleted backup into its child: files backed by its own
    /// are merged, others left alone.
    #[tokio::test]
    async fn merging_into_a_child() {
        if tokio::process::Command::new("qemu-img")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            eprintln!("skipping: no qemu-img");
            return;
        }
        let root = std::env::temp_dir().join(format!("mst-agent-merge-{}", std::process::id()));
        let (a, b) = (root.join("aa"), root.join("bb"));
        tokio::fs::create_dir_all(&a).await.unwrap();
        tokio::fs::create_dir_all(&b).await.unwrap();
        use mst_storage::backups::{backing_of, create_target};
        create_target(&disk_file(&a, 0), 1 << 20, None)
            .await
            .unwrap();
        create_target(&disk_file(&a, 1), 1 << 20, None)
            .await
            .unwrap();
        // b's disk 0 is a's disk 1; its disk 1 is new.
        create_target(&disk_file(&b, 0), 1 << 20, Some(&parent_file("aa", 1)))
            .await
            .unwrap();
        create_target(&disk_file(&b, 1), 1 << 20, None)
            .await
            .unwrap();
        let stored = merge(&a, "aa", &b).await.unwrap();
        assert!(stored > 0);
        assert_eq!(backing_of(&disk_file(&b, 0)).await.unwrap(), None);
        assert!(!disk_file(&a, 1).exists() && disk_file(&a, 0).exists());
        // Again (a retry): nothing left to merge.
        merge(&a, "aa", &b).await.unwrap();

        // Restores read only chains of backups' files.
        check_chain(&disk_file(&b, 0)).await.unwrap();
        let odd = root.join("odd.qcow2");
        create_target(&odd, 1 << 20, None).await.unwrap();
        create_target(&disk_file(&a, 3), 1 << 20, Some("../odd.qcow2"))
            .await
            .unwrap();
        assert!(check_chain(&disk_file(&a, 3)).await.is_err());

        // A child's note holds deleting its parent off while the child's
        // files are backed by the parent's, and not once merged.
        let c = root.join("cc");
        tokio::fs::create_dir_all(&c).await.unwrap();
        create_target(&disk_file(&c, 0), 1 << 20, Some(&parent_file("bb", 0)))
            .await
            .unwrap();
        mark_child(&b, "cc").await.unwrap();
        mark_child(&b, "dd").await.unwrap();
        assert_eq!(built_on(&b, "bb").await.unwrap(), ["cc"]);
        merge(&b, "bb", &c).await.unwrap();
        assert!(built_on(&b, "bb").await.unwrap().is_empty());
        tokio::fs::remove_dir_all(&root).await.unwrap();
    }
}
