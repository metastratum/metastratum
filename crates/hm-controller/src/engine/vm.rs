//! VM task handlers. Each step is idempotent so a resumed task can rerun it.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use hm_core::ids::{HostId, VmId};
use hm_db::vms::Vm;
use hm_proto::command::Cmd;
use hm_proto::{
    DefineVm, DeleteVolume, EnsureImage, EnsureVolume, RebootVm, StartVm, StopVm, UndefineVm,
};

use super::TaskCtx;

const QUICK: Duration = Duration::from_secs(60);
/// Image downloads can be large.
const SLOW: Duration = Duration::from_secs(30 * 60);

fn target(ctx: &TaskCtx) -> Result<VmId> {
    ctx.task
        .target_id
        .map(VmId)
        .context("task has no target VM")
}

fn host_of(vm: &Vm) -> Result<HostId> {
    vm.host_id.context("VM is not placed on a host")
}

async fn call(ctx: &TaskCtx, host: HostId, cmd: Cmd, timeout: Duration) -> Result<()> {
    ctx.state
        .agents
        .call(host, cmd, timeout)
        .await
        .map_err(|e| anyhow!(e))
}

pub async fn create(ctx: &TaskCtx) -> Result<()> {
    let id = target(ctx)?;
    let result = provision(ctx, id).await;
    let db = &ctx.state.db;
    match &result {
        Ok(()) => hm_db::vms::set_status(db, id, "ready", None).await?,
        Err(e) => hm_db::vms::set_status(db, id, "error", Some(&format!("{e:#}"))).await?,
    }
    result
}

async fn provision(ctx: &TaskCtx, id: VmId) -> Result<()> {
    let db = &ctx.state.db;
    let vm = hm_db::vms::get(db, id).await?;
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
            let p = hm_db::storage_pools::get(db, disk.pool_id).await?;
            e.insert(crate::api::agent_pool(&p));
        }
    }

    ctx.step("image", 0.2).await;
    for disk in &spec.disks {
        if let Some(image_id) = disk.image_id {
            let image = hm_db::images::get(db, image_id).await?;
            call(
                ctx,
                host,
                Cmd::EnsureImage(EnsureImage {
                    image_id: image.id.to_string(),
                    source_url: image.source_url,
                    format: image.format,
                    pool: Some(pools[&disk.pool_id].clone()),
                }),
                SLOW,
            )
            .await
            .context("fetching image")?;
        }
    }

    ctx.step("volumes", 0.6).await;
    for disk in &spec.disks {
        call(
            ctx,
            host,
            Cmd::EnsureVolume(EnsureVolume {
                volume_id: disk.volume_id.to_string(),
                size_bytes: disk.size_bytes,
                image_id: disk.image_id.map(|i| i.to_string()).unwrap_or_default(),
                pool: Some(pools[&disk.pool_id].clone()),
            }),
            QUICK,
        )
        .await
        .context("creating volume")?;
    }

    ctx.step("define", 0.8).await;
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
    let vm = hm_db::vms::get(&ctx.state.db, target(ctx)?).await?;
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
    let vm = hm_db::vms::get(&ctx.state.db, target(ctx)?).await?;
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
    let vm = hm_db::vms::get(&ctx.state.db, target(ctx)?).await?;
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

pub async fn delete(ctx: &TaskCtx) -> Result<()> {
    let db = &ctx.state.db;
    let id = target(ctx)?;
    let vm = match hm_db::vms::get(db, id).await {
        Ok(vm) => vm,
        // Already gone: a previous attempt finished the job.
        Err(hm_db::Error::Domain(hm_core::Error::NotFound { .. })) => return Ok(()),
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
        for vol in hm_db::vms::volumes_for_vm(db, id).await? {
            let pool = crate::api::agent_pool(&hm_db::storage_pools::get(db, vol.pool_id).await?);
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
            hm_db::vms::delete_volume(db, vol.id).await?;
        }
    }

    ctx.step("network", 0.8).await;
    // Deleting the VM deletes its NICs; their ports go in the next pass.
    hm_db::vms::delete(db, id).await?;
    if let Err(e) = ctx.state.net.sync().await {
        tracing::warn!(vm = %id, error = %format!("{e:#}"), "removing the VM's ports; retrying in the background");
        ctx.state.net.kick();
    }
    Ok(())
}
