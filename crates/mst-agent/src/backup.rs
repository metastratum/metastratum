//! Backups: VMs' disks copied into backup repositories, and back.
//!
//! A repository is an NFS pool the host mounts; a backup is a directory in
//! it, `backups/<id>/`, with a qcow2 file per disk (`<index>.qcow2`, vda
//! being 0) and `done.json` once it is complete.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mst_libvirt::{JOB_OPERATION_BACKUP, Power};
use mst_proto::{BackupVm, DeleteBackup, RestoreVolume};
use serde::{Deserialize, Serialize};

use crate::session::Agent;

/// What `done.json` holds, and the command answers.
#[derive(Serialize, Deserialize)]
struct Done {
    stored_bytes: u64,
    /// The VM was stopped, or its file systems frozen for the start.
    consistent: bool,
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

async fn backup_dir(
    agent: &Agent,
    repository: Option<&mst_proto::Pool>,
    id: &str,
) -> Result<PathBuf> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        bail!("bad backup id {id:?}");
    }
    let mst_storage::Pool::File(pool) = agent.pool(repository).await? else {
        bail!("a backup repository must be a file system (NFS)");
    };
    Ok(pool.root().join("backups").join(id))
}

fn disk_file(dir: &Path, index: u32) -> PathBuf {
    dir.join(format!("{index}.qcow2"))
}

fn target_of(index: u32) -> String {
    // As the VM's domain names its disks: vda, vdb, ...
    format!("vd{}", (b'a' + index as u8) as char)
}

/// Takes a backup (or waits for, or answers for, one already under way or
/// taken).
pub async fn backup(agent: &Agent, c: &BackupVm) -> Result<String> {
    let lock = lock(&c.backup_id);
    let _held = lock.lock().await;
    let dir = backup_dir(agent, c.repository.as_ref(), &c.backup_id).await?;
    let done = dir.join("done.json");
    if let Ok(text) = tokio::fs::read_to_string(&done).await {
        return Ok(text);
    }
    tokio::fs::create_dir_all(&dir).await?;
    let consistent = if matches!(
        agent.lv.power(&c.vm_id).await,
        Ok(Power::Running | Power::Paused)
    ) {
        live(agent, c, &dir).await?
    } else {
        offline(agent, c, &dir).await?;
        true
    };
    let mut stored = 0;
    for d in &c.disks {
        stored += tokio::fs::metadata(disk_file(&dir, d.index))
            .await
            .with_context(|| format!("the backup of disk {} is missing", d.index))?
            .len();
    }
    let text = serde_json::to_string(&Done {
        stored_bytes: stored,
        consistent,
    })?;
    let tmp = dir.join("done.json.tmp");
    tokio::fs::write(&tmp, &text).await?;
    tokio::fs::rename(&tmp, &done).await?;
    tracing::info!(vm = %c.vm_id, backup = %c.backup_id, stored, "backup taken");
    Ok(text)
}

/// A running VM: libvirt's backup job copies its disks as they are when it
/// starts, while the VM carries on.
async fn live(agent: &Agent, c: &BackupVm, dir: &Path) -> Result<bool> {
    let lv = &agent.lv;
    let ours = |st: &mst_libvirt::JobStats| st.active && st.operation == Some(JOB_OPERATION_BACKUP);
    // Whether the file systems were frozen, kept for a retried request.
    let marker = dir.join("frozen");
    // A retried request finds its backup still running, and waits.
    if !lv.job_stats(&c.vm_id).await.is_ok_and(|st| ours(&st)) {
        let wanted: HashMap<String, PathBuf> = c
            .disks
            .iter()
            .map(|d| (target_of(d.index), disk_file(dir, d.index)))
            .collect();
        let targets = lv.disk_targets(&c.vm_id).await?;
        if let Some(missing) = wanted.keys().find(|t| !targets.contains(t)) {
            bail!("the VM has no disk {missing}");
        }
        for path in wanted.values() {
            // Left by an attempt that failed; libvirt makes its own.
            match tokio::fs::remove_file(path).await {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
        }
        let xml = backup_xml(&targets, &wanted);
        // The backup is of the moment it starts: with the guest's file
        // systems frozen then (when its agent answers), they are consistent.
        let frozen = matches!(
            tokio::time::timeout(Duration::from_secs(30), lv.fs_freeze(&c.vm_id)).await,
            Ok(Ok(_))
        );
        let begun = lv.backup_begin(&c.vm_id, xml).await;
        if frozen && let Err(e) = lv.fs_thaw(&c.vm_id).await {
            tracing::warn!(vm = %c.vm_id, error = %e, "thawing the guest's file systems");
        }
        begun.context("starting the backup")?;
        if frozen {
            tokio::fs::write(&marker, b"").await?;
        } else {
            tracing::info!(vm = %c.vm_id, "no guest agent answered: the backup is crash-consistent");
        }
    }
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if !ours(&lv.job_stats(&c.vm_id).await?) {
            break;
        }
    }
    match lv.completed_job(&c.vm_id).await? {
        Some(job) if job.operation == Some(JOB_OPERATION_BACKUP) && !job.ok => {
            bail!(
                "the backup failed: {}",
                job.error.as_deref().unwrap_or("no reason given")
            )
        }
        _ => Ok(tokio::fs::try_exists(&marker).await?),
    }
}

fn backup_xml(targets: &[String], wanted: &HashMap<String, PathBuf>) -> String {
    let mut xml = String::from("<domainbackup mode='push'><disks>");
    for t in targets {
        match wanted.get(t) {
            Some(path) => {
                let _ = write!(
                    xml,
                    "<disk name='{}' backup='yes' type='file'><target file='{}'/><driver type='qcow2'/></disk>",
                    mst_libvirt::xml::escape(t),
                    mst_libvirt::xml::escape(&path.to_string_lossy())
                );
            }
            // The cloud-init seed, and anything else not asked for.
            None => {
                let _ = write!(
                    xml,
                    "<disk name='{}' backup='no'/>",
                    mst_libvirt::xml::escape(t)
                );
            }
        }
    }
    xml.push_str("</disks></domainbackup>");
    xml
}

/// A stopped VM: its volumes are copied as they are.
async fn offline(agent: &Agent, c: &BackupVm, dir: &Path) -> Result<()> {
    for d in &c.disks {
        let file = disk_file(dir, d.index);
        // Files appear complete or not at all: one there is done.
        if tokio::fs::try_exists(&file).await? {
            continue;
        }
        let pool = agent.pool(d.pool.as_ref()).await?;
        pool.export_volume(&d.volume_id, &file)
            .await
            .with_context(|| format!("copying disk {}", d.index))?;
    }
    Ok(())
}

/// Creates a volume from one of a backup's disks.
pub async fn restore(agent: &Agent, c: &RestoreVolume) -> Result<()> {
    let dir = backup_dir(agent, c.repository.as_ref(), &c.backup_id).await?;
    let file = disk_file(&dir, c.index);
    if !tokio::fs::try_exists(&file).await? {
        bail!("the backup has no disk {}", c.index);
    }
    let pool = agent.pool(c.pool.as_ref()).await?;
    pool.import_volume(&c.volume_id, c.size_bytes, &file)
        .await
        .context("restoring the volume")?;
    Ok(())
}

pub async fn delete(agent: &Agent, c: &DeleteBackup) -> Result<()> {
    let lock = lock(&c.backup_id);
    let _held = lock.lock().await;
    let dir = backup_dir(agent, c.repository.as_ref(), &c.backup_id).await?;
    match tokio::fs::remove_dir_all(&dir).await {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_xml_names_every_disk() {
        let wanted: HashMap<String, PathBuf> =
            [("vda".to_owned(), PathBuf::from("/r/backups/b/0.qcow2"))].into();
        let xml = backup_xml(&["vda".into(), "sda".into()], &wanted);
        assert_eq!(
            xml,
            "<domainbackup mode='push'><disks>\
             <disk name='vda' backup='yes' type='file'><target file='/r/backups/b/0.qcow2'/><driver type='qcow2'/></disk>\
             <disk name='sda' backup='no'/>\
             </disks></domainbackup>"
        );
        assert_eq!(target_of(1), "vdb");
    }
}
