//! Snapshots of VMs' disks, kept in the disks themselves under the names
//! `mst_storage::snapshot_name` gives them.
//!
//! A running VM's disks are snapshotted by its QEMU, all in one QMP
//! transaction so they match (qcow2 internal snapshots; for Ceph disks
//! QEMU takes RBD snapshots), with the guest's file systems frozen if its
//! agent answers. A stopped VM's volumes are snapshotted directly.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use mst_libvirt::Power;
use mst_proto::{BackupDisk, DeleteSnapshot, RevertSnapshot, SnapshotVm};
use serde_json::{Value, json};

use crate::backup::target;
use crate::session::Agent;

/// One snapshot request at a time per VM.
static LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Mutex::default);

fn lock(vm: &str) -> Arc<tokio::sync::Mutex<()>> {
    LOCKS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(vm.to_owned())
        .or_default()
        .clone()
}

async fn running(agent: &Agent, vm: &str) -> bool {
    matches!(agent.lv.power(vm).await, Ok(Power::Running | Power::Paused))
}

/// The VM's disks asked for, each with its pool.
async fn volumes(
    agent: &Agent,
    disks: &[BackupDisk],
) -> Result<Vec<(u32, String, mst_storage::Pool)>> {
    let mut out = vec![];
    for d in disks {
        out.push((
            d.index,
            d.volume_id.clone(),
            agent.pool(d.pool.as_ref()).await?,
        ));
    }
    Ok(out)
}

/// The QEMU block nodes of a running VM's disks, by index: each disk's top
/// node, its format layer or, for raw disks without one, its storage.
async fn nodes(agent: &Agent, vm: &str, disks: &[BackupDisk]) -> Result<HashMap<u32, String>> {
    let sources: HashMap<String, u32> = agent.lv.disk_sources(vm).await?.into_iter().collect();
    let named = qmp(
        agent,
        vm,
        json!({ "execute": "query-named-block-nodes", "arguments": { "flat": true } }),
    )
    .await?;
    let exists = |name: &str| {
        named
            .as_array()
            .is_some_and(|ns| ns.iter().any(|n| n["node-name"] == name))
    };
    disks
        .iter()
        .map(|d| {
            let target = target(d);
            let index = sources
                .get(&target)
                .ok_or_else(|| anyhow!("the VM has no disk {target}"))?;
            ["format", "storage"]
                .iter()
                .map(|layer| format!("libvirt-{index}-{layer}"))
                .find(|n| exists(n))
                .map(|n| (d.index, n))
                .ok_or_else(|| anyhow!("QEMU has no block node for disk {target}"))
        })
        .collect()
}

/// Sends a QMP command to the VM's QEMU; its `return`, or its error.
async fn qmp(agent: &Agent, vm: &str, cmd: Value) -> Result<Value> {
    let out = agent.lv.qemu_monitor(vm, &cmd.to_string()).await?;
    let mut reply: Value = serde_json::from_str(&out).context("QEMU's reply")?;
    if let Some(e) = reply.get("error") {
        bail!(
            "QEMU: {}",
            e.get("desc").and_then(Value::as_str).unwrap_or("failed")
        );
    }
    Ok(reply["return"].take())
}

/// Takes a snapshot; answers `{"consistent": ...}`, null for one already
/// taken (a retried request).
pub async fn snapshot(agent: &Agent, c: &SnapshotVm) -> Result<String> {
    let lock = lock(&c.vm_id);
    let _held = lock.lock().await;
    let name = mst_storage::snapshot_name(&c.snapshot_id)?;
    let vols = volumes(agent, &c.disks).await?;
    let consistent = if running(agent, &c.vm_id).await {
        let mut taken = true;
        for (_, vol, pool) in &vols {
            taken &= pool.snapshots(vol).await?.contains(&name);
        }
        if taken {
            None
        } else {
            Some(live(agent, c, &name).await?)
        }
    } else {
        for (_, vol, pool) in &vols {
            pool.create_snapshot(vol, &c.snapshot_id).await?;
        }
        Some(true)
    };
    tracing::info!(vm = %c.vm_id, snapshot = %c.snapshot_id, ?consistent, "snapshot taken");
    Ok(json!({ "consistent": consistent }).to_string())
}

/// A running VM's snapshot, through its QEMU. Whether its file systems
/// were frozen for it.
async fn live(agent: &Agent, c: &SnapshotVm, name: &str) -> Result<bool> {
    let lv = &agent.lv;
    let nodes = nodes(agent, &c.vm_id, &c.disks).await?;
    let actions: Vec<Value> = c
        .disks
        .iter()
        .map(|d| {
            json!({
                "type": "blockdev-snapshot-internal-sync",
                "data": { "device": nodes[&d.index], "name": name },
            })
        })
        .collect();
    let frozen = matches!(
        tokio::time::timeout(Duration::from_secs(30), lv.fs_freeze(&c.vm_id)).await,
        Ok(Ok(_))
    );
    let taken = qmp(
        agent,
        &c.vm_id,
        json!({ "execute": "transaction", "arguments": { "actions": actions } }),
    )
    .await;
    if frozen && let Err(e) = lv.fs_thaw(&c.vm_id).await {
        tracing::warn!(vm = %c.vm_id, error = %e, "thawing the guest's file systems");
    }
    taken.context("taking the snapshot")?;
    if !frozen {
        tracing::info!(vm = %c.vm_id, "no guest agent answered: the snapshot is crash-consistent");
    }
    Ok(frozen)
}

/// Puts a stopped VM's disks back as they were in a snapshot.
pub async fn revert(agent: &Agent, c: &RevertSnapshot) -> Result<()> {
    let lock = lock(&c.vm_id);
    let _held = lock.lock().await;
    if running(agent, &c.vm_id).await {
        bail!("the VM is running: stop it to revert it to a snapshot");
    }
    for (index, vol, pool) in volumes(agent, &c.disks).await? {
        pool.revert_snapshot(&vol, &c.snapshot_id)
            .await
            .with_context(|| format!("reverting disk {index}"))?;
    }
    tracing::info!(vm = %c.vm_id, snapshot = %c.snapshot_id, "reverted to snapshot");
    Ok(())
}

/// Removes a snapshot from a VM's disks, running or not.
pub async fn delete(agent: &Agent, c: &DeleteSnapshot) -> Result<()> {
    let lock = lock(&c.vm_id);
    let _held = lock.lock().await;
    let name = mst_storage::snapshot_name(&c.snapshot_id)?;
    let vols = volumes(agent, &c.disks).await?;
    if running(agent, &c.vm_id).await {
        let nodes = nodes(agent, &c.vm_id, &c.disks).await?;
        for (index, vol, pool) in &vols {
            if !pool.snapshots(vol).await?.contains(&name) {
                continue;
            }
            qmp(
                agent,
                &c.vm_id,
                json!({
                    "execute": "blockdev-snapshot-delete-internal-sync",
                    "arguments": { "device": nodes[index], "name": name },
                }),
            )
            .await
            .with_context(|| format!("deleting the snapshot of disk {index}"))?;
        }
    } else {
        for (index, vol, pool) in &vols {
            pool.delete_snapshot(vol, &c.snapshot_id)
                .await
                .with_context(|| format!("deleting the snapshot of disk {index}"))?;
        }
    }
    Ok(())
}
