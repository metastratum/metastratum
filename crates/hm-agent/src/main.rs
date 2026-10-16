//! hypermon host agent. Runs on every hypervisor host, drives libvirt and
//! keeps a stream open to the controller.

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Controller agent endpoint.
    #[arg(long, env = "HM_CONTROLLER", default_value = "https://127.0.0.1:8443")]
    controller: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    tracing::info!(controller = %args.controller, "hm-agent starting (not yet implemented)");
    Ok(())
}
