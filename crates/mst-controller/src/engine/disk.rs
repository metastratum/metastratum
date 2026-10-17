//! Moving disks attached to no VM: to another host (local storage) or
//! another pool. One host that reaches both copies it alone; otherwise the
//! host it is on serves it (over TLS, with credentials made for this copy
//! alone) to one that reaches where it goes. The copy carries its image's
//! data, so it depends on nothing there. Once copied, it is recorded where
//! it went and the old copy is removed.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use mst_core::ids::{HostId, StoragePoolId, VmId, VolumeId};
use mst_db::disks::Disk;
use mst_db::hosts::Host;
use mst_db::storage_pools::Pool;
use mst_proto::command::Cmd;
use mst_proto::{CopyVolume, DeleteVolume, EndExport, ExportVolume, MigrationTls};

use super::TaskCtx;
use super::backup::send;

/// Copying a large disk takes a while.
const COPY: Duration = Duration::from_secs(12 * 3600);
const QUICK: Duration = Duration::from_secs(60);

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

pub async fn move_disk(ctx: &TaskCtx) -> Result<()> {
    let db = &ctx.state.db;
    let id = target(ctx)?;
    let pool: StoragePoolId = param(ctx, "pool")?.context("no pool to move the disk to")?;
    let host: Option<HostId> = param(ctx, "host")?;
    let attach_to: Option<VmId> = param(ctx, "vm")?;
    let disk = mst_db::disks::get(db, id).await?;
    if let Err(e) = copy(ctx, &disk, pool, host).await {
        let message = format!("moving it failed: {e:#}");
        mst_db::disks::set_state(db, id, "ready", Some(&message)).await?;
        return Err(e);
    }
    mst_db::disks::set_state(db, id, "ready", None).await?;
    if let Some(vm) = attach_to {
        ctx.step("attaching", 0.95).await;
        let mut tx = db.begin().await?;
        let disk = mst_db::disks::get_for_update(&mut tx, id).await?;
        crate::api::attach_disk(&ctx.state, &mut tx, vm, &disk)
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

async fn copy(
    ctx: &TaskCtx,
    disk: &Disk,
    dest: StoragePoolId,
    dest_host: Option<HostId>,
) -> Result<()> {
    let db = &ctx.state.db;
    let state = &ctx.state;
    let src_pool = mst_db::storage_pools::get(db, disk.pool_id).await?;
    let dst_pool = mst_db::storage_pools::get(db, dest).await?;
    let src_host = disk.host_id.filter(|_| src_pool.kind == "local");
    let dst_host = dest_host.filter(|_| dst_pool.kind == "local");
    if src_pool.id == dst_pool.id && src_host == dst_host {
        bail!("the disk is there already");
    }
    let hosts: Vec<Host> = mst_db::hosts::list(db)
        .await?
        .into_iter()
        .filter(|h| h.is_live() && !matches!(h.state.as_str(), "fenced" | "decommissioned"))
        .collect();
    let src = crate::api::agent_pool(&src_pool)?;
    let dst = crate::api::agent_pool(&dst_pool)?;
    let volume_id = disk.id.to_string();
    let size_bytes = disk.size_bytes as u64;

    // Where the copy is made, to remove it should the move fail.
    let copier = match hosts
        .iter()
        .find(|h| reaches(h, &src_pool, src_host) && reaches(h, &dst_pool, dst_host))
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
                .map(|_| h.id)
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
                .find(|h| reaches(h, &dst_pool, dst_host))
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
            let port = serde_json::from_str::<serde_json::Value>(&served)?["port"]
                .as_u64()
                .context("no port the disk is served on")?;
            ctx.step(&format!("copying to {}", client.name), 0.2).await;
            let copied = send(
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
            .await;
            let end = Cmd::EndExport(EndExport {
                volume_id: volume_id.clone(),
            });
            let _ = send(state, server.id, end, QUICK).await;
            copied.context("copying the disk").map(|_| client.id)
        }
    };
    let copier = match copier {
        Ok(h) => h,
        Err(e) => {
            // The copy, as far as it got, goes (never the disk itself: it
            // is elsewhere, checked above).
            for h in hosts
                .iter()
                .filter(|h| reaches(h, &dst_pool, dst_host))
                .take(1)
            {
                let cmd = Cmd::DeleteVolume(DeleteVolume {
                    volume_id: volume_id.clone(),
                    pool: Some(dst.clone()),
                });
                let _ = send(state, h.id, cmd, QUICK).await;
            }
            return Err(e);
        }
    };
    mst_db::disks::moved(db, disk.id, dst_pool.id, dst_host).await?;
    tracing::info!(disk = %disk.id, pool = %dst_pool.name, host = ?dst_host, %copier, "disk moved");

    // The old copy goes.
    ctx.step("removing the old copy", 0.9).await;
    match hosts.iter().find(|h| reaches(h, &src_pool, src_host)) {
        Some(h) => {
            let cmd = Cmd::DeleteVolume(DeleteVolume {
                volume_id,
                pool: Some(src),
            });
            if let Err(e) = send(state, h.id, cmd, QUICK).await {
                tracing::warn!(disk = %disk.id, error = %format!("{e:#}"), "removing a moved disk's old copy");
            }
        }
        None => {
            tracing::warn!(disk = %disk.id, "no connected host has a moved disk's old copy to remove")
        }
    }
    Ok(())
}
