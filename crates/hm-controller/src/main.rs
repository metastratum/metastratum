//! hypermon control plane.

mod agents;
mod api;
mod authz;
mod ca;
mod engine;
mod error;
mod state;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use clap::{Parser, Subcommand};
use hm_proto::agent_control_server::AgentControlServer;
use hm_proto::enrollment_server::EnrollmentServer;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::agents::{AgentControlService, Agents, EnrollmentService};
use crate::state::{AppState, Inner};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Postgres connection URL.
    #[arg(long, env = "DATABASE_URL", global = true)]
    database_url: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the controller.
    Serve(ServeArgs),
    /// Create the provider tenant and its first administrator.
    Bootstrap {
        #[arg(long, default_value = "admin")]
        admin_user: String,
        #[arg(long, env = "HM_ADMIN_PASSWORD")]
        admin_password: String,
    },
    /// Print a single-use host join token.
    JoinToken {
        /// Validity in minutes.
        #[arg(long, default_value_t = 60)]
        ttl_minutes: u64,
    },
    /// Print the OpenAPI document.
    Openapi,
}

#[derive(clap::Args)]
struct ServeArgs {
    /// Address for the HTTP API and web UI.
    #[arg(long, env = "HM_LISTEN", default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    /// Address for the agent endpoint (gRPC over mutual TLS).
    #[arg(long, env = "HM_AGENT_LISTEN", default_value = "0.0.0.0:8443")]
    agent_listen: SocketAddr,
    /// Extra DNS names / IPs for the agent endpoint certificate.
    #[arg(long, env = "HM_AGENT_NAMES", value_delimiter = ',')]
    agent_names: Vec<String>,
    /// OVN northbound database, e.g. tcp:10.77.0.10:6641.
    #[arg(long, env = "HM_OVN_NB")]
    ovn_nb: String,
    /// Built web UI to serve at `/`.
    #[arg(long, env = "HM_WEB_DIR")]
    web_dir: Option<PathBuf>,
    /// Mark session cookies Secure (when served over HTTPS).
    #[arg(long, env = "HM_SECURE_COOKIES")]
    secure_cookies: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,sqlx::postgres::notice=warn".into()),
        )
        .init();
    let args = Args::parse();

    if let Cmd::Openapi = args.cmd {
        println!("{}", api::router().1.to_pretty_json()?);
        return Ok(());
    }

    let url = args
        .database_url
        .context("--database-url or DATABASE_URL is required")?;
    let db = hm_db::connect(&url, 16)
        .await
        .context("connecting to postgres")?;
    hm_db::migrate(&db).await.context("running migrations")?;

    match args.cmd {
        Cmd::Serve(s) => serve(db, s).await,
        Cmd::Bootstrap {
            admin_user,
            admin_password,
        } => bootstrap(db, &admin_user, &admin_password).await,
        Cmd::JoinToken { ttl_minutes } => {
            let ca = ca::load_or_create(&db).await?;
            let token = hm_pki::JoinToken::generate(&ca.fingerprint());
            let expires = chrono::Utc::now() + chrono::Duration::minutes(ttl_minutes as i64);
            hm_db::join_tokens::create(&db, &token.id, &token.secret_hash(), None, expires).await?;
            println!("{token}");
            Ok(())
        }
        Cmd::Openapi => unreachable!("handled above"),
    }
}

async fn bootstrap(db: hm_db::sqlx::PgPool, user: &str, password: &str) -> anyhow::Result<()> {
    if password.len() < 12 {
        anyhow::bail!("admin password must be at least 12 characters");
    }
    let mut tx = db.begin().await?;
    let root = match hm_db::tenants::root(&mut *tx).await? {
        Some(r) => r,
        None => hm_db::tenants::create(&mut *tx, None, "provider", "Provider").await?,
    };
    let hash = api::hash_password(password)?;
    hm_db::users::create(
        &mut *tx,
        hm_db::users::NewUser {
            tenant_id: root.id,
            username: user,
            display_name: "Administrator",
            email: None,
            password_hash: Some(&hash),
        },
    )
    .await
    .context("creating admin user")?;
    tx.commit().await?;
    ca::load_or_create(&db).await?;
    println!("provider tenant {} and user {user} ready", root.id);
    Ok(())
}

async fn serve(db: hm_db::sqlx::PgPool, args: ServeArgs) -> anyhow::Result<()> {
    let ca = ca::load_or_create(&db).await?;
    let (cert_chain, key) = ca::server_identity(&ca, &args.agent_names)?;
    let ca_pem = ca.cert_pem().to_owned();
    let replica = format!("{}-{}", hostname(), uuid::Uuid::now_v7().simple());
    let state = AppState::new(Inner {
        db: db.clone(),
        ca,
        agents: Agents::default(),
        nb: hm_ovsdb::Northbound::new(args.ovn_nb.clone()),
        replica: replica.clone(),
        secure_cookies: args.secure_cookies,
    });
    tracing::info!(%replica, fingerprint = %state.ca.fingerprint(), "controller starting");

    tokio::spawn(engine::run(state.clone()));
    tokio::spawn(housekeeping(state.clone()));

    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(cert_chain, key))
        .client_ca_root(Certificate::from_pem(ca_pem))
        // Enrollment happens before the agent has a certificate; AgentControl
        // checks for one itself.
        .client_auth_optional(true);
    let grpc = Server::builder()
        .tls_config(tls)?
        .http2_keepalive_interval(Some(Duration::from_secs(15)))
        .add_service(EnrollmentServer::new(EnrollmentService(state.clone())))
        .add_service(AgentControlServer::new(AgentControlService(state.clone())))
        .serve_with_shutdown(args.agent_listen, shutdown());
    tracing::info!(addr = %args.agent_listen, "agent endpoint listening");

    let (api, _) = api::router();
    let mut app = Router::new().route("/healthz", get(healthz)).merge(api);
    if let Some(dir) = &args.web_dir {
        // Client-side routes fall back to the SPA entry point.
        app = app
            .fallback_service(ServeDir::new(dir).fallback(ServeFile::new(dir.join("index.html"))));
    }
    let app = app.layer(TraceLayer::new_for_http()).with_state(state);
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(addr = %args.listen, "HTTP API listening");
    let http = axum::serve(listener, app).with_graceful_shutdown(shutdown());

    let (g, h) = tokio::join!(grpc, http);
    g?;
    h?;
    Ok(())
}

/// Periodic cleanup that any replica may run.
async fn housekeeping(state: AppState) {
    loop {
        if let Err(e) = hm_db::sessions::purge_expired(&state.db).await {
            tracing::warn!(error = %e, "purging sessions");
        }
        tokio::time::sleep(Duration::from_secs(600)).await;
    }
}

async fn healthz(State(state): State<AppState>) -> StatusCode {
    match hm_db::sqlx::query("SELECT 1").execute(&state.db).await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "controller".into())
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
