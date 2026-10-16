//! hypermon control plane.

mod agents;
mod api;
mod authz;
mod ca;
mod engine;
mod error;
mod events;
mod fence;
mod ha;
mod metrics;
mod migration;
mod net;
mod ratelimit;
mod secrets;
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
use hm_proto::replica_control_server::ReplicaControlServer;
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
    /// File holding the master key that seals secrets in the database (the
    /// CA key, storage keys, identity provider secrets). Create one with
    /// `hm-controller gen-master-key`; keep it out of the database's backups.
    #[arg(long, global = true, env = "HM_MASTER_KEY_FILE")]
    master_key_file: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the controller.
    Serve(Box<ServeArgs>),
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
        /// Name of the cluster the host joins.
        #[arg(long)]
        cluster: Option<String>,
    },
    /// Print the OpenAPI document.
    Openapi,
    /// Print a new random master key, for --master-key-file.
    GenMasterKey,
}

#[derive(clap::Args)]
struct ServeArgs {
    /// Address for the HTTP API and web UI.
    #[arg(long, env = "HM_LISTEN", default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
    /// Address for the agent endpoint (gRPC over mutual TLS).
    #[arg(long, env = "HM_AGENT_LISTEN", default_value = "0.0.0.0:8443")]
    agent_listen: SocketAddr,
    /// Address other controller replicas reach this one's agent endpoint on,
    /// to forward commands for agents connected here. Defaults to the agent
    /// listen address, or the first agent name with its port.
    #[arg(long, env = "HM_PEER_ADDRESS")]
    peer_address: Option<String>,
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
    /// Address to serve Prometheus metrics on (`/metrics`), e.g.
    /// 127.0.0.1:9180. Kept off the API address: metrics name hosts and
    /// count resources across tenants.
    #[arg(long, env = "HM_METRICS_LISTEN")]
    metrics_listen: Option<SocketAddr>,
    /// Load balancers in front of the API (addresses or CIDRs), whose
    /// X-Forwarded-For header gives the client's address for rate limits.
    #[arg(long, env = "HM_TRUSTED_PROXIES", value_delimiter = ',')]
    trusted_proxies: Vec<ratelimit::Cidr>,
    /// Identity provider issuer URLs that sub-tenants may use although they
    /// are on internal addresses (other tenants' providers must be public;
    /// the provider tenant's may be anywhere).
    #[arg(long, env = "HM_TRUSTED_OIDC_ISSUERS", value_delimiter = ',')]
    trusted_oidc_issuers: Vec<String>,
    /// Base URL users reach the API and UI at (for OIDC redirect URIs).
    /// Defaults to http://<listen address>.
    #[arg(long, env = "HM_PUBLIC_URL")]
    public_url: Option<String>,
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
    if let Cmd::GenMasterKey = args.cmd {
        println!("{}", hm_pki::Sealer::generate_key());
        return Ok(());
    }
    // Before anything reads a secret (every remaining command uses the CA).
    secrets::init(args.master_key_file.as_deref().context(
        "--master-key-file or HM_MASTER_KEY_FILE is required (see `hm-controller gen-master-key`)",
    )?)?;

    let url = args
        .database_url
        .context("--database-url or DATABASE_URL is required")?;
    let db = hm_db::connect(&url, 16)
        .await
        .context("connecting to postgres")?;
    hm_db::migrate(&db).await.context("running migrations")?;

    match args.cmd {
        Cmd::Serve(s) => serve(db, *s).await,
        Cmd::Bootstrap {
            admin_user,
            admin_password,
        } => bootstrap(db, &admin_user, &admin_password).await,
        Cmd::JoinToken {
            ttl_minutes,
            cluster,
        } => {
            let ca = ca::load_or_create(&db).await?;
            let cluster = match cluster {
                Some(name) => Some(
                    hm_db::clusters::list(&db)
                        .await?
                        .into_iter()
                        .find(|c| c.name == name)
                        .with_context(|| format!("no cluster named {name}"))?
                        .id,
                ),
                None => None,
            };
            let token = hm_pki::JoinToken::generate(&ca.fingerprint());
            let expires = chrono::Utc::now() + chrono::Duration::minutes(ttl_minutes as i64);
            hm_db::join_tokens::create(
                &db,
                &token.id,
                &token.secret_hash(),
                None,
                expires,
                cluster,
            )
            .await?;
            println!("{token}");
            Ok(())
        }
        Cmd::Openapi | Cmd::GenMasterKey => unreachable!("handled above"),
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
    let admin = hm_db::users::create(
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
    hm_db::rbac::create_binding(&mut *tx, admin.id, root.id, "admin", None).await?;
    tx.commit().await?;
    ca::load_or_create(&db).await?;
    println!("provider tenant {} and user {user} ready", root.id);
    Ok(())
}

async fn serve(db: hm_db::sqlx::PgPool, args: ServeArgs) -> anyhow::Result<()> {
    let recorder = metrics::install()?;
    secrets::seal_existing(&db)
        .await
        .context("sealing stored secrets")?;
    let ca = ca::load_or_create(&db).await?;
    let (cert_chain, key) = ca::server_identity(&ca, &args.agent_names)?;
    let ca_pem = ca.cert_pem().to_owned();
    let replica = format!("{}-{}", hostname(), uuid::Uuid::now_v7().simple());
    let nb = std::sync::Arc::new(hm_ovsdb::Northbound::new(args.ovn_nb.clone()));
    let state = AppState::new(Inner {
        db: db.clone(),
        ca,
        agents: Agents::default(),
        net: net::Reconciler::new(db.clone(), nb),
        replica: replica.clone(),
        secure_cookies: args.secure_cookies,
        public_url: args
            .public_url
            .clone()
            .unwrap_or_else(|| format!("http://{}", args.listen)),
        events: events::Hub::default(),
        limits: ratelimit::Limits::default(),
        trusted_proxies: args.trusted_proxies.clone(),
        trusted_oidc_issuers: args.trusted_oidc_issuers.clone(),
    });
    tracing::info!(%replica, fingerprint = %state.ca.fingerprint(), "controller starting");

    // Other replicas forward commands for agents connected here, and this
    // one forwards to them, authenticated by replica certificates.
    let peer_address = match &args.peer_address {
        Some(a) => a.clone(),
        None if !args.agent_listen.ip().is_unspecified() => args.agent_listen.to_string(),
        None => format!(
            "{}:{}",
            args.agent_names.first().context(
                "--peer-address is needed when the agent endpoint listens on all addresses"
            )?,
            args.agent_listen.port()
        ),
    };
    let (replica_cert, replica_key) = state.ca.issue_client(
        &format!("{}{replica}", agents::peers::REPLICA_CN_PREFIX),
        Duration::from_secs(365 * 86400),
    )?;
    let peer_tls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(state.ca.cert_pem()))
        .identity(Identity::from_pem(replica_cert, replica_key))
        .domain_name(ca::SERVER_NAME);
    state.agents.set_peers(agents::peers::Peers::new(
        db.clone(),
        replica.clone(),
        peer_tls,
    ));
    hm_db::replicas::heartbeat(&db, &replica, &peer_address).await?;
    tokio::spawn({
        let (db, replica) = (db.clone(), replica.clone());
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if let Err(e) = hm_db::replicas::heartbeat(&db, &replica, &peer_address).await {
                    tracing::warn!(error = %e, "replica heartbeat");
                }
            }
        }
    });

    tokio::spawn(engine::run(state.clone()));
    tokio::spawn(housekeeping(state.clone()));
    tokio::spawn(state.events.clone().listen(db.clone()));
    tokio::spawn(state.events.clone().watch_hosts(db.clone()));
    tokio::spawn(state.net.clone().run());
    // Host failure handling runs on one replica at a time.
    tokio::spawn({
        let state = state.clone();
        ha::lead(db.clone(), move || ha::monitor(state.clone()))
    });

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
        .add_service(ReplicaControlServer::new(agents::ReplicaControlService(
            state.clone(),
        )))
        .serve_with_shutdown(args.agent_listen, shutdown());
    tracing::info!(addr = %args.agent_listen, "agent endpoint listening");

    let (api, _) = api::router();
    let mut app = Router::new().route("/healthz", get(healthz)).merge(api);
    if let Some(dir) = &args.web_dir {
        // Client-side routes fall back to the SPA entry point.
        app = app
            .fallback_service(ServeDir::new(dir).fallback(ServeFile::new(dir.join("index.html"))));
    }
    let app = app
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            ratelimit::limit,
        ))
        .layer(axum::middleware::from_fn(metrics::track_http))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            ratelimit::harden,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(addr = %args.listen, "HTTP API listening");
    let http = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown());

    let metrics_server = async {
        let Some(addr) = args.metrics_listen else {
            return anyhow::Ok(());
        };
        let app = Router::new()
            .route("/metrics", get(metrics::render))
            .with_state((state.clone(), recorder));
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!(%addr, "metrics listening");
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown())
            .await?;
        Ok(())
    };

    let (g, h, m) = tokio::join!(grpc, http, metrics_server);
    g?;
    h?;
    m?;
    Ok(())
}

/// Periodic cleanup that any replica may run.
async fn housekeeping(state: AppState) {
    loop {
        if let Err(e) = hm_db::sessions::purge_expired(&state.db).await {
            tracing::warn!(error = %e, "purging sessions");
        }
        if let Err(e) = hm_db::oidc::purge_expired(&state.db).await {
            tracing::warn!(error = %e, "purging OIDC logins");
        }
        if let Err(e) = hm_db::replicas::purge_stale(&state.db).await {
            tracing::warn!(error = %e, "purging gone replicas");
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
