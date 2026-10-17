//! VM task handlers. Each step is idempotent so a resumed task can rerun it.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use mst_core::ids::{HostId, VmId};
use mst_db::vms::Vm;
use mst_proto::command::Cmd;
use mst_proto::{
    CloneVolume, DefineVm, DeleteVolume, EnsureImage, EnsureVolume, RebootVm, RestoreVolume,
    StartVm, StopVm, UndefineVm,
};

use super::TaskCtx;

const QUICK: Duration = Duration::from_secs(60);
/// Image downloads can be large.
const SLOW: Duration = Duration::from_secs(30 * 60);
/// So can disks restored from backups.
const RESTORE: Duration = Duration::from_secs(12 * 3600);

fn target(ctx: &TaskCtx) -> Result<VmId> {
    ctx.task
        .target_id
        .map(VmId)
        .context("task has no target VM")
}

fn host_of(vm: &Vm) -> Result<HostId> {
    vm.host_id.context("VM is not placed on a host")
}

/// Runs a command on a host. Every command sent from here is idempotent, so
/// one cut off by the agent reconnecting (a renewed certificate, a network
/// blip, another controller) is simply sent again once it is back.
async fn call(ctx: &TaskCtx, host: HostId, cmd: Cmd, timeout: Duration) -> Result<()> {
    use crate::agents::AgentError;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        match ctx.state.agents.call(host, cmd.clone(), timeout).await {
            Err(AgentError::Closed | AgentError::NotConnected)
                if tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            r => return r.map_err(|e| anyhow!(e)),
        }
    }
}

pub async fn create(ctx: &TaskCtx) -> Result<()> {
    let id = target(ctx)?;
    let result = provision(ctx, id).await;
    let db = &ctx.state.db;
    match &result {
        Ok(()) => mst_db::vms::set_status(db, id, "ready", None).await?,
        Err(e) => mst_db::vms::set_status(db, id, "error", Some(&format!("{e:#}"))).await?,
    }
    result
}

pub(super) async fn provision(ctx: &TaskCtx, id: VmId) -> Result<()> {
    let db = &ctx.state.db;
    let vm = mst_db::vms::get(db, id).await?;
    let host = host_of(&vm)?;
    let spec = &vm.spec.0;

    ctx.step("network", 0.1).await;
    // The VM's ports, security groups and addresses are in the database;
    // have OVN implement them before the VM plugs in.
    ctx.state
        .net
        .sync()
        .await
        .context("programming the network")?;

    let mut pools = std::collections::BTreeMap::new();
    for disk in &spec.disks {
        if let std::collections::btree_map::Entry::Vacant(e) = pools.entry(disk.pool_id) {
            let p = mst_db::storage_pools::get(db, disk.pool_id).await?;
            e.insert(crate::api::agent_pool(&p)?);
        }
    }

    ctx.step("image", 0.2).await;
    for disk in &spec.disks {
        if let Some(image_id) = disk.image_id {
            let image = mst_db::images::get(db, image_id).await?;
            call(
                ctx,
                host,
                Cmd::EnsureImage(EnsureImage {
                    trusted: super::image_trusted(db, &image).await?,
                    image_id: image.id.to_string(),
                    source_url: image.source_url,
                    format: image.format,
                    pool: Some(pools[&disk.pool_id].clone()),
                    // An image larger than the disk is refused before it
                    // takes up any room.
                    max_virtual_bytes: disk.size_bytes,
                }),
                SLOW,
            )
            .await
            .context("fetching image")?;
        }
    }

    ctx.step("volumes", 0.6).await;
    for disk in &spec.disks {
        let pool = Some(pools[&disk.pool_id].clone());
        let (cmd, timeout) = match disk.restored_from {
            // A copy of another VM's disk, which takes as long as its size.
            None if disk.cloned_from.is_some() => {
                let cmd = Cmd::CloneVolume(CloneVolume {
                    pool,
                    source_volume_id: disk.cloned_from.map(|v| v.to_string()).unwrap_or_default(),
                    volume_id: disk.volume_id.to_string(),
                    size_bytes: disk.size_bytes,
                    image_id: disk.image_id.map(|i| i.to_string()).unwrap_or_default(),
                });
                (cmd, RESTORE)
            }
            // A copy of a backup's disk, which takes as long as its size.
            Some(from) => {
                let backup = mst_db::backups::get(db, from.backup_id).await?;
                let repo = mst_db::backups::get_repository(db, backup.repository_id).await?;
                let cmd = Cmd::RestoreVolume(RestoreVolume {
                    volume_id: disk.volume_id.to_string(),
                    size_bytes: disk.size_bytes,
                    pool,
                    repository: Some(crate::api::repository_pool(&repo)),
                    backup_id: from.backup_id.to_string(),
                    index: from.index,
                });
                (cmd, RESTORE)
            }
            None => {
                let cmd = Cmd::EnsureVolume(EnsureVolume {
                    volume_id: disk.volume_id.to_string(),
                    size_bytes: disk.size_bytes,
                    image_id: disk.image_id.map(|i| i.to_string()).unwrap_or_default(),
                    pool,
                });
                (cmd, QUICK)
            }
        };
        call(ctx, host, cmd, timeout)
            .await
            .context("creating volume")?;
    }

    ctx.step("define", 0.8).await;
    // A CPU model every host of the cluster runs, so the VM can migrate
    // anywhere in it. Kept in the spec: the VM keeps it for life.
    let mut spec = spec.clone();
    if spec.migratable && spec.cpu.is_none() {
        spec.cpu = crate::migration::baseline_cpu(&ctx.state, host).await;
        if spec.cpu.is_some() {
            mst_db::vms::set_spec(db, vm.id, &spec).await?;
        }
    }
    let spec = &spec;
    call(
        ctx,
        host,
        Cmd::DefineVm(DefineVm {
            vm_id: vm.id.to_string(),
            tenant_id: vm.tenant_id.to_string(),
            name: vm.name.clone(),
            generation: vm.generation as u64,
            spec_json: serde_json::to_string(spec)?,
            pools: pools.into_values().collect(),
            disk_limits: crate::limits::disk_limits(db, host).await?,
        }),
        QUICK,
    )
    .await
    .context("defining VM")?;

    if vm.desired_power == "running" {
        ctx.step("start", 0.9).await;
        call(
            ctx,
            host,
            Cmd::StartVm(StartVm {
                vm_id: vm.id.to_string(),
            }),
            QUICK,
        )
        .await
        .context("starting VM")?;
    }
    Ok(())
}

pub async fn start(ctx: &TaskCtx) -> Result<()> {
    let vm = mst_db::vms::get(&ctx.state.db, target(ctx)?).await?;
    call(
        ctx,
        host_of(&vm)?,
        Cmd::StartVm(StartVm {
            vm_id: vm.id.to_string(),
        }),
        QUICK,
    )
    .await
}

pub async fn stop(ctx: &TaskCtx) -> Result<()> {
    let vm = mst_db::vms::get(&ctx.state.db, target(ctx)?).await?;
    let force = ctx
        .task
        .params
        .get("force")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    call(
        ctx,
        host_of(&vm)?,
        Cmd::StopVm(StopVm {
            vm_id: vm.id.to_string(),
            force,
        }),
        QUICK,
    )
    .await
}

pub async fn reboot(ctx: &TaskCtx) -> Result<()> {
    let vm = mst_db::vms::get(&ctx.state.db, target(ctx)?).await?;
    call(
        ctx,
        host_of(&vm)?,
        Cmd::RebootVm(RebootVm {
            vm_id: vm.id.to_string(),
        }),
        QUICK,
    )
    .await
}

/// Drops the copies of tenants' images cached for a VM just deleted, if no
/// volume is built on them any more: they count against nothing, so they
/// must not pile up (an image can unpack to the size of the disk it was
/// for). The provider's images stay cached for the next VM. Where a volume
/// still is built on one after all (a VM being created meanwhile), the host
/// keeps it.
async fn drop_unused_images(
    ctx: &TaskCtx,
    host: HostId,
    images: std::collections::BTreeMap<mst_core::ids::ImageId, Vec<mst_proto::Pool>>,
) {
    let db = &ctx.state.db;
    for (id, pools) in images {
        let Ok(image) = mst_db::images::get(db, id).await else {
            continue;
        };
        if super::image_trusted(db, &image).await.unwrap_or(true)
            || mst_db::images::in_use(db, id).await.unwrap_or(true)
        {
            continue;
        }
        let cmd = Cmd::DeleteImage(mst_proto::DeleteImage {
            image_id: id.to_string(),
            pools,
        });
        match ctx.state.agents.call(host, cmd, QUICK).await {
            Ok(()) => tracing::info!(image = %id, "dropped an unused image's cached copies"),
            Err(e) => tracing::info!(image = %id, error = %e, "kept an image's cached copies"),
        }
    }
}

pub async fn delete(ctx: &TaskCtx) -> Result<()> {
    let db = &ctx.state.db;
    let id = target(ctx)?;
    let vm = match mst_db::vms::get(db, id).await {
        Ok(vm) => vm,
        // Already gone: a previous attempt finished the job.
        Err(mst_db::Error::Domain(mst_core::Error::NotFound { .. })) => return Ok(()),
        Err(e) => return Err(e.into()),
    };

    if let Some(host) = vm.host_id {
        ctx.step("undefine", 0.2).await;
        call(
            ctx,
            host,
            Cmd::UndefineVm(UndefineVm {
                vm_id: id.to_string(),
            }),
            QUICK,
        )
        .await
        .context("removing domain")?;
        ctx.step("volumes", 0.5).await;
        let mut images: std::collections::BTreeMap<_, Vec<mst_proto::Pool>> = Default::default();
        for vol in mst_db::vms::volumes_for_vm(db, id).await? {
            let stored = mst_db::storage_pools::get(db, vol.pool_id).await?;
            let pool = crate::api::agent_pool(&stored)?;
            if let Some(image) = vol.image_id {
                let shared = images.entry(image).or_default();
                if stored.kind != "local" {
                    shared.push(pool.clone());
                }
            }
            call(
                ctx,
                host,
                Cmd::DeleteVolume(DeleteVolume {
                    volume_id: vol.id.to_string(),
                    pool: Some(pool),
                }),
                QUICK,
            )
            .await
            .context("deleting volume")?;
            mst_db::vms::delete_volume(db, vol.id).await?;
        }
        drop_unused_images(ctx, host, images).await;
    }

    ctx.step("network", 0.8).await;
    // Deleting the VM deletes its NICs; their ports go in the next pass.
    mst_db::vms::delete(db, id).await?;
    if let Err(e) = ctx.state.net.sync().await {
        tracing::warn!(vm = %id, error = %format!("{e:#}"), "removing the VM's ports; retrying in the background");
        ctx.state.net.kick();
    }
    Ok(())
}
