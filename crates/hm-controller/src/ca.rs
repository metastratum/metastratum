//! The controller's CA lives in the database so every replica shares it.

use std::time::Duration;

use anyhow::Context;
use hm_db::sqlx::PgPool;
use hm_pki::Ca;

pub const CA_NAME: &str = "ca";
/// A CA of its own for OVN southbound access: the southbound servers trust
/// it alone, so no other certificate hypermon issues (replicas', the agent
/// endpoint's, hosts' own) opens the southbound database.
pub const OVN_CA_NAME: &str = "ovn-ca";
const CA_LIFETIME: Duration = Duration::from_secs(10 * 365 * 86400);
const SERVER_LIFETIME: Duration = Duration::from_secs(365 * 86400);

/// The name agents use for TLS server-name verification. Trust is pinned to
/// the private CA, so a fixed name lets agents reach the controller by any
/// address.
pub const SERVER_NAME: &str = "hm-controller";

pub async fn load_or_create(db: &PgPool) -> anyhow::Result<Ca> {
    load_or_create_named(db, CA_NAME, "hypermon CA", crate::secrets::CA_KEY).await
}

pub async fn load_or_create_ovn(db: &PgPool) -> anyhow::Result<Ca> {
    load_or_create_named(
        db,
        OVN_CA_NAME,
        "hypermon OVN CA",
        crate::secrets::OVN_CA_KEY,
    )
    .await
}

async fn load_or_create_named(
    db: &PgPool,
    name: &str,
    common_name: &str,
    context: &str,
) -> anyhow::Result<Ca> {
    if hm_db::pki::get(db, name).await?.is_none() {
        let ca = Ca::generate(common_name, CA_LIFETIME)?;
        let key = crate::secrets::seal(context, &ca.key_pem());
        if hm_db::pki::insert_if_absent(db, name, ca.cert_pem(), &key).await? {
            tracing::info!(%name, fingerprint = %ca.fingerprint(), "generated new CA");
        }
    }
    let (cert, key) = hm_db::pki::get(db, name)
        .await?
        .with_context(|| format!("{name} vanished"))?;
    let key = crate::secrets::open(context, &key)?;
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
