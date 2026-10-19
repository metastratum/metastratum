//! VM task handlers. Each step is idempotent so a resumed task can rerun it.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use mst_core::ids::{HostId, VmId};
use mst_db::vms::Vm;
use mst_proto::command::Cmd;
use mst_proto::{
    CloneVolume, DefineVm, DeleteVolume, EnsureVolume, RebootVm, RestoreVolume, StartVm, StopVm,
    UndefineVm, VolumeFromImage,
};

use super::TaskCtx;

const QUICK: Duration = Duration::from_secs(60);
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
    let result = provision(ctx, id, true).await;
    let db = &ctx.state.db;
    match &result {
        Ok(()) => mst_db::vms::settle(db, id, "ready", None).await?,
        Err(e) => mst_db::vms::settle(db, id, "error", Some(&format!("{e:#}"))).await?,
    }
    result
}

/// Puts the VM on its host: network, disks (`fresh`: made now; otherwise
/// they exist, on shared storage, as when it is restarted after its host
/// failed), definition, and power.
pub(super) async fn provision(ctx: &TaskCtx, id: VmId, fresh: bool) -> Result<()> {
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

    if fresh {
        make_disks(ctx, id, host, &spec.disks, &pools).await?;
    }

    ctx.step("define", 0.8).await;
    // A CPU model every host of the cluster runs, so the VM can migrate
    // anywhere in it. Kept in the spec: the VM keeps it for life.
    let mut spec = spec.clone();
    if fit_cpu(ctx, vm.id, host, &mut spec).await {
        mst_db::vms::set_spec(db, vm.id, &spec).await?;
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
            spec_json: crate::guest::host_spec_json(vm.id, spec)?,
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

/// Makes the VM's disks on its host: copies of their image, a cloned VM's
/// or a backup's disks, or blank. Disks there already are left alone (a
/// resumed task).
async fn make_disks(
    ctx: &TaskCtx,
    vm: VmId,
    host: HostId,
    disks: &[mst_core::vm::DiskSpec],
    pools: &std::collections::BTreeMap<mst_core::ids::StoragePoolId, mst_proto::Pool>,
) -> Result<()> {
    let db = &ctx.state.db;
    ctx.step("image", 0.2).await;
    // Volume → where the host makes it from: a shared pool's store, or its
    // own (None).
    let mut from_image = std::collections::BTreeMap::new();
    for disk in disks {
        if let Some(image_id) = disk.image_id
            && disk.cloned_from.is_none()
            && disk.restored_from.is_none()
        {
            let source = super::image::ensure_on(ctx, image_id, host, vm)
                .await
                .context("getting the image onto the host")?;
            from_image.insert(disk.volume_id, (image_id, source));
        }
    }

    ctx.step("volumes", 0.6).await;
    for disk in disks {
        let pool = Some(pools[&disk.pool_id].clone());
        if let Some((image_id, source)) = from_image.get(&disk.volume_id) {
            let pool = &pools[&disk.pool_id];
            volume_from_image(ctx, vm, host, disk, *image_id, pool, source.clone()).await?;
            continue;
        }
        let (cmd, timeout) = match disk.restored_from {
            // A copy of another VM's disk, which takes as long as its size.
            None if disk.cloned_from.is_some() => {
                let cmd = Cmd::CloneVolume(CloneVolume {
                    pool,
                    source_volume_id: disk.cloned_from.map(|v| v.to_string()).unwrap_or_default(),
                    volume_id: disk.volume_id.to_string(),
                    size_bytes: disk.size_bytes,
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
                    pool,
                });
                (cmd, QUICK)
            }
        };
        call(ctx, host, cmd, timeout)
            .await
            .context("creating volume")?;
    }
    Ok(())
}

/// Makes the disk a copy of the image in the host's store, or `source`'s
/// (a shared pool's).
async fn volume_from_image(
    ctx: &TaskCtx,
    vm: VmId,
    host: HostId,
    disk: &mst_core::vm::DiskSpec,
    image_id: mst_core::ids::ImageId,
    pool: &mst_proto::Pool,
    source: Option<mst_proto::Pool>,
) -> Result<()> {
    let db = &ctx.state.db;
    let image = mst_db::images::get(db, image_id).await?;
    let cmd = Cmd::VolumeFromImage(VolumeFromImage {
        volume_id: disk.volume_id.to_string(),
        size_bytes: disk.size_bytes,
        pool: Some(pool.clone()),
        image_id: image_id.to_string(),
        tenant_path: mst_db::images::store_path(db, image.tenant_id).await?,
        source_pool: source.clone(),
    });
    match call(ctx, host, cmd.clone(), RESTORE).await {
        Err(e) if super::image::missing_from_store(&e) => {
            // Not where it was recorded (removed by hand): forgotten there,
            // and got onto the host anew, once.
            let at = match &source {
                Some(p) => {
                    mst_db::images::Copy::shared(p.id.parse().map(mst_core::ids::StoragePoolId)?)
                }
                None => mst_db::images::Copy::local(host),
            };
            mst_db::images::remove_copy(db, image_id, at).await?;
            let source = super::image::ensure_on(ctx, image_id, host, vm)
                .await
                .context("getting the image onto the host")?;
            let mut cmd = cmd;
            if let Cmd::VolumeFromImage(c) = &mut cmd {
                c.source_pool = source;
            }
            call(ctx, host, cmd, RESTORE)
                .await
                .context("creating volume")?;
        }
        r => r.context("creating volume")?,
    }
    Ok(())
}

/// Gives a VM that may migrate and has no CPU model its cluster's: what
/// every host of it runs. Where there is none (hosts of two vendors, say),
/// the VM may not migrate: it gets its host's CPU as is, and says so.
/// Returns whether the spec changed.
async fn fit_cpu(ctx: &TaskCtx, vm: VmId, host: HostId, spec: &mut mst_core::vm::VmSpec) -> bool {
    if !spec.migratable || spec.cpu.is_some() {
        return false;
    }
    match crate::migration::baseline_cpu(&ctx.state, host).await {
        Some(cpu) => {
            // What it needs of hosts, to place and move it by.
            let needs = crate::cpu::expand(&ctx.state, host, &cpu)
                .await
                .ok()
                .flatten();
            let _ = mst_db::vms::set_cpu_features(
                &ctx.state.db,
                vm,
                needs.as_ref().map(|n| n.features.as_slice()),
            )
            .await;
            spec.cpu = Some(cpu);
        }
        None => {
            tracing::warn!(%vm, "no CPU model all hosts of the cluster run (CPU vendors differ?): the VM gets its host's CPU and does not migrate");
            spec.migratable = false;
        }
    }
    true
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

pub async fn delete(ctx: &TaskCtx) -> Result<()> {
    let id = target(ctx)?;
    let result = remove(ctx, id).await;
    // A delete that failed (its host was away, say) leaves the VM in
    // error with the reason, so it can be deleted again.
    if let Err(e) = &result {
        let error = format!("deleting it failed: {e:#}");
        mst_db::vms::set_status(&ctx.state.db, id, "error", Some(&error)).await?;
    }
    result
}

async fn remove(ctx: &TaskCtx, id: VmId) -> Result<()> {
    let db = &ctx.state.db;
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
        // Disks made with it (its boot disk, copies from a clone or a
        // restore: unnamed) go with it; disks of their own (named) only
        // when asked, otherwise they are kept, attached to nothing.
        let all = ctx
            .task
            .params
            .get("delete_disks")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let disks = mst_db::disks::of_vm(db, id).await?;
        // Disks kept lose the VM's snapshots, which go with it.
        let mut kept = vec![];
        for (i, d) in disks.iter().enumerate() {
            if !all && d.name.is_some() && !d.boot {
                let pool = mst_db::storage_pools::get(db, d.pool_id).await?;
                kept.push(mst_proto::BackupDisk {
                    index: i as u32,
                    volume_id: d.id.to_string(),
                    pool: Some(crate::api::agent_pool(&pool)?),
                    target: String::new(),
                    parent_index: None,
                });
            }
        }
        if !kept.is_empty() {
            for snap in mst_db::snapshots::of_vm(db, id).await? {
                let cmd = Cmd::DeleteSnapshot(mst_proto::DeleteSnapshot {
                    vm_id: id.to_string(),
                    snapshot_id: snap.id.to_string(),
                    disks: kept.clone(),
                });
                if let Err(e) = call(ctx, host, cmd, Duration::from_secs(3600)).await {
                    tracing::warn!(vm = %id, snapshot = %snap.id, error = %format!("{e:#}"), "removing a deleted VM's snapshot from a disk kept");
                }
            }
        }
        for vol in disks {
            if !all && vol.name.is_some() && !vol.boot {
                continue;
            }
            let stored = mst_db::storage_pools::get(db, vol.pool_id).await?;
            let pool = crate::api::agent_pool(&stored)?;
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
