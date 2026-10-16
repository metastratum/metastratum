//! hypermon host agent. Runs on every hypervisor: drives libvirt, manages
//! local storage, and keeps a session open to the controller.

mod console;
mod enroll;
mod facts;
mod session;
mod vm;

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::Parser;
use hm_libvirt::Libvirt;
use hm_storage::LocalPool;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Controller agent endpoint.
    #[arg(long, env = "HM_CONTROLLER")]
    controller: String,
    /// Join token; only needed for the first start.
    #[arg(long, env = "HM_JOIN_TOKEN")]
    join_token: Option<String>,
    /// Address the controller and other hosts reach this host on. Detected
    /// from the route to the controller by default.
    #[arg(long, env = "HM_ADDRESS")]
    address: Option<IpAddr>,
    /// Agent identity (key, certificates).
    #[arg(long, env = "HM_STATE_DIR", default_value = "/var/lib/hypermon-agent")]
    state_dir: PathBuf,
    /// VM data: images, volumes, cloud-init seeds.
    #[arg(long, env = "HM_DATA_DIR", default_value = "/var/lib/hypermon")]
    data_dir: PathBuf,
    #[arg(long, env = "HM_LIBVIRT_URI", default_value = "qemu:///system")]
    libvirt_uri: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    let _ = rustls::crypto::ring::default_provider().install_default();

    let lv = Libvirt::new(&args.libvirt_uri);
    let address = match args.address {
        Some(a) => a,
        None => facts::detect_address(&args.controller)?,
    };
    let facts = facts::gather(&lv, address).await?;
    if !facts.kvm {
        tracing::warn!("/dev/kvm is missing; this host cannot run VMs");
    }

    let identity = match enroll::HostIdentity::load(&args.state_dir)? {
        Some(id) => id,
        None => {
            let Some(token) = &args.join_token else {
                bail!(
                    "host is not enrolled; pass --join-token (from `hm-controller join-token` or the UI)"
                );
            };
            let id =
                enroll::enroll(&args.controller, token, facts.clone(), &args.state_dir).await?;
            tracing::info!(host = %id.host_id, "enrolled");
            id
        }
    };

    let pool = LocalPool::new(&args.data_dir);
    pool.init().await.context("preparing storage pool")?;
    let agent = Arc::new(session::Agent {
        controller: args.controller,
        identity,
        facts,
        lv,
        pool,
        files: vm::VmFiles {
            cloud_init_dir: args.data_dir.join("cloud-init"),
        },
    });
    tokio::select! {
        () = agent.run() => {},
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    Ok(())
}
