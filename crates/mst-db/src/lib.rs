//! Postgres persistence for the metastratum controller.
//!
//! Repositories are plain functions over any `PgExecutor`, so callers can
//! compose them inside a single transaction.

pub mod audit;
pub mod backups;
pub mod clusters;
pub mod console;
pub mod disks;
pub mod external;
pub mod grants;
pub mod hosts;
pub mod images;
pub mod join_tokens;
pub mod load_balancers;
pub mod migrations;
pub mod netmodel;
pub mod netsec;
pub mod networks;
pub mod oidc;
pub mod pki;
pub mod quotas;
pub mod rbac;
pub mod replicas;
pub mod sessions;
pub mod snapshots;
pub mod stale;
pub mod stats;
pub mod storage_pools;
pub mod tasks;
pub mod tenants;
pub mod two_factor;
pub mod users;
pub mod vms;

use sqlx::postgres::{PgPool, PgPoolOptions};
use thiserror::Error;

pub use sqlx;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Domain(#[from] mst_core::Error),
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error(
        "this database was made by metastratum 0.4 or older, which 0.5 does not upgrade: \
         install 0.5 on a new database"
    )]
    TooOld,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub async fn connect(url: &str, max_connections: u32) -> Result<PgPool> {
    Ok(PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(url)
        .await?)
}

/// The first migration of 0.5's schema.
const FIRST_MIGRATION: i64 = 20261019000001;

/// Brings the schema up to date. Migrations this build does not know were
/// applied by a newer controller during a rolling upgrade; they are left
/// alone, so an older replica can still restart until it is replaced.
pub async fn migrate(pool: &PgPool) -> Result<()> {
    // 0.5 starts the schema afresh (its first migration is all of it);
    // migrations older than that are 0.4's or earlier. A new database has
    // no migrations table yet, and a query naming it would not even parse.
    let migrated: bool = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
        .fetch_one(pool)
        .await?;
    if migrated {
        let made_before: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM _sqlx_migrations WHERE version < $1)")
                .bind(FIRST_MIGRATION)
                .fetch_one(pool)
                .await?;
        if made_before {
            return Err(Error::TooOld);
        }
    }
    let mut migrator = sqlx::migrate!("../../migrations");
    migrator.set_ignore_missing(true);
    migrator.run(pool).await?;
    Ok(())
}

/// Maps unique-violation errors to a domain conflict, leaving others intact.
pub(crate) fn conflict_on_unique(err: sqlx::Error, what: &str) -> Error {
    match &err {
        sqlx::Error::Database(db) if db.is_unique_violation() => {
            mst_core::Error::Conflict(format!("{what} already exists")).into()
        }
        _ => err.into(),
    }
}
