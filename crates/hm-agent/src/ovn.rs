//! ovn-controller's access to the OVN southbound database, when the
//! controller manages it (its Welcome names the southbound remote).
//!
//! The host gets a certificate from the controller's OVN CA naming its
//! chassis, which OVN's RBAC (`role=ovn-controller`) holds it to: it may
//! change its own chassis' records and no one else's. Open vSwitch is set up
//! to use it, and ovn-controller pointed at the remote.

use std::path::Path;

use anyhow::{Context, Result, bail};
use hm_proto::agent_control_client::AgentControlClient;

use crate::tls::{Access, group_id, write_if_changed};

const DIR: &str = "/etc/openvswitch/hypermon";

fn path(file: &str) -> String {
    format!("{DIR}/{file}")
}

/// Makes ovn-controller reach the southbound database at `remote` with a
/// current certificate for this chassis, renewing it when a third of its
/// lifetime is left or the chassis was renamed.
pub async fn ensure(agent: &crate::session::Agent, remote: &str) -> Result<()> {
    let chassis = crate::facts::ovn_chassis().await;
    if chassis.is_empty() {
        bail!("Open vSwitch has no system-id (chassis name) yet");
    }
    let current = std::fs::read_to_string(path("cert.pem")).ok();
    if current.as_deref().is_none_or(|pem| !fits(pem, &chassis)) {
        let (key_pem, csr_pem) = hm_pki::new_key_and_csr(&chassis).context("making a key")?;
        let creds = AgentControlClient::new(agent.channel().await?)
            .ovn_credentials(hm_proto::OvnCredentialsRequest { csr_pem })
            .await
            .context("asking for an OVN certificate")?
            .into_inner();
        // ovs-vswitchd and ovn-controller run as root, or as openvswitch.
        let ovs = Access::Group(group_id(&["openvswitch"]));
        write_if_changed(Path::new(&path("ca.pem")), &creds.ca_pem, Access::Public)?;
        write_if_changed(Path::new(&path("key.pem")), &key_pem, ovs)?;
        write_if_changed(
            Path::new(&path("cert.pem")),
            &creds.cert_pem,
            Access::Public,
        )?;
        tracing::info!(%chassis, "installed a certificate for the OVN southbound database");
    }
    let ssl = vsctl(&["get-ssl"]).await?;
    if !ssl.contains(&path("key.pem")) || !ssl.contains(&path("cert.pem")) {
        // Replaces any SSL configuration there was.
        vsctl(&[
            "set-ssl",
            &path("key.pem"),
            &path("cert.pem"),
            &path("ca.pem"),
        ])
        .await?;
    }
    let current_remote = vsctl(&[
        "--if-exists",
        "get",
        "Open_vSwitch",
        ".",
        "external_ids:ovn-remote",
    ])
    .await?
    .trim()
    .trim_matches('"')
    .to_owned();
    if current_remote != remote {
        vsctl(&[
            "set",
            "Open_vSwitch",
            ".",
            &format!("external_ids:ovn-remote=\"{remote}\""),
        ])
        .await?;
        tracing::info!(%remote, "ovn-controller now reaches the southbound database over TLS");
    }
    Ok(())
}

/// Whether a certificate names `chassis` and has more than a third of its
/// lifetime left.
fn fits(cert_pem: &str, chassis: &str) -> bool {
    let Ok(der) = hm_pki::pem_to_der(cert_pem) else {
        return false;
    };
    let named = hm_pki::common_name(&der).is_ok_and(|cn| cn == chassis);
    let fresh = hm_pki::validity_of(&der).is_ok_and(|(from, until)| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        until - now > (until - from) / 3
    });
    named && fresh
}

async fn vsctl(args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new("ovs-vsctl")
        .args(args)
        .output()
        .await
        .context("running ovs-vsctl")?;
    if !out.status.success() {
        bail!(
            "ovs-vsctl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
