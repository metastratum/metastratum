//! The controller's CA lives in the database so every replica shares it.

use std::time::Duration;

use anyhow::Context;
use hm_db::sqlx::PgPool;
use hm_pki::Ca;

const CA_NAME: &str = "ca";
const CA_LIFETIME: Duration = Duration::from_secs(10 * 365 * 86400);
const SERVER_LIFETIME: Duration = Duration::from_secs(365 * 86400);

/// The name agents use for TLS server-name verification. Trust is pinned to
/// the private CA, so a fixed name lets agents reach the controller by any
/// address.
pub const SERVER_NAME: &str = "hm-controller";

pub async fn load_or_create(db: &PgPool) -> anyhow::Result<Ca> {
    if hm_db::pki::get(db, CA_NAME).await?.is_none() {
        let ca = Ca::generate("hypermon CA", CA_LIFETIME)?;
        if hm_db::pki::insert_if_absent(db, CA_NAME, ca.cert_pem(), &ca.key_pem()).await? {
            tracing::info!(fingerprint = %ca.fingerprint(), "generated new CA");
        }
    }
    let (cert, key) = hm_db::pki::get(db, CA_NAME).await?.context("CA vanished")?;
    Ok(Ca::from_pem(&cert, &key)?)
}

/// Issues this replica's agent-endpoint certificate. Returns the PEM chain
/// (leaf then CA, so agents can find the CA to pin) and the key.
pub fn server_identity(ca: &Ca, extra_names: &[String]) -> anyhow::Result<(String, String)> {
    let mut sans = vec![SERVER_NAME.to_owned()];
    sans.extend(extra_names.iter().cloned());
    let (cert, key) = ca.issue_server(&sans, SERVER_LIFETIME)?;
    Ok((format!("{cert}{}", ca.cert_pem()), key))
}
