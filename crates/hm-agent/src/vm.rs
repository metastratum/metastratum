//! Turning a VM spec into a libvirt domain on this host.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use hm_core::vm::{Firmware, VmSpec};
use hm_libvirt::{Disk, DiskDevice, DiskSource, DomainDef, Nic};
use hm_storage::Pool;

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
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

/// libvirt's CPU model as hypermon stores it.
pub fn cpu_spec(c: hm_libvirt::CpuModel) -> hm_core::vm::CpuSpec {
    hm_core::vm::CpuSpec {
        model: c.model,
        vendor: c.vendor,
        features: c
            .features
            .into_iter()
            .map(|(policy, name)| hm_core::vm::CpuFeature { policy, name })
            .collect(),
    }
}

pub fn cpu_model(c: &hm_core::vm::CpuSpec) -> hm_libvirt::CpuModel {
    hm_libvirt::CpuModel {
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
    vm_id: &str,
    tenant_id: &str,
    name: &str,
    generation: u64,
    spec: &VmSpec,
) -> Result<DomainDef> {
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
        tokio::fs::create_dir_all(&files.cloud_init_dir).await?;
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
        tenant_id: tenant_id.to_owned(),
        title: name.to_owned(),
        generation,
        vcpus: spec.vcpus,
        memory_mib: spec.memory_mib,
        firmware: match spec.firmware {
            Firmware::Bios => hm_libvirt::Firmware::Bios,
            Firmware::Uefi => hm_libvirt::Firmware::Uefi,
        },
        disks,
        // A VM that never migrates gets everything its host offers.
        cpu: spec.cpu.as_ref().filter(|_| spec.migratable).map(cpu_model),
        host_cache: !spec.migratable,
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
