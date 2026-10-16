//! Postgres persistence for the hypermon controller.
//!
//! Repositories are plain functions over any `PgExecutor`, so callers can
//! compose them inside a single transaction.

pub mod audit;
pub mod clusters;
pub mod console;
pub mod grants;
pub mod hosts;
pub mod images;
pub mod join_tokens;
pub mod networks;
pub mod pki;
pub mod quotas;
pub mod rbac;
pub mod sessions;
pub mod tasks;
pub mod tenants;
pub mod users;
pub mod vms;

use sqlx::postgres::{PgPool, PgPoolOptions};
use thiserror::Error;

pub use sqlx;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Domain(#[from] hm_core::Error),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub async fn connect(url: &str, max_connections: u32) -> Result<PgPool> {
    Ok(PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(url)
        .await?)
}

pub async fn migrate(pool: &PgPool) -> Result<()> {
    MIGRATOR.run(pool).await?;
    Ok(())
}

/// Maps unique-violation errors to a domain conflict, leaving others intact.
pub(crate) fn conflict_on_unique(err: sqlx::Error, what: &str) -> Error {
    match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            hm_core::Error::Conflict(format!("{what} already exists")).into()
        }
        _ => err.into(),
    }
}
