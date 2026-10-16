//! hypermon control plane.

use std::net::SocketAddr;

use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use clap::Parser;
use hm_db::sqlx::PgPool;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Postgres connection URL.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,
    /// Address for the HTTP API.
    #[arg(long, env = "HM_LISTEN", default_value = "127.0.0.1:8080")]
    listen: SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();

    let pool = hm_db::connect(&args.database_url, 16)
        .await
        .context("connecting to postgres")?;
    hm_db::migrate(&pool).await.context("running migrations")?;

    let app = Router::new()
        .route("/healthz", get(healthz))
        .layer(TraceLayer::new_for_http())
        .with_state(pool);

    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    tracing::info!(addr = %args.listen, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn healthz(State(pool): State<PgPool>) -> StatusCode {
    match hm_db::sqlx::query("SELECT 1").execute(&pool).await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
