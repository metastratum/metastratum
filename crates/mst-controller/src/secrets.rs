//! Secrets the database holds, sealed with the master key (see
//! `mst_pki::sealing`): the CA's private key, storage pools' keys and
//! identity providers' client secrets. The key itself lives outside the
//! database, in a file only the controller reads.

use std::sync::OnceLock;

use anyhow::{Context, Result};
use mst_pki::Sealer;

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
