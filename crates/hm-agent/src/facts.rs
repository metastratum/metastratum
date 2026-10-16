//! What the agent reports about its host.

use std::net::{IpAddr, UdpSocket};

use anyhow::{Context, Result};
use hm_libvirt::Libvirt;
use hm_proto::HostFacts;

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
async fn ovn_chassis() -> String {
    let out = tokio::process::Command::new("ovs-vsctl")
        .args([
            "--if-exists",
            "get",
            "Open_vSwitch",
            ".",
            "external_ids:system-id",
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

pub async fn gather(lv: &Libvirt, address: IpAddr) -> Result<HostFacts> {
    let node = lv.node_facts().await.context("querying libvirt")?;
    Ok(HostFacts {
        hostname: hostname(),
        address: address.to_string(),
        cpu_count: node.cpus,
        memory_mib: node.memory_mib,
        cpu_model: cpu_model(),
        kvm: std::path::Path::new("/dev/kvm").exists(),
        libvirt_version: node.libvirt_version,
        ovn_chassis: ovn_chassis().await,
        agent_version: crate::VERSION.to_owned(),
    })
}
