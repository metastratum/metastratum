//! What the agent reports about its host.

use std::net::{IpAddr, UdpSocket};

use anyhow::{Context, Result};
use mst_libvirt::Libvirt;
use mst_proto::HostFacts;

/// The local address used to reach `controller`, which is also how other
/// hosts and the controller can reach this one. No packets are sent.
pub fn detect_address(controller: &str) -> Result<IpAddr> {
    let uri: tonic::transport::Uri = controller.parse().context("invalid controller URL")?;
    let host = uri
        .host()
        .context("controller URL has no host")?
        .trim_matches(['[', ']']);
    let port = uri.port_u16().unwrap_or(443);
    let sock = UdpSocket::bind(("0.0.0.0", 0))?;
    sock.connect((host, port))?;
    Ok(sock.local_addr()?.ip())
}

fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_owned())
        })
        .unwrap_or_default()
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

/// The OVN chassis name (`external_ids:system-id`), if OVS is configured.
pub async fn ovn_chassis() -> String {
    ovs_external_id("system-id").await
}

/// Physical networks from `external_ids:ovn-bridge-mappings`
/// (`physnet:bridge,...`).
pub async fn physical_networks() -> Vec<String> {
    ovs_external_id("ovn-bridge-mappings")
        .await
        .split(',')
        .filter_map(|m| m.split_once(':').map(|(net, _)| net.trim().to_owned()))
        .filter(|n| !n.is_empty())
        .collect()
}

async fn ovs_external_id(key: &str) -> String {
    let out = tokio::process::Command::new("ovs-vsctl")
        .args([
            "--if-exists",
            "get",
            "Open_vSwitch",
            ".",
            &format!("external_ids:{key}"),
        ])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .trim()
            .trim_matches('"')
            .to_owned(),
        _ => String::new(),
    }
}

/// What guests can have here beyond the basics (none if libvirt cannot say).
pub async fn guest_support(lv: &Libvirt) -> mst_libvirt::xml::GuestSupport {
    match lv.guest_support().await {
        Ok(s) => {
            if !s.tpm {
                tracing::warn!("guests can have no TPM here: install swtpm (and swtpm-tools)");
            }
            if !s.secure_boot {
                tracing::warn!(
                    "guests can have no Secure Boot here: install UEFI firmware with it (OVMF/edk2)"
                );
            }
            s
        }
        Err(e) => {
            tracing::warn!(error = %e, "asking libvirt what guests can have");
            Default::default()
        }
    }
}

pub async fn gather(lv: &Libvirt, address: IpAddr) -> Result<HostFacts> {
    let node = lv.node_facts().await.context("querying libvirt")?;
    let cpu = match lv.host_cpu().await {
        Ok(cpu) => cpu,
        Err(e) => {
            tracing::warn!(error = %e, "reading the host CPU model");
            None
        }
    };
    Ok(HostFacts {
        cpu_json: cpu
            .map(|c| serde_json::to_string(&crate::vm::cpu_spec(c)).unwrap_or_default())
            .unwrap_or_default(),
        // Checked again for every session; see Agent::session.
        migration_ready: crate::tls::ready_for_migration(),
        tpm: false,
        secure_boot: false,
        hostname: hostname(),
        address: address.to_string(),
        cpu_count: node.cpus,
        memory_mib: node.memory_mib,
        cpu_model: cpu_model(),
        kvm: std::path::Path::new("/dev/kvm").exists(),
        libvirt_version: node.libvirt_version,
        ovn_chassis: ovn_chassis().await,
        physical_networks: physical_networks().await,
        agent_version: crate::VERSION.to_owned(),
    })
}
