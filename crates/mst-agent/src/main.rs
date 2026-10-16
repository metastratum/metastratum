//! metastratum host agent. Runs on every hypervisor: drives libvirt, manages
//! local storage, and keeps a session open to the controller.

mod cleanup;
mod console;
mod enroll;
mod facts;
mod ovn;
mod session;
mod tls;
mod vm;

use std::net::IpAddr;
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
    /// Controller agent endpoints; with several (comma-separated), the agent
    /// moves to the next when one is unreachable.
    #[arg(long, env = "MST_CONTROLLER", value_delimiter = ',', required = true)]
    controller: Vec<String>,
    /// Join token; only needed for the first start.
    #[arg(long, env = "MST_JOIN_TOKEN")]
    join_token: Option<String>,
    /// Address the controller and other hosts reach this host on. Detected
    /// from the route to the controller by default.
    #[arg(long, env = "MST_ADDRESS")]
    address: Option<IpAddr>,
    /// Agent identity (key, certificates).
    #[arg(
        long,
        env = "MST_STATE_DIR",
        default_value = "/var/lib/metastratum-agent"
    )]
    state_dir: PathBuf,
    /// VM data: images, volumes, cloud-init seeds.
    #[arg(long, env = "MST_DATA_DIR", default_value = "/var/lib/metastratum")]
    data_dir: PathBuf,
    /// Largest image the host downloads, in GiB.
    #[arg(long, env = "MST_IMAGE_MAX_GIB", default_value_t = 64)]
    image_max_gib: u64,
    #[arg(long, env = "MST_LIBVIRT_URI", default_value = "qemu:///system")]
    libvirt_uri: String,
    /// libvirt daemon socket. Defaults to virtqemud's, or libvirtd's on
    /// monolithic installs.
    #[arg(long, env = "MST_LIBVIRT_SOCKET")]
    libvirt_socket: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    tracing::info!(version = VERSION, "mst-agent starting");
    let _ = rustls::crypto::ring::default_provider().install_default();

    let socket = args
        .libvirt_socket
        .clone()
        .unwrap_or_else(mst_libvirt::system_socket);
    let lv = Libvirt::new(socket, &args.libvirt_uri);
    let address = match args.address {
        Some(a) => a,
        None => facts::detect_address(&args.controller[0])?,
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
                    "host is not enrolled; pass --join-token (from `mst-controller join-token` or the UI)"
                );
            };
            let id =
                enroll::enroll(&args.controller[0], token, facts.clone(), &args.state_dir).await?;
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

    let pools = Pools::new(&args.data_dir);
    let agent = Arc::new(session::Agent {
        controllers: args.controller,
        current: std::sync::atomic::AtomicUsize::new(0),
        identity: std::sync::RwLock::new(identity),
        state_dir: args.state_dir.clone(),
        reconnect: std::sync::Arc::default(),
        in_flight: Default::default(),
        facts,
        lv: lv.clone(),
        pools,
        max_image_bytes: args.image_max_gib << 30,
        ovn_sb_remote: Default::default(),
        qemu_tls: Default::default(),
        files: vm::VmFiles {
            cloud_init_dir: args.data_dir.join("cloud-init"),
        },
        changes: tokio::sync::Mutex::new(lv.watch()),
    });
    tokio::spawn(agent.clone().keep_certificate_fresh());
    tokio::select! {
        () = agent.run() => {},
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    Ok(())
}
