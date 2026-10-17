//! Turning a VM spec into a libvirt domain on this host.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use mst_core::vm::{Firmware, VmSpec};
use mst_libvirt::{Disk, DiskDevice, DiskSource, DomainDef, Nic};
use mst_storage::Pool;

/// OVS integration bridge managed by ovn-controller.
const INTEGRATION_BRIDGE: &str = "br-int";

pub struct VmFiles {
    pub cloud_init_dir: PathBuf,
}

impl VmFiles {
    pub fn seed_path(&self, vm_id: &str) -> PathBuf {
        self.cloud_init_dir.join(format!("{vm_id}.iso"))
    }
}

/// Writes the NoCloud seed ISO for a VM. Always regenerated so a redefined
/// VM picks up changed user data.
async fn write_seed(path: &Path, vm_id: &str, hostname: &str, user_data: &str) -> Result<()> {
    let work = path.with_extension("d");
    tokio::fs::create_dir_all(&work).await?;
    tokio::fs::write(work.join("user-data"), user_data).await?;
    tokio::fs::write(
        work.join("meta-data"),
        format!("instance-id: {vm_id}\nlocal-hostname: {hostname}\n"),
    )
    .await?;
    let tmp = path.with_extension("tmp");
    let out = tokio::process::Command::new("genisoimage")
        .arg("-quiet")
        .arg("-output")
        .arg(&tmp)
        .args(["-volid", "cidata", "-joliet", "-rock"])
        .arg(work.join("user-data"))
        .arg(work.join("meta-data"))
        .output()
        .await
        .context("running genisoimage")?;
    let _ = tokio::fs::remove_dir_all(&work).await;
    if !out.status.success() {
        bail!(
            "genisoimage: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // User data may hold secrets: for root, and the VM's QEMU (libvirt
    // hands it over while the VM runs), only.
    tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// libvirt's CPU model as metastratum stores it.
pub fn cpu_spec(c: mst_libvirt::CpuModel) -> mst_core::vm::CpuSpec {
    mst_core::vm::CpuSpec {
        model: c.model,
        vendor: c.vendor,
        features: c
            .features
            .into_iter()
            .map(|(policy, name)| mst_core::vm::CpuFeature { policy, name })
            .collect(),
    }
}

pub fn cpu_model(c: &mst_core::vm::CpuSpec) -> mst_libvirt::CpuModel {
    mst_libvirt::CpuModel {
        model: c.model.clone(),
        vendor: c.vendor.clone(),
        features: c
            .features
            .iter()
            .map(|f| (f.policy.clone(), f.name.clone()))
            .collect(),
    }
}

pub async fn domain(
    pools: &std::collections::HashMap<String, Pool>,
    files: &VmFiles,
    vm: &mst_proto::DefineVm,
    spec: &VmSpec,
    disk_limits: mst_libvirt::DiskLimits,
) -> Result<DomainDef> {
    let vm_id = vm.vm_id.as_str();
    let mut disks = Vec::new();
    for (i, d) in spec.disks.iter().enumerate() {
        let pool = pools
            .get(&d.pool_id.to_string())
            .with_context(|| format!("storage pool {} of disk {i} was not provided", d.pool_id))?;
        let volume = d.volume_id.to_string();
        let (source, format) = match pool {
            Pool::File(f) => (
                DiskSource::File(f.volume_path(&volume)?.to_string_lossy().into_owned()),
                "qcow2",
            ),
            Pool::Rbd(r) => (
                DiskSource::Rbd {
                    name: r.volume_path(&volume)?,
                    monitors: r.monitor_addrs(),
                    user: r.user.clone(),
                    secret_uuid: d.pool_id.to_string(),
                },
                "raw",
            ),
        };
        disks.push(Disk {
            device: DiskDevice::Disk,
            source,
            format: format.into(),
            // vda, vdb, ...; specs stay far below 26 disks.
            target: format!("vd{}", (b'a' + i as u8) as char),
            bus: "virtio".into(),
            boot_order: (i == 0).then_some(1),
            readonly: false,
        });
    }
    if let Some(ci) = &spec.cloud_init {
        mst_storage::private_dir(&files.cloud_init_dir).await?;
        let seed = files.seed_path(vm_id);
        write_seed(&seed, vm_id, &ci.hostname, &ci.user_data).await?;
        disks.push(Disk {
            device: DiskDevice::Cdrom,
            source: DiskSource::File(seed.to_string_lossy().into_owned()),
            format: "raw".into(),
            target: "sda".into(),
            bus: "sata".into(),
            boot_order: None,
            readonly: true,
        });
    }
    Ok(DomainDef {
        uuid: vm_id.to_owned(),
        tenant_id: vm.tenant_id.clone(),
        title: vm.name.clone(),
        generation: vm.generation,
        vcpus: spec.vcpus,
        memory_mib: spec.memory_mib,
        max_memory_mib: spec.reserved_memory_mib(),
        free_page_reporting: spec.free_page_reporting,
        firmware: match spec.firmware {
            Firmware::Bios => mst_libvirt::Firmware::Bios,
            Firmware::Uefi => mst_libvirt::Firmware::Uefi,
        },
        disks,
        // A VM that never migrates gets everything its host offers.
        cpu: spec.cpu.as_ref().filter(|_| spec.migratable).map(cpu_model),
        host_cache: !spec.migratable,
        disk_limits,
        nics: spec
            .nics
            .iter()
            .map(|n| Nic {
                mac: n.mac.clone(),
                bridge: INTEGRATION_BRIDGE.into(),
                interface_id: n.id.to_string(),
            })
            .collect(),
    })
}

/// Grows one of a VM's disks: through libvirt while the VM runs, so the
/// guest sees it at once; otherwise the volume itself.
pub async fn resize_disk(agent: &crate::session::Agent, c: &mst_proto::ResizeDisk) -> Result<()> {
    let disk = c.disk.as_ref().context("no disk given")?;
    let pool = agent.pool(disk.pool.as_ref()).await?;
    if matches!(
        agent.lv.power(&c.vm_id).await,
        Ok(mst_libvirt::Power::Running | mst_libvirt::Power::Paused)
    ) {
        agent
            .lv
            .block_resize(
                &c.vm_id,
                &crate::backup::target_of(disk.index),
                c.size_bytes,
            )
            .await
            .context("growing the disk")?;
    } else {
        pool.grow_volume(&disk.volume_id, c.size_bytes)
            .await
            .context("growing the volume")?;
    }
    tracing::info!(vm = %c.vm_id, disk = disk.index, size = c.size_bytes, "disk grown");
    Ok(())
}

/// Creates a volume as a copy of another VM's (stopped) volume.
pub async fn clone_volume(agent: &crate::session::Agent, c: &mst_proto::CloneVolume) -> Result<()> {
    let pool = agent.pool(c.pool.as_ref()).await?;
    let image = (!c.image_id.is_empty()).then_some(c.image_id.as_str());
    pool.clone_volume(&c.source_volume_id, &c.volume_id, c.size_bytes, image)
        .await
        .context("copying the volume")?;
    Ok(())
}
