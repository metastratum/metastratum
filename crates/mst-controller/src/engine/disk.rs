//! Disk tasks: attaching a disk to a VM and detaching it (the API records
//! the intent; these carry it out on the VM's host, and reconcile with what
//! the VM has should that fail), and moving disks attached to no VM.
//!
//! A move goes to another host (local storage) or another pool. One host
//! that reaches both copies the disk alone; otherwise the host it is on
//! serves it (over TLS, with credentials made for this copy alone) to one
//! that reaches where it goes. A copy always replaces what is at the
//! destination, never adopts it. Once copied, the disk is recorded where it
//! went, and its old copy as stale, in one transaction; stale copies are
//! removed then, and retried in the background (`sweep`). A move asked for
//! again (a resumed task) continues from where the disk is.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use mst_core::ids::{HostId, StoragePoolId, VmId, VolumeId};
use mst_db::disks::Disk;
use mst_db::hosts::Host;
use mst_db::storage_pools::Pool;
use mst_proto::command::Cmd;
use mst_proto::{
    AttachDisk, CopyVolume, DeleteVolume, DetachDisk, EndExport, ExportVolume, MigrationTls,
};
use uuid::Uuid;

use super::TaskCtx;
use super::backup::send;
use crate::state::AppState;

/// Copying a large disk takes a while.
const COPY: Duration = Duration::from_secs(12 * 3600);
const QUICK: Duration = Duration::from_secs(60);
/// Unplugging waits for the guest to let go (up to 30 seconds).
const PLUG: Duration = Duration::from_secs(90);
/// How often stale copies are tried again.
const SWEEP_EVERY: Duration = Duration::from_secs(300);

fn target(ctx: &TaskCtx) -> Result<VolumeId> {
    ctx.task
        .target_id
        .map(VolumeId)
        .context("task has no target disk")
}

fn param<T: serde::de::DeserializeOwned>(ctx: &TaskCtx, key: &str) -> Result<Option<T>> {
    match ctx.task.params.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(v) => Ok(Some(serde_json::from_value(v.clone())?)),
    }
}

/// The live disks of the VM on `host` (target, path), if it runs there.
async fn live_disks(
    state: &AppState,
    host: HostId,
    vm: VmId,
) -> Result<Option<Vec<(String, String)>>> {
    let cmd = Cmd::DomainInfo(mst_proto::DomainInfo {
        vm_id: vm.to_string(),
    });
    let info: serde_json::Value = serde_json::from_str(&send(state, host, cmd, QUICK).await?)?;
    if info["power"] == "stopped" || info["power"] == "missing" {
        return Ok(None);
    }
    Ok(Some(
        info["disks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| {
                Some((
                    d["target"].as_str()?.to_owned(),
                    d["path"].as_str()?.to_owned(),
                ))
            })
            .collect(),
    ))
}

pub async fn attach(ctx: &TaskCtx) -> Result<()> {
    let db = &ctx.state.db;
    let id = target(ctx)?;
    let vm_id: VmId = param(ctx, "vm")?.context("no VM to attach the disk to")?;
    let disk = mst_db::disks::get(db, id).await?;
    if disk.state != "attaching" || disk.vm_id != Some(vm_id) {
        // Done already (a resumed task).
        return Ok(());
    }
    let vm = mst_db::vms::get(db, vm_id).await?;
    let host = vm.host_id.context("the VM is on no host")?;
    let index = vm
        .spec
        .0
        .disks
        .iter()
        .position(|d| d.volume_id == id)
        .context("the VM's definition lacks the disk")?;
    let slot = vm.spec.0.disk_slots().swap_remove(index);
    let mut conn = db.acquire().await?;
    let define =
        crate::api::vm_definition(&ctx.state, &mut conn, &vm, host, &vm.spec.0, vm.generation)
            .await
            .map_err(|e| anyhow!("{}", e.message))?;
    drop(conn);
    let cmd = Cmd::AttachDisk(AttachDisk {
        vm: Some(define),
        target: slot.target.clone(),
        volume_id: id.to_string(),
    });
    match send(&ctx.state, host, cmd, PLUG).await {
        Ok(_) => {
            mst_db::disks::set_state(db, id, "ready", None).await?;
            Ok(())
        }
        Err(e) => {
            // Plugged in after all (the answer was lost)?
            let live = live_disks(&ctx.state, host, vm_id).await.ok().flatten();
            let there = live.is_some_and(|l| {
                l.iter()
                    .any(|(t, p)| *t == slot.target && p.contains(&id.to_string()))
            });
            if there {
                mst_db::disks::set_state(db, id, "ready", None).await?;
                return Ok(());
            }
            // Not attached: the VM's definition goes back to without it.
            let mut tx = db.begin().await?;
            let vm = mst_db::vms::get_for_update(&mut tx, vm_id).await?;
            let mut spec = vm.spec.0.clone();
            spec.disks.retain(|d| d.volume_id != id);
            let generation = mst_db::vms::redefine(&mut tx, vm_id, &spec).await?;
            mst_db::disks::set_vm(&mut *tx, id, None).await?;
            let message = format!("attaching it failed: {e:#}");
            mst_db::disks::set_state(&mut *tx, id, "ready", Some(&message)).await?;
            tx.commit().await?;
            let mut conn = db.acquire().await?;
            if let Err(e) =
                crate::api::define_on_host(&ctx.state, &mut conn, &vm, host, &spec, generation)
                    .await
            {
                tracing::warn!(vm = %vm_id, error = %e.message, "defining the VM without a disk that failed to attach");
            }
            Err(e)
        }
    }
}

pub async fn detach(ctx: &TaskCtx) -> Result<()> {
    let db = &ctx.state.db;
    let id = target(ctx)?;
    let vm_id: VmId = param(ctx, "vm")?.context("no VM to detach the disk from")?;
    let disk = mst_db::disks::get(db, id).await?;
    if disk.state != "detaching" || disk.vm_id != Some(vm_id) {
        return Ok(());
    }
    let vm = mst_db::vms::get(db, vm_id).await?;
    let host = vm.host_id.context("the VM is on no host")?;
    let mut spec = vm.spec.0.clone();
    let index = spec
        .disks
        .iter()
        .position(|d| d.volume_id == id)
        .context("the VM's definition lacks the disk")?;
    let slot = spec.disk_slots().swap_remove(index);
    spec.disks.remove(index);
    let mut conn = db.acquire().await?;
    let define =
        crate::api::vm_definition(&ctx.state, &mut conn, &vm, host, &spec, vm.generation + 1)
            .await
            .map_err(|e| anyhow!("{}", e.message))?;
    drop(conn);
    let cmd = Cmd::DetachDisk(DetachDisk {
        vm: Some(define),
        target: slot.target.clone(),
        volume_id: id.to_string(),
    });
    let outcome = send(&ctx.state, host, cmd, PLUG).await;
    if let Err(e) = &outcome {
        // Unplugged after all (the answer was lost)? Only a running VM
        // tells; a stopped one keeps the disk until defined without it.
        let live = live_disks(&ctx.state, host, vm_id).await.ok().flatten();
        let gone = live.is_some_and(|l| !l.iter().any(|(_, p)| p.contains(&id.to_string())));
        if !gone {
            let message = format!("detaching it failed: {e:#}");
            mst_db::disks::set_state(db, id, "ready", Some(&message)).await?;
            return Err(anyhow!("{e:#}"));
        }
    }
    let mut tx = db.begin().await?;
    let vm = mst_db::vms::get_for_update(&mut tx, vm_id).await?;
    let mut spec = vm.spec.0.clone();
    spec.disks.retain(|d| d.volume_id != id);
    let generation = mst_db::vms::redefine(&mut tx, vm_id, &spec).await?;
    mst_db::disks::set_vm(&mut *tx, id, None).await?;
    mst_db::disks::set_state(&mut *tx, id, "ready", None).await?;
    tx.commit().await?;
    if outcome.is_err() {
        // Unplugged, but its definition may still have it.
        let mut conn = db.acquire().await?;
        if let Err(e) =
            crate::api::define_on_host(&ctx.state, &mut conn, &vm, host, &spec, generation).await
        {
            tracing::warn!(vm = %vm_id, error = %e.message, "defining the VM without a detached disk");
        }
    }
    Ok(())
}

pub async fn move_disk(ctx: &TaskCtx) -> Result<()> {
    let db = &ctx.state.db;
    let id = target(ctx)?;
    let pool: StoragePoolId = param(ctx, "pool")?.context("no pool to move the disk to")?;
    let host: Option<HostId> = param(ctx, "host")?;
    let from_pool: StoragePoolId =
        param(ctx, "from_pool")?.context("no pool the disk moves from")?;
    let from_host: Option<HostId> = param(ctx, "from_host")?;
    let attach_to: Option<VmId> = param(ctx, "vm")?;
    let disk = mst_db::disks::get(db, id).await?;
    if disk.state != "moving" {
        // Done already (a resumed task).
        return Ok(());
    }
    let dst_pool = mst_db::storage_pools::get(db, pool).await?;
    let dst_host = host.filter(|_| dst_pool.kind == "local");
    let stale = if disk.pool_id == pool && disk.host_id == dst_host {
        // There already: a resumed task, after it was recorded there.
        None
    } else if disk.pool_id == from_pool && disk.host_id == from_host {
        match copy(ctx, &disk, &dst_pool, dst_host).await {
            Ok(stale) => Some(stale),
            Err(e) => {
                let message = format!("moving it failed: {e:#}");
                mst_db::disks::set_state(db, id, "ready", Some(&message)).await?;
                return Err(e);
            }
        }
    } else {
        mst_db::disks::set_state(db, id, "ready", Some("it was moved elsewhere meanwhile")).await?;
        bail!("the disk was moved elsewhere meanwhile");
    };
    if let Some(stale) = stale {
        ctx.step("removing the old copy", 0.9).await;
        if let Err(e) = remove_stale(&ctx.state, stale).await {
            tracing::info!(disk = %id, error = %format!("{e:#}"), "removing a moved disk's old copy; retried later");
        }
    }
    if let Some(vm) = attach_to {
        ctx.step("attaching", 0.95).await;
        let mut tx = db.begin().await?;
        let disk = mst_db::disks::get_for_update(&mut tx, id).await?;
        crate::api::begin_attach(&ctx.state, &mut tx, vm, &disk, ctx.task.created_by)
            .await
            .map_err(|e| anyhow!("moved, but attaching it failed: {}", e.message))?;
        tx.commit().await?;
    }
    Ok(())
}

/// Whether the host reaches the volume: a pool attached to its cluster, or
/// local storage on the host itself.
fn reaches(h: &Host, pool: &Pool, local_host: Option<HostId>) -> bool {
    if pool.kind == "local" {
        local_host == Some(h.id)
    } else {
        h.cluster_id.is_some_and(|c| pool.clusters.contains(&c))
    }
}

/// Hosts that can do work: connected, not fenced or decommissioned.
async fn working_hosts(state: &AppState) -> Result<Vec<Host>> {
    Ok(mst_db::hosts::list(&state.db)
        .await?
        .into_iter()
        .filter(|h| h.is_live() && !matches!(h.state.as_str(), "fenced" | "decommissioned"))
        .collect())
}

/// Copies the disk where it goes, records it there, and returns the record
/// of its old copy (stale now).
async fn copy(
    ctx: &TaskCtx,
    disk: &Disk,
    dst_pool: &Pool,
    dst_host: Option<HostId>,
) -> Result<Uuid> {
    let db = &ctx.state.db;
    let state = &ctx.state;
    let src_pool = mst_db::storage_pools::get(db, disk.pool_id).await?;
    let src_host = disk.host_id.filter(|_| src_pool.kind == "local");
    if src_pool.id == dst_pool.id && src_host == dst_host {
        bail!("the disk is there already");
    }
    let hosts = working_hosts(state).await?;
    let src = crate::api::agent_pool(&src_pool)?;
    let dst = crate::api::agent_pool(dst_pool)?;
    let volume_id = disk.id.to_string();
    let size_bytes = disk.size_bytes as u64;

    let copied = match hosts
        .iter()
        .find(|h| reaches(h, &src_pool, src_host) && reaches(h, dst_pool, dst_host))
    {
        Some(h) => {
            ctx.step(&format!("copying on {}", h.name), 0.2).await;
            let cmd = CopyVolume {
                volume_id: volume_id.clone(),
                size_bytes,
                pool: Some(dst.clone()),
                source_pool: Some(src.clone()),
                ..Default::default()
            };
            send(state, h.id, Cmd::CopyVolume(cmd), COPY)
                .await
                .context("copying the disk")
                .map(|_| ())
        }
        None => {
            let server = hosts
                .iter()
                .find(|h| reaches(h, &src_pool, src_host))
                .with_context(|| format!("no connected host has the disk ({})", src_pool.name))?;
            if src_pool.kind == "rbd" {
                bail!(
                    "a disk on Ceph moves through a host that reaches both its pool and where it goes; none does"
                );
            }
            let client = hosts
                .iter()
                .find(|h| reaches(h, dst_pool, dst_host))
                .with_context(|| format!("no connected host reaches {}", dst_pool.name))?;
            // The host that has it serves it to the one that copies it, and
            // to no other: credentials for this copy alone.
            let tls = mst_pki::MigrationCredentials::generate(
                &[server.address.clone(), server.name.clone()],
                &client.id.to_string(),
                COPY,
            )?;
            ctx.step(&format!("serving it from {}", server.name), 0.1)
                .await;
            let served = send(
                state,
                server.id,
                Cmd::ExportVolume(ExportVolume {
                    volume_id: volume_id.clone(),
                    pool: Some(src.clone()),
                    tls: Some(MigrationTls {
                        ca_pem: tls.ca_pem.clone(),
                        cert_pem: tls.server_cert_pem.clone(),
                        key_pem: tls.server_key_pem.clone(),
                    }),
                    address: server.address.clone(),
                }),
                QUICK,
            )
            .await
            .context("serving the disk for the copy")?;
            let copied = async {
                let port = serde_json::from_str::<serde_json::Value>(&served)?["port"]
                    .as_u64()
                    .context("no port the disk is served on")?;
                ctx.step(&format!("copying to {}", client.name), 0.2).await;
                send(
                    state,
                    client.id,
                    Cmd::CopyVolume(CopyVolume {
                        volume_id: volume_id.clone(),
                        size_bytes,
                        pool: Some(dst.clone()),
                        source_pool: None,
                        nbd_address: server.address.clone(),
                        nbd_port: port as u32,
                        tls: Some(MigrationTls {
                            ca_pem: tls.ca_pem.clone(),
                            cert_pem: tls.client_cert_pem.clone(),
                            key_pem: tls.client_key_pem.clone(),
                        }),
                    }),
                    COPY,
                )
                .await
                .context("copying the disk")
            }
            .await;
            let end = Cmd::EndExport(EndExport {
                volume_id: volume_id.clone(),
            });
            let _ = send(state, server.id, end, QUICK).await;
            copied.map(|_| ())
        }
    };
    if let Err(e) = copied {
        // What got copied, if anything, goes (in the background, should
        // the host not answer now). Never the disk itself: it is not there.
        let stale = mst_db::stale::add(db, disk.id, dst_pool.id, dst_host).await?;
        if let Err(e) = remove_stale(state, stale).await {
            tracing::info!(disk = %disk.id, error = %format!("{e:#}"), "removing a failed move's copy; retried later");
        }
        return Err(e);
    }

    // Where it goes must still take it: a host taken out of service
    // meanwhile would not keep it.
    if let Some(h) = dst_host {
        let h = mst_db::hosts::get(db, h).await?;
        if matches!(h.state.as_str(), "decommissioned" | "fenced") {
            let stale = mst_db::stale::add(db, disk.id, dst_pool.id, dst_host).await?;
            let _ = remove_stale(state, stale).await;
            bail!("{} was taken out of service meanwhile", h.name);
        }
    }
    let mut tx = db.begin().await?;
    mst_db::disks::moved(&mut *tx, disk.id, dst_pool.id, dst_host).await?;
    let stale = mst_db::stale::add(&mut *tx, disk.id, src_pool.id, src_host).await?;
    mst_db::disks::set_state(&mut *tx, disk.id, "ready", None).await?;
    tx.commit().await?;
    tracing::info!(disk = %disk.id, pool = %dst_pool.name, host = ?dst_host, "disk moved");
    Ok(stale)
}

/// Removes a stale copy, unless the disk is there after all (moved back
/// meanwhile) or moving (then later). Done under the disk's lock, so no
/// move starts meanwhile.
pub async fn remove_stale(state: &AppState, id: Uuid) -> Result<()> {
    let db = &state.db;
    let Some(stale) = mst_db::stale::get(db, id).await? else {
        return Ok(());
    };
    let mut tx = db.begin().await?;
    match mst_db::disks::get_for_update(&mut tx, stale.volume_id).await {
        Ok(d) if d.state == "moving" => return Ok(()),
        Ok(d) if d.pool_id == stale.pool_id && d.host_id == stale.host_id => {
            mst_db::stale::done(&mut *tx, id).await?;
            tx.commit().await?;
            return Ok(());
        }
        Ok(_) | Err(mst_db::Error::Domain(mst_core::Error::NotFound { .. })) => {}
        Err(e) => return Err(e.into()),
    }
    let pool = mst_db::storage_pools::get(&mut *tx, stale.pool_id).await?;
    let host = match stale.host_id {
        Some(h) => {
            let host = mst_db::hosts::get(&mut *tx, h).await?;
            if host.state == "decommissioned" {
                // Wiped with the host.
                mst_db::stale::done(&mut *tx, id).await?;
                tx.commit().await?;
                return Ok(());
            }
            h
        }
        None => working_hosts(state)
            .await?
            .into_iter()
            .find(|h| reaches(h, &pool, None))
            .map(|h| h.id)
            .with_context(|| format!("no connected host reaches {}", pool.name))?,
    };
    let cmd = Cmd::DeleteVolume(DeleteVolume {
        volume_id: stale.volume_id.to_string(),
        pool: Some(crate::api::agent_pool(&pool)?),
    });
    match send(state, host, cmd, QUICK).await {
        Ok(_) => {
            mst_db::stale::done(&mut *tx, id).await?;
            tx.commit().await?;
            Ok(())
        }
        Err(e) => {
            drop(tx);
            mst_db::stale::failed(db, id, &format!("{e:#}")).await?;
            Err(e)
        }
    }
}

/// Removes stale copies of disks in the background (on the leader).
pub async fn sweep(state: AppState) {
    loop {
        tokio::time::sleep(SWEEP_EVERY).await;
        let Ok(all) = mst_db::stale::all(&state.db).await else {
            continue;
        };
        for stale in all {
            if let Err(e) = remove_stale(&state, stale.id).await {
                tracing::debug!(volume = %stale.volume_id, error = %format!("{e:#}"), "removing a stale copy of a disk");
            }
        }
    }
}
