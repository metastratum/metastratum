//! metastratum host agent. Runs on every hypervisor: drives libvirt, manages
//! local storage, and keeps a session open to the controller.

mod backup;
mod cleanup;
mod config;
mod console;
mod enroll;
mod facts;
mod ovn;
mod session;
mod snapshot;
mod tls;
mod transfer;
mod vm;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::bail;
use clap::Parser;
use mst_libvirt::Libvirt;
use mst_storage::Pools;
use tracing_subscriber::EnvFilter;

/// Crate version and the source revision it was built from.
const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("MST_SOURCE_REVISION"),
    ")"
);

#[derive(Parser)]
#[command(version = VERSION, about)]
struct Args {
    /// The configuration file (see deploy/config/agent.toml).
    #[arg(long, env = "MST_CONFIG", default_value = config::DEFAULT_PATH)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    tracing::info!(version = VERSION, "mst-agent starting");
    let cfg = config::Config::load(&args.config)?;
    let _ = rustls::crypto::ring::default_provider().install_default();

    let socket = cfg
        .libvirt_socket
        .clone()
        .unwrap_or_else(mst_libvirt::system_socket);
    let lv = Libvirt::new(socket, &cfg.libvirt_uri);
    let address = match cfg.address {
        Some(a) => a,
        None => facts::detect_address(&cfg.controllers[0])?,
    };
    let facts = facts::gather(&lv, address).await?;
    if !facts.kvm {
        tracing::warn!("/dev/kvm is missing; this host cannot run VMs");
    }

    let identity = match enroll::HostIdentity::load(&cfg.state_dir)? {
        Some(id) => id,
        None => {
            let Some(token) = &cfg.join_token else {
                bail!(
                    "host is not enrolled; set join_token in {} (from `mst-controller join-token` or the UI)",
                    args.config.display()
                );
            };
            let id =
                enroll::enroll(&cfg.controllers[0], token, facts.clone(), &cfg.state_dir).await?;
            tracing::info!(host = %id.host_id, "enrolled");
            id
        }
    };

    // libvirt and QEMU use the host identity for migration TLS.
    match tls::install(&identity) {
        Ok(true) => tracing::info!("installed the host certificate for libvirt and QEMU TLS"),
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "installing TLS certificates; live migration will fail")
        }
    }
    if let Err(e) = tls::restrict_libvirt_clients().await {
        tracing::warn!(error = %format!("{e:#}"), "restricting libvirt's TLS clients to hosts");
    }

    let pools = Pools::new(&cfg.data_dir);
    let agent = Arc::new(session::Agent {
        controllers: cfg.controllers,
        current: std::sync::atomic::AtomicUsize::new(0),
        identity: std::sync::RwLock::new(identity),
        state_dir: cfg.state_dir.clone(),
        reconnect: std::sync::Arc::default(),
        in_flight: Default::default(),
        facts,
        lv: lv.clone(),
        pools,
        max_image_bytes: cfg.image_max_gib << 30,
        ovn_sb_remote: Default::default(),
        qemu_tls: Default::default(),
        incoming: Default::default(),
        files: vm::VmFiles {
            cloud_init_dir: cfg.data_dir.join("cloud-init"),
        },
        changes: tokio::sync::Mutex::new(lv.watch()),
        libvirt_tls: tokio::sync::watch::Sender::new(None),
        consoles: Default::default(),
    });
    // VMs' files are root's and their own QEMU's only (made so since; older
    // ones are tightened here), and libvirt should confine each QEMU.
    if let Err(e) = agent.pools.local().init().await {
        tracing::warn!(error = %e, "making the VMs' files private");
    }
    if let Err(e) = mst_storage::private_dir(&agent.files.cloud_init_dir).await {
        tracing::warn!(error = %e, "making cloud-init seeds private");
    }
    match lv.security_models().await {
        Ok(m) if m.iter().any(|m| m == "apparmor" || m == "selinux") => {}
        Ok(m) => tracing::warn!(
            models = ?m,
            "libvirt confines no VM with AppArmor or SELinux: a guest that breaks out of QEMU reaches every VM's files on this host"
        ),
        Err(e) => tracing::warn!(error = %e, "asking libvirt how it confines VMs"),
    }
    tokio::spawn(agent.clone().keep_certificate_fresh());
    tokio::spawn(agent.clone().apply_libvirt_tls());
    tokio::select! {
        () = agent.run() => {},
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    Ok(())
}
