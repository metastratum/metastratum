//! VM specification shared by the controller (desired state) and the agent
//! (which renders it into a libvirt domain).

use serde::{Deserialize, Serialize};

use crate::ids::{ImageId, LOCAL_POOL, NetworkId, NicId, StoragePoolId, VolumeId};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Firmware {
    Bios,
    #[default]
    Uefi,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct VmSpec {
    /// The virtual CPUs the guest has (plugged in, while it runs).
    pub vcpus: u32,
    /// What `vcpus` may grow to while the VM runs (CPUs plugged in, and out
    /// again). Unset: none are, and it has `vcpus`. Once set it stays while
    /// the VM runs, also when all are plugged in: its CPUs are laid out for
    /// plugging. What hosts' capacity and quotas count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_vcpus: Option<u32>,
    /// The memory the guest has: what it boots with, and what the balloon
    /// gives it while it runs.
    pub memory_mib: u64,
    /// What `memory_mib` may grow to while the VM runs (through its
    /// balloon, for guests with the driver); `memory_mib` when unset. What
    /// hosts' capacity and quotas count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_mib: Option<u64>,
    /// The guest hands memory it frees back to the host (virtio-balloon free
    /// page reporting): on for new VMs, and can be turned off (for guests
    /// that should keep their memory warm, say). It is part of the balloon
    /// device, so it changes only while the VM is stopped. Unset for VMs
    /// from before it existed; they get it at their next start from stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub free_page_reporting: Option<bool>,
    /// Its disks other than the boot disk sit on a virtio-SCSI controller,
    /// each at a unit of its own, so they plug in and out while it runs (and
    /// stay put when one before them goes). It adds a device, so it is set
    /// as the VM starts from stopped; unset for VMs not started since, whose
    /// disks are all virtio disks in order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_hotplug: Option<bool>,
    #[serde(default)]
    pub firmware: Firmware,
    /// Boot disk first.
    pub disks: Vec<DiskSpec>,
    pub nics: Vec<NicSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_init: Option<CloudInit>,
    /// The guest CPU: the VM's cluster baseline, so it can run (and migrate)
    /// on any host of the cluster. Unset for VMs from before live migration,
    /// which get their host's CPU as is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CpuSpec>,
    /// Whether the VM may live-migrate. If not, it gets its host's CPU with
    /// every feature, and disks use the host's page cache: faster, but tied
    /// to the host. VMs from before this setting existed could migrate.
    #[serde(default = "yes")]
    pub migratable: bool,
}

impl VmSpec {
    /// The most memory the VM can have without a restart: what is reserved
    /// for it on its host, and counted against its tenant's quota.
    pub fn reserved_memory_mib(&self) -> u64 {
        self.max_memory_mib.unwrap_or(0).max(self.memory_mib)
    }

    /// Whether the balloon hands free memory back.
    pub fn reports_free_pages(&self) -> bool {
        self.free_page_reporting.unwrap_or(false)
    }

    /// The most virtual CPUs the VM can have without a restart: what its
    /// host keeps for it, and what counts against its tenant's quota.
    pub fn reserved_vcpus(&self) -> u32 {
        self.max_vcpus.unwrap_or(0).max(self.vcpus)
    }

    /// Where each of its disks sits in the guest, in the order of `disks`.
    pub fn disk_slots(&self) -> Vec<DiskSlot> {
        let hotplug = self.disk_hotplug == Some(true);
        self.disks
            .iter()
            .enumerate()
            .map(|(i, d)| match (i, hotplug) {
                (0, _) | (_, false) => DiskSlot {
                    // vda, vdb, ...: specs stay far below 26 disks.
                    target: format!("vd{}", letter(i as u32)),
                    scsi_unit: None,
                },
                (_, true) => {
                    let unit = d.unit.unwrap_or(i as u32 - 1);
                    DiskSlot {
                        // sdb...: sda is the cloud-init seed's (on SATA).
                        target: format!("sd{}", letter(unit + 1)),
                        scsi_unit: Some(unit),
                    }
                }
            })
            .collect()
    }

    /// The SCSI unit for another data disk: the lowest one free, also
    /// before its disks are on the SCSI controller (they take the units
    /// they will have there).
    pub fn free_unit(&self) -> u32 {
        let taken: Vec<u32> = self
            .disks
            .iter()
            .skip(1)
            .enumerate()
            .map(|(i, d)| d.unit.unwrap_or(i as u32))
            .collect();
        (0..).find(|u| !taken.contains(u)).unwrap_or(0)
    }
}

/// The most disks a VM has besides its boot disk.
pub const MAX_DATA_DISKS: usize = 16;

/// Where a disk sits in the guest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskSlot {
    /// libvirt's target device: `vda` for the boot disk.
    pub target: String,
    /// Its unit on the virtio-SCSI controller, for data disks of VMs that
    /// plug them in while running.
    pub scsi_unit: Option<u32>,
}

fn letter(i: u32) -> char {
    (b'a' + i as u8) as char
}

fn yes() -> bool {
    true
}

/// A named CPU model with feature adjustments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct CpuSpec {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// `require`d and `disable`d features on top of the model.
    #[serde(default)]
    pub features: Vec<CpuFeature>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct CpuFeature {
    pub policy: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct DiskSpec {
    pub volume_id: VolumeId,
    /// Pool the volume lives in. Specs from before storage pools existed
    /// used local storage.
    #[serde(default = "local_pool")]
    pub pool_id: StoragePoolId,
    pub size_bytes: u64,
    /// Image the volume was cloned from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_id: Option<ImageId>,
    /// The backup disk the volume was restored from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored_from: Option<RestoredFrom>,
    /// The volume (in the same pool) it was copied from, if any: a clone of
    /// another VM's disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloned_from: Option<VolumeId>,
    /// Its unit on the VM's SCSI controller (see [`VmSpec::disk_hotplug`]),
    /// kept while it is attached; unset for the boot disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<u32>,
}

/// One of a backup's disks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct RestoredFrom {
    pub backup_id: crate::ids::BackupId,
    /// Its place among the VM's disks (vda is 0).
    pub index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct NicSpec {
    /// Also the OVN logical switch port name and OVS iface-id.
    pub id: NicId,
    pub network_id: NetworkId,
    pub mac: String,
    #[cfg_attr(feature = "utoipa", schema(value_type = String, format = Ipv4))]
    pub ip: std::net::Ipv4Addr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "utoipa", derive(utoipa::ToSchema))]
pub struct CloudInit {
    /// Hostname inside the guest.
    pub hostname: String,
    /// Complete `#cloud-config` (or script) user data.
    pub user_data: String,
}

fn local_pool() -> StoragePoolId {
    LOCAL_POOL
}

/// Locally administered unicast MAC in metastratum's prefix, derived from the
/// NIC ID so it is stable and needs no allocation table.
pub fn mac_for(nic: NicId) -> String {
    let b = nic.0.as_bytes();
    // UUIDv7's leading bytes are a timestamp; the tail is random.
    format!(
        "02:68:{:02x}:{:02x}:{:02x}:{:02x}",
        b[12], b[13], b[14], b[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_page_reporting_tells_off_from_never_set() {
        let base = r#""vcpus":1,"memory_mib":512,"disks":[],"nics":[]"#;
        let parse = |extra: &str| -> VmSpec {
            serde_json::from_str(&format!("{{{base}{extra}}}")).unwrap()
        };
        // From before it existed: unset, off until the next start sets it.
        let legacy = parse("");
        assert_eq!(legacy.free_page_reporting, None);
        assert!(!legacy.reports_free_pages());
        // Turned off: stays off, also written out.
        let off = parse(r#","free_page_reporting":false"#);
        assert_eq!(off.free_page_reporting, Some(false));
        let again: VmSpec = serde_json::from_value(serde_json::to_value(&off).unwrap()).unwrap();
        assert_eq!(again.free_page_reporting, Some(false));
        assert!(parse(r#","free_page_reporting":true"#).reports_free_pages());
    }

    #[test]
    fn spec_json_roundtrip_with_defaults() {
        let json = r#"{"vcpus":2,"memory_mib":1024,"disks":[{"volume_id":"0199f0a2-7b3c-7d00-8000-000000000001","size_bytes":1}],"nics":[]}"#;
        let spec: VmSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.firmware, Firmware::Uefi);
        assert!(spec.cloud_init.is_none());
        assert_eq!(
            spec.disks[0].pool_id, LOCAL_POOL,
            "old specs used local storage"
        );
        let again: VmSpec = serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
        assert_eq!(again, spec);
    }

    #[test]
    fn data_disks_keep_their_place() {
        let disk = |unit| DiskSpec {
            volume_id: VolumeId::new(),
            pool_id: LOCAL_POOL,
            size_bytes: 1,
            image_id: None,
            restored_from: None,
            cloned_from: None,
            unit,
        };
        let mut spec: VmSpec =
            serde_json::from_str(r#"{"vcpus":1,"memory_mib":512,"disks":[],"nics":[]}"#).unwrap();
        spec.disks = vec![disk(None), disk(Some(0)), disk(Some(2))];
        // Not started since: virtio disks in order.
        let targets = |s: &VmSpec| {
            s.disk_slots()
                .into_iter()
                .map(|d| d.target)
                .collect::<Vec<_>>()
        };
        assert_eq!(targets(&spec), ["vda", "vdb", "vdc"]);
        // A disk from before units takes the one it will have.
        let mut legacy = spec.clone();
        legacy.disks[1].unit = None;
        legacy.disks[2].unit = None;
        assert_eq!(legacy.free_unit(), 2);
        // Plugged in: units stay as they were when one goes.
        spec.disk_hotplug = Some(true);
        assert_eq!(targets(&spec), ["vda", "sdb", "sdd"]);
        assert_eq!(spec.free_unit(), 1);
        spec.disks.remove(1);
        assert_eq!(targets(&spec), ["vda", "sdd"]);
        assert_eq!(spec.disk_slots()[1].scsi_unit, Some(2));
        assert_eq!(spec.free_unit(), 0);
    }

    #[test]
    fn macs_are_locally_administered_unicast() {
        let mac = mac_for(NicId::new());
        let first = u8::from_str_radix(&mac[..2], 16).unwrap();
        assert_eq!(first & 0b11, 0b10, "{mac}");
        assert_eq!(mac.len(), 17);
    }
}
