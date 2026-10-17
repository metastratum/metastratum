//! metastratum control plane.

mod agents;
mod api;
mod authz;
mod ca;
mod cleanup;
mod config;
mod crl;
mod doctor;
mod engine;
mod error;
mod events;
mod fence;
mod ha;
mod limits;
mod metrics;
mod migration;
mod net;
mod ovn_tls;
mod port_watch;
mod ratelimit;
mod secrets;
mod state;
mod two_factor;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use clap::{Parser, Subcommand};
use mst_proto::agent_control_server::AgentControlServer;
use mst_proto::enrollment_server::EnrollmentServer;
use mst_proto::replica_control_server::ReplicaControlServer;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::agents::{AgentControlService, Agents, EnrollmentService};
use crate::state::{AppState, Inner};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// The configuration file (see deploy/config/controller.toml).
    #[arg(long, global = true, env = "MST_CONFIG", default_value = config::DEFAULT_PATH)]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the controller.
    Serve,
    /// Create the provider tenant and its first administrator.
    Bootstrap {
        #[arg(long, default_value = "admin")]
        admin_user: String,
        #[arg(long, env = "MST_ADMIN_PASSWORD")]
        admin_password: String,
    },
    /// Remove a user's second factors and backup codes, for someone locked
    /// out with no administrator to reset them (signing them out too).
    ResetTwoFactor {
        username: String,
        /// The user's tenant as a slug path from the provider (`acme/dev`);
        /// the provider by default.
        #[arg(long, default_value = "")]
        tenant: String,
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
    /// Check this installation against the deployment checklist in
    /// SECURITY.md and say what to fix. Exits 0 when all is in order, 1 with
    /// warnings, 2 with failures.
    Doctor,
    /// Print the OpenAPI document.
    Openapi,
    /// Print a new random master key, for the configuration's master_key_file.
    GenMasterKey,
    /// Write what an OVN central node needs for TLS: certificates for its
    /// northbound and southbound databases from the controller's CAs, the
    /// CAs its clients' certificates come from, and setup.sh, which installs
    /// them and switches on the TLS listeners (run it as root on the node).
    /// Do this for each node, and again before the certificates expire: the
    /// controller warns a month ahead.
    OvnCertificate {
        /// The node's name (the certificates' common name).
        #[arg(long)]
        name: String,
        /// Addresses and further names the node is reached at, comma-separated.
        #[arg(long, value_delimiter = ',')]
        address: Vec<String>,
        #[arg(long)]
        out_dir: PathBuf,
        /// Validity in days.
        #[arg(long, default_value_t = 365)]
        days: u64,
        /// Port of the northbound TLS listener.
        #[arg(long, default_value_t = 6641)]
        nb_port: u16,
        /// Port of the southbound TLS listener.
        #[arg(long, default_value_t = 6642)]
        sb_port: u16,
    },
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
        println!("{}", mst_pki::Sealer::generate_key());
        return Ok(());
    }
    let cfg = config::Config::load(&args.config)?;
    // Before anything reads a secret (every remaining command uses the CA).
    secrets::init(&cfg.master_key_file)?;

    let db = mst_db::connect(&cfg.database_url, 16)
        .await
        .context("connecting to postgres")?;
    mst_db::migrate(&db).await.context("running migrations")?;

    match args.cmd {
        Cmd::Serve => serve(db, &cfg).await,
        Cmd::Doctor => {
            let report = doctor::run(&cfg, &args.config, &db).await?;
            print!("{}", report.render());
            std::process::exit(match report.worst() {
                doctor::Level::Ok => 0,
                doctor::Level::Warn => 1,
                doctor::Level::Fail => 2,
            });
        }
        Cmd::Bootstrap {
            admin_user,
            admin_password,
        } => bootstrap(db, &admin_user, &admin_password).await,
        Cmd::ResetTwoFactor { username, tenant } => reset_two_factor(&db, &tenant, &username).await,
        Cmd::JoinToken {
            ttl_minutes,
            cluster,
        } => {
            let ca = ca::load_or_create(&db).await?;
            let cluster = match cluster {
                Some(name) => Some(
                    mst_db::clusters::list(&db)
                        .await?
                        .into_iter()
                        .find(|c| c.name == name)
                        .with_context(|| format!("no cluster named {name}"))?
                        .id,
                ),
                None => None,
            };
            let token = mst_pki::JoinToken::generate(&ca.fingerprint());
            let expires = chrono::Utc::now() + chrono::Duration::minutes(ttl_minutes as i64);
            mst_db::join_tokens::create(
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
        Cmd::OvnCertificate {
            name,
            address,
            out_dir,
            days,
            nb_port,
            sb_port,
        } => {
            let nb_ca = ca::load_or_create_ovn_nb(&db).await?;
            let sb_ca = ca::load_or_create_ovn(&db).await?;
            let mut names = vec![name];
            names.extend(address.into_iter().filter(|a| !a.is_empty()));
            ovn_tls::Bundle {
                names: &names,
                lifetime: Duration::from_secs(days * 86400),
                nb_port,
                sb_port,
            }
            .write(&nb_ca, &sb_ca, &out_dir)?;
            println!(
                "wrote certificates for {} and setup.sh to {}; copy the directory to the OVN node and run setup.sh there as root",
                names.join(", "),
                out_dir.display()
            );
            Ok(())
        }
        Cmd::Openapi | Cmd::GenMasterKey => unreachable!("handled above"),
    }
}

async fn bootstrap(db: mst_db::sqlx::PgPool, user: &str, password: &str) -> anyhow::Result<()> {
    if password.len() < 12 {
        anyhow::bail!("admin password must be at least 12 characters");
    }
    let mut tx = db.begin().await?;
    let root = match mst_db::tenants::root(&mut *tx).await? {
        Some(r) => r,
        None => mst_db::tenants::create(&mut *tx, None, "provider", "Provider").await?,
    };
    let hash = api::hash_password(password)?;
    let admin = mst_db::users::create(
        &mut *tx,
        mst_db::users::NewUser {
            tenant_id: root.id,
            username: user,
            display_name: "Administrator",
            email: None,
            password_hash: Some(&hash),
        },
    )
    .await
    .context("creating admin user")?;
    mst_db::rbac::create_binding(&mut *tx, admin.id, root.id, "admin", None).await?;
    tx.commit().await?;
    ca::load_or_create(&db).await?;
    println!("provider tenant {} and user {user} ready", root.id);
    Ok(())
}

async fn reset_two_factor(
    db: &mst_db::sqlx::PgPool,
    tenant: &str,
    username: &str,
) -> anyhow::Result<()> {
    let mut t = mst_db::tenants::root(db)
        .await?
        .context("not bootstrapped yet")?;
    for slug in tenant.split('/').filter(|s| !s.is_empty()) {
        t = mst_db::tenants::child_by_slug(db, Some(t.id), slug)
            .await?
            .with_context(|| format!("no tenant {tenant}"))?;
    }
    let user = mst_db::users::find_by_username(db, t.id, username)
        .await?
        .with_context(|| format!("no user {username} in {}", t.name))?;
    let mut tx = db.begin().await?;
    let n = mst_db::two_factor::reset(&mut tx, user.id).await?;
    mst_db::sessions::delete_for_user(&mut *tx, user.id, None).await?;
    let entry = mst_db::audit::Entry {
        user_id: None,
        username: None,
        tenant_id: Some(user.tenant_id),
        action: "user.two_factor.reset",
        target: Some(("user", user.id.0)),
        success: true,
        details: serde_json::json!({"username": user.username, "factors": n, "via": "command line"}),
    };
    mst_db::audit::record(&mut *tx, entry).await?;
    tx.commit().await?;
    println!("removed {n} second factor(s) and the backup codes of {username}");
    Ok(())
}

async fn serve(db: mst_db::sqlx::PgPool, cfg: &config::Config) -> anyhow::Result<()> {
    let recorder = metrics::install()?;
    secrets::seal_existing(&db)
        .await
        .context("sealing stored secrets")?;
    let ca = ca::load_or_create(&db).await?;
    let ovn_ca = ca::load_or_create_ovn(&db).await?;
    let (cert_chain, key) = ca::server_identity(&ca)?;
    let ca_pem = ca.cert_pem().to_owned();
    let replica = format!("{}-{}", hostname(), uuid::Uuid::now_v7().simple());
    // The northbound database over TLS: with a client certificate this
    // replica issues itself from the northbound CA, and renews.
    let ovn_nb_ca = std::sync::Arc::new(ca::load_or_create_ovn_nb(&db).await?);
    let nb_tls = cfg.northbound().contains("ssl:");
    let nb = std::sync::Arc::new(mst_ovsdb::Northbound::new(
        &cfg.northbound(),
        if nb_tls {
            Some(ovn_tls::nb_client(&ovn_nb_ca)?)
        } else {
            None
        },
    ));
    if nb_tls {
        tokio::spawn(ovn_tls::keep_nb_client_fresh(nb.clone(), ovn_nb_ca.clone()));
    }
    tokio::spawn(ovn_tls::watch_servers(
        nb.remotes().to_vec(),
        ovn_nb_ca.cert_pem().to_owned(),
        cfg.ovn.southbound.clone(),
        ovn_ca.cert_pem().to_owned(),
    ));
    let state = AppState::new(Inner {
        db: db.clone(),
        ca,
        ovn_ca,
        ovn_sb_remote: cfg.southbound().unwrap_or_default(),
        agents: Agents::default(),
        net: net::Reconciler::new(db.clone(), nb),
        replica: replica.clone(),
        events: events::Hub::default(),
        limits: ratelimit::Limits::new(
            cfg.api
                .redis_url
                .as_deref()
                .map(ratelimit::Shared::new)
                .transpose()
                .context("api.redis_url")?,
        ),
        trusted_proxies: cfg.api.trusted_proxies.clone(),
        trusted_oidc_issuers: cfg.api.trusted_oidc_issuers.clone(),
        yubico: cfg
            .yubico
            .as_ref()
            .map(two_factor::yubico::Validator::new)
            .transpose()?,
    });
    tracing::info!(%replica, fingerprint = %state.ca.fingerprint(), "controller starting");

    // Other replicas forward commands for agents connected here, and this
    // one forwards to them, authenticated by replica certificates.
    let peer_address = cfg.peer_address(&hostname());
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
    mst_db::replicas::heartbeat(&db, &replica, &peer_address).await?;
    tokio::spawn({
        let (db, replica) = (db.clone(), replica.clone());
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if let Err(e) = mst_db::replicas::heartbeat(&db, &replica, &peer_address).await {
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
    // Host failure handling, backup schedules and the port binding watch
    // run on one replica at a time.
    tokio::spawn({
        let state = state.clone();
        let southbound = cfg.ovn.southbound.clone();
        ha::lead(db.clone(), move || {
            let (state, southbound) = (state.clone(), southbound.clone());
            async move {
                tokio::join!(
                    ha::monitor(state.clone()),
                    engine::backup_schedule(state.clone()),
                    port_watch::watch(state, southbound),
                );
            }
        })
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
        .serve_with_shutdown(cfg.agents.listen, shutdown());
    tracing::info!(addr = %cfg.agents.listen, "agent endpoint listening");

    let (api, _) = api::router();
    let mut app = Router::new().route("/healthz", get(healthz)).merge(api);
    if let Some(dir) = &cfg.web_dir {
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
    let listener = tokio::net::TcpListener::bind(cfg.api.listen).await?;
    tracing::info!(addr = %cfg.api.listen, "HTTP API listening");
    let http = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown());

    let metrics_server = async {
        let Some(addr) = cfg.metrics_listen else {
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
        if let Err(e) = mst_db::sessions::purge_expired(&state.db).await {
            tracing::warn!(error = %e, "purging sessions");
        }
        if let Err(e) = mst_db::oidc::purge_expired(&state.db).await {
            tracing::warn!(error = %e, "purging OIDC logins");
        }
        if let Err(e) = mst_db::two_factor::purge_expired_challenges(&state.db).await {
            tracing::warn!(error = %e, "purging two-factor challenges");
        }
        if let Err(e) = mst_db::replicas::purge_stale(&state.db).await {
            tracing::warn!(error = %e, "purging gone replicas");
        }
        tokio::time::sleep(Duration::from_secs(600)).await;
    }
}

async fn healthz(State(state): State<AppState>) -> StatusCode {
    match mst_db::sqlx::query("SELECT 1").execute(&state.db).await {
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
