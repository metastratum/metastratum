//! Secrets the database holds, sealed with the master key (see
//! `mst_pki::sealing`): the CA's private key, storage pools' keys and
//! identity providers' client secrets. The key itself lives outside the
//! database, in a file only the controller reads.

use std::sync::OnceLock;

use anyhow::{Context, Result};
use mst_db::sqlx::PgPool;
use mst_pki::{Sealer, is_sealed};

static SEALER: OnceLock<Sealer> = OnceLock::new();

pub const CA_KEY: &str = "ca.key";
pub const OVN_CA_KEY: &str = "ovn-ca.key";
pub const OVN_NB_CA_KEY: &str = "ovn-nb-ca.key";
pub const POOL_KEY: &str = "storage_pool.key";

pub fn idp_secret(tenant: mst_core::ids::TenantId) -> String {
    format!("identity_provider.client_secret:{tenant}")
}

/// Loads the master key. Called once at startup.
pub fn init(key_file: &std::path::Path) -> Result<()> {
    let key = std::fs::read_to_string(key_file)
        .with_context(|| format!("reading the master key from {}", key_file.display()))?;
    let sealer = Sealer::from_base64(&key).context("loading the master key")?;
    let _ = SEALER.set(sealer);
    Ok(())
}

fn sealer() -> &'static Sealer {
    SEALER.get().expect("secrets::init runs at startup")
}

pub fn seal(context: &str, plaintext: &str) -> String {
    sealer().seal(context, plaintext)
}

pub fn open(context: &str, value: &str) -> Result<String> {
    sealer()
        .open(context, value)
        .with_context(|| format!("opening {context}"))
}

/// Seals secrets stored before sealing existed (or while it was off).
pub async fn seal_existing(db: &PgPool) -> Result<()> {
    let mut n = 0;
    if let Some((cert, key)) = mst_db::pki::get(db, crate::ca::CA_NAME).await?
        && !is_sealed(&key)
    {
        mst_db::pki::set_key(db, crate::ca::CA_NAME, &seal(CA_KEY, &key)).await?;
        let _ = cert;
        n += 1;
    }
    for pool in mst_db::storage_pools::list(db).await? {
        let Some(key) = pool.config.get("key").and_then(|k| k.as_str()) else {
            continue;
        };
        if !is_sealed(key) {
            let mut config = pool.config.clone();
            config["key"] = seal(POOL_KEY, key).into();
            mst_db::storage_pools::set_config(db, pool.id, &config).await?;
            n += 1;
        }
    }
    for (tenant, secret) in mst_db::oidc::client_secrets(db).await? {
        if !is_sealed(&secret) {
            mst_db::oidc::set_client_secret(db, tenant, &seal(&idp_secret(tenant), &secret))
                .await?;
            n += 1;
        }
    }
    if n > 0 {
        tracing::info!(count = n, "sealed secrets stored in plaintext");
    }
    Ok(())
}
