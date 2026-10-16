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
    pub vcpus: u32,
    pub memory_mib: u64,
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

/// Locally administered unicast MAC in hypermon's prefix, derived from the
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
    fn macs_are_locally_administered_unicast() {
        let mac = mac_for(NicId::new());
        let first = u8::from_str_radix(&mac[..2], 16).unwrap();
        assert_eq!(first & 0b11, 0b10, "{mac}");
        assert_eq!(mac.len(), 17);
    }
}
