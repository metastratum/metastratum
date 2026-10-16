//! Domain definitions: what the agent wants a VM to look like, rendered to
//! libvirt domain XML.

use crate::xml::El;

/// XML namespace for metastratum's per-domain metadata.
pub const METADATA_NS: &str = "urn:metastratum:vm:1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Firmware {
    Bios,
    Uefi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskDevice {
    Disk,
    Cdrom,
}

/// Where a disk's data lives.
#[derive(Clone, Debug)]
pub enum DiskSource {
    File(String),
    /// A Ceph RBD image, reached directly by QEMU.
    Rbd {
        /// `pool/image`.
        name: String,
        /// Monitor `(host, port)`s.
        monitors: Vec<(String, u16)>,
        /// Ceph client name without the `client.` prefix.
        user: String,
        /// libvirt secret holding the client's key.
        secret_uuid: String,
    },
}

#[derive(Clone, Debug)]
pub struct Disk {
    pub device: DiskDevice,
    pub source: DiskSource,
    /// "qcow2" or "raw".
    pub format: String,
    /// "vda", "sda", ...
    pub target: String,
    /// "virtio" or "sata".
    pub bus: String,
    pub boot_order: Option<u32>,
    pub readonly: bool,
}

#[derive(Clone, Debug)]
pub struct Nic {
    pub mac: String,
    /// OVS integration bridge, normally `br-int`.
    pub bridge: String,
    /// OVS `external_ids:iface-id`; OVN binds the logical switch port with this
    /// name to the interface. libvirt requires a UUID here.
    pub interface_id: String,
}

#[derive(Clone, Debug)]
pub struct DomainDef {
    /// Libvirt domain UUID. metastratum uses the VM ID, so lookups need no mapping.
    pub uuid: String,
    pub tenant_id: String,
    /// Human-readable name, shown as the domain title.
    pub title: String,
    pub generation: u64,
    pub vcpus: u32,
    pub memory_mib: u64,
    pub firmware: Firmware,
    pub disks: Vec<Disk>,
    pub nics: Vec<Nic>,
    /// The guest CPU; the host's own (passthrough) when unset. VMs that
    /// should migrate get their cluster's baseline model.
    pub cpu: Option<crate::cpu::CpuModel>,
    /// Let disks use the host's page cache (`writeback`). Faster, but unsafe
    /// for live migration of shared disks, which need `none`.
    pub host_cache: bool,
    /// Throttling for each disk (not CD-ROMs).
    pub disk_limits: DiskLimits,
}

/// Disk throttling; `None`: no limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DiskLimits {
    pub iops: Option<u64>,
    pub bytes_per_sec: Option<u64>,
}

impl DomainDef {
    /// Libvirt domain name. Names must be unique per host, display names are
    /// not, so the name is derived from the UUID.
    pub fn name(&self) -> String {
        domain_name(&self.uuid)
    }

    pub fn to_xml(&self) -> String {
        let os = match self.firmware {
            Firmware::Uefi => El::new("os").attr("firmware", "efi").child(
                El::new("firmware").child(
                    El::new("feature")
                        .attr("enabled", "no")
                        .attr("name", "secure-boot"),
                ),
            ),
            Firmware::Bios => El::new("os"),
        }
        .child(
            El::new("type")
                .attr("arch", "x86_64")
                .attr("machine", "q35")
                .text("hvm"),
        );

        let disks = self.disks.iter().map(|d| {
            let device = match d.device {
                DiskDevice::Disk => "disk",
                DiskDevice::Cdrom => "cdrom",
            };
            let mut driver = El::new("driver")
                .attr("name", "qemu")
                .attr("type", &d.format);
            if d.device == DiskDevice::Disk {
                // Without the host page cache, disks on shared storage can
                // live-migrate safely; VMs that never migrate may use it.
                let cache = if self.host_cache { "writeback" } else { "none" };
                driver = driver.attr("cache", cache).attr("discard", "unmap");
            }
            let (kind, source) = match &d.source {
                DiskSource::File(path) => ("file", El::new("source").attr("file", path)),
                DiskSource::Rbd {
                    name,
                    monitors,
                    user,
                    secret_uuid,
                } => (
                    "network",
                    El::new("source")
                        .attr("protocol", "rbd")
                        .attr("name", name)
                        .children(
                            monitors
                                .iter()
                                .map(|(h, p)| El::new("host").attr("name", h).attr("port", p)),
                        )
                        .child(
                            El::new("auth").attr("username", user).child(
                                El::new("secret")
                                    .attr("type", "ceph")
                                    .attr("uuid", secret_uuid),
                            ),
                        ),
                ),
            };
            El::new("disk")
                .attr("type", kind)
                .attr("device", device)
                .child(driver)
                .child(source)
                .child(El::new("target").attr("dev", &d.target).attr("bus", &d.bus))
                .maybe(
                    (d.device == DiskDevice::Disk && self.disk_limits != DiskLimits::default())
                        .then(|| {
                            let l = self.disk_limits;
                            El::new("iotune")
                                .maybe(l.bytes_per_sec.map(|b| El::new("total_bytes_sec").text(b)))
                                .maybe(l.iops.map(|i| El::new("total_iops_sec").text(i)))
                        }),
                )
                .maybe(d.readonly.then(|| El::new("readonly")))
                .maybe(d.boot_order.map(|o| El::new("boot").attr("order", o)))
        });

        let nics = self.nics.iter().map(|n| {
            El::new("interface")
                .attr("type", "bridge")
                .child(El::new("mac").attr("address", &n.mac))
                .child(El::new("source").attr("bridge", &n.bridge))
                .child(
                    El::new("virtualport")
                        .attr("type", "openvswitch")
                        .child(El::new("parameters").attr("interfaceid", &n.interface_id)),
                )
                .child(El::new("model").attr("type", "virtio"))
        });

        El::new("domain")
            .attr("type", "kvm")
            .child(El::new("name").text(self.name()))
            .child(El::new("uuid").text(&self.uuid))
            .child(El::new("title").text(&self.title))
            .child(
                El::new("metadata").child(
                    El::new("mst:vm")
                        .attr("xmlns:mst", METADATA_NS)
                        .attr("id", &self.uuid)
                        .attr("tenant", &self.tenant_id)
                        .attr("generation", self.generation),
                ),
            )
            .child(El::new("memory").attr("unit", "MiB").text(self.memory_mib))
            .child(El::new("vcpu").attr("placement", "static").text(self.vcpus))
            .child(os)
            .child(
                El::new("features")
                    .child(El::new("acpi"))
                    .child(El::new("apic")),
            )
            .child(match &self.cpu {
                Some(cpu) => El::new("cpu")
                    .attr("mode", "custom")
                    .attr("match", "exact")
                    .attr("check", "partial")
                    .child(El::new("model").attr("fallback", "forbid").text(&cpu.model))
                    .maybe(cpu.vendor.as_ref().map(|v| El::new("vendor").text(v)))
                    .children(cpu.features.iter().map(|(policy, name)| {
                        El::new("feature").attr("policy", policy).attr("name", name)
                    })),
                None => El::new("cpu").attr("mode", "host-passthrough"),
            })
            .child(El::new("clock").attr("offset", "utc"))
            .child(El::new("on_poweroff").text("destroy"))
            .child(El::new("on_reboot").text("restart"))
            .child(El::new("on_crash").text("destroy"))
            .child(
                El::new("devices")
                    .children(disks)
                    .children(nics)
                    .child(
                        El::new("serial")
                            .attr("type", "pty")
                            .child(El::new("target").attr("port", 0)),
                    )
                    .child(
                        El::new("console")
                            .attr("type", "pty")
                            .child(El::new("target").attr("type", "serial").attr("port", 0)),
                    )
                    // No listener at all: the agent reaches the VNC server via
                    // virDomainOpenGraphicsFD, so nothing is exposed on the host.
                    .child(
                        El::new("graphics")
                            .attr("type", "vnc")
                            .child(El::new("listen").attr("type", "none")),
                    )
                    .child(El::new("video").child(El::new("model").attr("type", "virtio")))
                    .child(El::new("input").attr("type", "tablet").attr("bus", "usb"))
                    .child(
                        El::new("channel").attr("type", "unix").child(
                            El::new("target")
                                .attr("type", "virtio")
                                .attr("name", "org.qemu.guest_agent.0"),
                        ),
                    )
                    .child(
                        El::new("rng").attr("model", "virtio").child(
                            El::new("backend")
                                .attr("model", "random")
                                .text("/dev/urandom"),
                        ),
                    )
                    .child(El::new("memballoon").attr("model", "virtio")),
            )
            .render()
    }
}

pub fn domain_name(uuid: &str) -> String {
    format!("mst-{uuid}")
}

/// metastratum metadata recovered from a domain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub vm_id: String,
    pub tenant_id: String,
    pub generation: u64,
}

/// Parses the `<vm .../>` element returned by `virDomainGetMetadata`.
pub fn parse_metadata(xml: &str) -> Option<Metadata> {
    Some(Metadata {
        vm_id: attr(xml, "id")?,
        tenant_id: attr(xml, "tenant")?,
        generation: attr(xml, "generation")?.parse().ok()?,
    })
}

/// Extracts attribute `name` from the first element in `xml`. Only meant for
/// the flat metadata element libvirt hands back.
fn attr(xml: &str, name: &str) -> Option<String> {
    let start = xml.find('<')?;
    let end = xml[start..].find('>')? + start;
    let tag = &xml[start..end];
    for quote in ['"', '\''] {
        let needle = format!(" {name}={quote}");
        if let Some(i) = tag.find(&needle) {
            let rest = &tag[i + needle.len()..];
            let value = &rest[..rest.find(quote)?];
            return Some(
                value
                    .replace("&quot;", "\"")
                    .replace("&apos;", "'")
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&amp;", "&"),
            );
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DomainDef {
        DomainDef {
            uuid: "0199f0a2-7b3c-7d00-8000-000000000001".into(),
            tenant_id: "0199f0a2-7b3c-7d00-8000-0000000000aa".into(),
            title: "web <1> & co".into(),
            generation: 3,
            vcpus: 2,
            memory_mib: 2048,
            firmware: Firmware::Uefi,
            disks: vec![
                Disk {
                    device: DiskDevice::Disk,
                    source: DiskSource::File("/var/lib/metastratum/pools/local/vol-1.qcow2".into()),
                    format: "qcow2".into(),
                    target: "vda".into(),
                    bus: "virtio".into(),
                    boot_order: Some(1),
                    readonly: false,
                },
                Disk {
                    device: DiskDevice::Disk,
                    source: DiskSource::Rbd {
                        name: "metastratum/vol-2".into(),
                        monitors: vec![("10.77.0.10".into(), 6789)],
                        user: "metastratum".into(),
                        secret_uuid: "0199f0a2-7b3c-7d00-8000-0000000000c1".into(),
                    },
                    format: "raw".into(),
                    target: "vdb".into(),
                    bus: "virtio".into(),
                    boot_order: None,
                    readonly: false,
                },
                Disk {
                    device: DiskDevice::Cdrom,
                    source: DiskSource::File("/var/lib/metastratum/cloud-init/vm-1.iso".into()),
                    format: "raw".into(),
                    target: "sda".into(),
                    bus: "sata".into(),
                    boot_order: None,
                    readonly: true,
                },
            ],
            host_cache: false,
            disk_limits: DiskLimits::default(),
            cpu: Some(crate::cpu::CpuModel {
                model: "EPYC-Milan".into(),
                vendor: Some("AMD".into()),
                features: vec![
                    ("require".into(), "x2apic".into()),
                    ("disable".into(), "xsaves".into()),
                ],
            }),
            nics: vec![Nic {
                mac: "02:00:00:00:00:01".into(),
                bridge: "br-int".into(),
                interface_id: "0199f0a2-7b3c-7d00-8000-0000000000b1".into(),
            }],
        }
    }

    #[test]
    fn domain_xml_matches_golden() {
        let got = sample().to_xml();
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/domain.xml");
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::write(path, &got).unwrap();
        }
        let want = std::fs::read_to_string(path).expect("golden file; run with UPDATE_GOLDEN=1");
        assert_eq!(
            got, want,
            "domain XML changed; rerun with UPDATE_GOLDEN=1 if intended"
        );
    }

    /// libvirt keeps metadata only in a namespace it can resolve, and agents
    /// find their domains by it.
    #[test]
    fn metadata_is_in_our_namespace() {
        use quick_xml::events::Event;
        use quick_xml::name::{Namespace, ResolveResult};
        let xml = sample().to_xml();
        let mut reader = quick_xml::NsReader::from_str(&xml);
        loop {
            match reader.read_resolved_event().unwrap() {
                (ns, Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == "vm" => {
                    assert_eq!(ns, ResolveResult::Bound(Namespace(METADATA_NS)));
                    return;
                }
                (_, Event::Eof) => panic!("no metadata element in {xml}"),
                _ => {}
            }
        }
    }

    #[test]
    fn disks_are_throttled_to_their_limits() {
        let mut d = sample();
        d.disk_limits = DiskLimits {
            iops: Some(500),
            bytes_per_sec: Some(50 << 20),
        };
        let xml = d.to_xml();
        assert_eq!(
            xml.matches("<iotune>").count(),
            2,
            "both disks, not the CD-ROM: {xml}"
        );
        assert!(
            xml.contains("<total_bytes_sec>52428800</total_bytes_sec>"),
            "{xml}"
        );
        assert!(
            xml.contains("<total_iops_sec>500</total_iops_sec>"),
            "{xml}"
        );
        assert!(!sample().to_xml().contains("iotune"));
    }

    #[test]
    fn host_bound_vms_get_the_host_cpu_and_cache() {
        let mut d = sample();
        d.cpu = None;
        d.host_cache = true;
        let xml = d.to_xml();
        assert!(xml.contains("<cpu mode='host-passthrough'/>"), "{xml}");
        assert!(xml.contains("cache='writeback'"), "{xml}");
        assert!(!xml.contains("cache='none'"), "{xml}");
    }

    /// Validates against libvirt's RelaxNG schema when `virt-xml-validate` is
    /// installed (it ships with libvirt).
    #[test]
    fn domain_xml_is_schema_valid() {
        let dir = std::env::temp_dir().join(format!("mst-libvirt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("domain.xml");
        std::fs::write(&file, sample().to_xml()).unwrap();
        let out = match std::process::Command::new("virt-xml-validate")
            .arg(&file)
            .arg("domain")
            .output()
        {
            Ok(out) => out,
            Err(_) => return eprintln!("virt-xml-validate not installed; skipping"),
        };
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn metadata_roundtrips_libvirt_style() {
        // libvirt returns the element with double quotes and its own namespace.
        let xml = r#"<vm xmlns="urn:metastratum:vm:1" id="abc" tenant="t1" generation="7"/>"#;
        assert_eq!(
            parse_metadata(xml),
            Some(Metadata {
                vm_id: "abc".into(),
                tenant_id: "t1".into(),
                generation: 7
            })
        );
        assert_eq!(parse_metadata("<vm id='x'/>"), None);
    }
}
