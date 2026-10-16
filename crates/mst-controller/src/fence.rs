//! Fencing: powering off a failed host before its VMs start elsewhere.
//!
//! A host that stops answering may still be running its VMs (a network
//! split, a hung agent). Starting them elsewhere would run them twice,
//! writing to the same disks. So a host is fenced first, through a channel
//! independent of its operating system, and its VMs move only once that
//! channel confirms the host is off.
//!
//! - `redfish`: the host's BMC. `{"url": "https://bmc", "system":
//!   "/redfish/v1/Systems/1" (optional), "username", "password",
//!   "insecure": false}`.
//! - `libvirt`: hosts that are VMs themselves, as in the lab. `{"socket":
//!   "/run/libvirt/virtqemud-sock", "domain": "mst-host1"}`, on the
//!   controller's machine.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

/// How long a host gets to report being off.
const CONFIRM: Duration = Duration::from_secs(90);

pub const SECRET_FIELDS: &[&str] = &["password"];

pub fn secret_context(host: mst_core::ids::HostId) -> String {
    format!("host.fence.password:{host}")
}

#[derive(Deserialize)]
struct Redfish {
    url: String,
    system: Option<String>,
    username: String,
    password: String,
    #[serde(default)]
    insecure: bool,
}

#[derive(Deserialize)]
struct LibvirtFence {
    #[serde(default = "default_socket")]
    socket: String,
    domain: String,
}

fn default_socket() -> String {
    mst_libvirt::system_socket().to_string_lossy().into_owned()
}

/// Checks a configuration is complete; returns it with secrets left as they
/// are (the caller seals them).
pub fn validate(kind: &str, config: &Value) -> Result<(), String> {
    match kind {
        "redfish" => serde_json::from_value::<Redfish>(config.clone())
            .map(drop)
            .map_err(|e| format!("redfish fencing needs url, username and password: {e}")),
        "libvirt" => serde_json::from_value::<LibvirtFence>(config.clone())
            .map(drop)
            .map_err(|e| format!("libvirt fencing needs the host's domain name: {e}")),
        _ => Err("fencing kind must be redfish or libvirt".into()),
    }
}

/// The host's power state as the fencing channel sees it: `on` or `off`.
pub async fn power_state(kind: &str, config: &Value) -> Result<String> {
    match kind {
        "redfish" => {
            let r: Redfish = serde_json::from_value(config.clone())?;
            let (client, system) = redfish(&r).await?;
            let s: Value = client
                .get(format!("{}{system}", r.url.trim_end_matches('/')))
                .basic_auth(&r.username, Some(&r.password))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            Ok(match s["PowerState"].as_str() {
                Some("Off") => "off".into(),
                Some(_) => "on".into(),
                None => bail!("the BMC reports no power state"),
            })
        }
        "libvirt" => {
            let l: LibvirtFence = serde_json::from_value(config.clone())?;
            let (c, dom) = libvirt(&l).await?;
            let st = c
                .domain_get_state(&mst_virtrpc::protocol::DomainGetStateArgs { dom, flags: 0 })
                .await?;
            // VIR_DOMAIN_SHUTOFF
            Ok(if st.state == 5 { "off" } else { "on" }.into())
        }
        other => bail!("unknown fencing kind {other}"),
    }
}

/// Powers the host off and waits until the channel confirms it.
pub async fn power_off(kind: &str, config: &Value) -> Result<()> {
    match kind {
        "redfish" => {
            let r: Redfish = serde_json::from_value(config.clone())?;
            let (client, system) = redfish(&r).await?;
            client
                .post(format!(
                    "{}{system}/Actions/ComputerSystem.Reset",
                    r.url.trim_end_matches('/')
                ))
                .basic_auth(&r.username, Some(&r.password))
                .json(&json!({"ResetType": "ForceOff"}))
                .send()
                .await?
                .error_for_status()
                .context("asking the BMC to power off")?;
        }
        "libvirt" => {
            let l: LibvirtFence = serde_json::from_value(config.clone())?;
            let (c, dom) = libvirt(&l).await?;
            match c
                .domain_destroy(&mst_virtrpc::protocol::DomainDestroyArgs { dom })
                .await
            {
                Ok(_) => {}
                // Already off.
                Err(e) if e.code() == Some(mst_virtrpc::err::OPERATION_INVALID) => {}
                Err(e) => return Err(e).context("destroying the host's domain"),
            }
        }
        other => bail!("unknown fencing kind {other}"),
    }
    let deadline = tokio::time::Instant::now() + CONFIRM;
    loop {
        if power_state(kind, config).await.ok().as_deref() == Some("off") {
            return Ok(());
        }
        if tokio::time::Instant::now() > deadline {
            bail!("the host did not report being off within {CONFIRM:?}");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn redfish(r: &Redfish) -> Result<(reqwest::Client, String)> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        // BMCs commonly use self-signed certificates.
        .danger_accept_invalid_certs(r.insecure)
        .build()?;
    let system = match &r.system {
        Some(s) => s.clone(),
        None => {
            // The first (usually only) system.
            let systems: Value = client
                .get(format!(
                    "{}/redfish/v1/Systems",
                    r.url.trim_end_matches('/')
                ))
                .basic_auth(&r.username, Some(&r.password))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            systems["Members"][0]["@odata.id"]
                .as_str()
                .context("the BMC lists no systems")?
                .to_owned()
        }
    };
    Ok((client, system))
}

async fn libvirt(
    l: &LibvirtFence,
) -> Result<(mst_virtrpc::Client, mst_virtrpc::protocol::NonnullDomain)> {
    let c = mst_virtrpc::Client::connect(&l.socket, "qemu:///system")
        .await
        .with_context(|| format!("connecting to libvirt at {}", l.socket))?;
    let dom = c
        .domain_lookup_by_name(&mst_virtrpc::protocol::DomainLookupByNameArgs {
            name: l.domain.clone(),
        })
        .await
        .with_context(|| format!("finding domain {}", l.domain))?
        .dom;
    Ok((c, dom))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::Json;
    use axum::extract::State;
    use axum::routing::{get, post};

    use super::*;

    /// A BMC that powers off on request.
    async fn bmc() -> (String, Arc<Mutex<&'static str>>) {
        let power = Arc::new(Mutex::new("On"));
        let app = axum::Router::new()
            .route(
                "/redfish/v1/Systems",
                get(|| async { Json(json!({"Members": [{"@odata.id": "/redfish/v1/Systems/1"}]})) }),
            )
            .route(
                "/redfish/v1/Systems/1",
                get(|State(p): State<Arc<Mutex<&'static str>>>| async move {
                    Json(json!({"PowerState": *p.lock().unwrap()}))
                }),
            )
            .route(
                "/redfish/v1/Systems/1/Actions/ComputerSystem.Reset",
                post(|State(p): State<Arc<Mutex<&'static str>>>, Json(body): Json<Value>| async move {
                    assert_eq!(body["ResetType"], "ForceOff");
                    *p.lock().unwrap() = "Off";
                }),
            )
            .with_state(power.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), power)
    }

    #[tokio::test]
    async fn redfish_fencing_powers_off_and_confirms() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (url, power) = bmc().await;
        let config = json!({"url": url, "username": "admin", "password": "pw"});
        assert!(validate("redfish", &config).is_ok());
        assert_eq!(power_state("redfish", &config).await.unwrap(), "on");
        power_off("redfish", &config).await.unwrap();
        assert_eq!(*power.lock().unwrap(), "Off");
        assert_eq!(power_state("redfish", &config).await.unwrap(), "off");
    }

    #[test]
    fn configs_are_checked() {
        assert!(validate("redfish", &json!({"url": "https://bmc"})).is_err());
        assert!(validate("libvirt", &json!({"domain": "mst-host1"})).is_ok());
        assert!(validate("ipmi", &json!({})).is_err());
    }
}
