//! VM specification shared by the controller (desired state) and the agent
//! (which renders it into a libvirt domain).

use serde::{Deserialize, Serialize};

use crate::ids::{ImageId, NetworkId, NicId, VolumeId};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Firmware {
    Bios,
    #[default]
    Uefi,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskSpec {
    pub volume_id: VolumeId,
    pub size_bytes: u64,
    /// Image the volume was cloned from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_id: Option<ImageId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NicSpec {
    /// Also the OVN logical switch port name and OVS iface-id.
    pub id: NicId,
    pub network_id: NetworkId,
    pub mac: String,
    pub ip: std::net::Ipv4Addr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudInit {
    /// Hostname inside the guest.
    pub hostname: String,
    /// Complete `#cloud-config` (or script) user data.
    pub user_data: String,
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
        let json = r#"{"vcpus":2,"memory_mib":1024,"disks":[],"nics":[]}"#;
        let spec: VmSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.firmware, Firmware::Uefi);
        assert!(spec.cloud_init.is_none());
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
