//! The controller's CA lives in the database so every replica shares it.

use std::time::Duration;

use anyhow::Context;
use mst_db::sqlx::PgPool;
use mst_pki::Ca;

pub const CA_NAME: &str = "ca";
/// A CA of its own for OVN southbound access: the southbound servers trust
/// it alone, so no other certificate metastratum issues (replicas', the agent
/// endpoint's, hosts' own) opens the southbound database.
pub const OVN_CA_NAME: &str = "ovn-ca";
/// A CA of its own for the OVN northbound database: its servers trust it
/// alone, and only controllers get certificates from it. The northbound
/// database has no access control, so hosts' southbound certificates must
/// not open it.
pub const OVN_NB_CA_NAME: &str = "ovn-nb-ca";
const CA_LIFETIME: Duration = Duration::from_secs(10 * 365 * 86400);
const SERVER_LIFETIME: Duration = Duration::from_secs(365 * 86400);

/// The name agents use for TLS server-name verification. Trust is pinned to
/// the private CA, so a fixed name lets agents reach the controller by any
/// address.
pub const SERVER_NAME: &str = "mst-controller";

pub async fn load_or_create(db: &PgPool) -> anyhow::Result<Ca> {
    load_or_create_named(db, CA_NAME, "metastratum CA", crate::secrets::CA_KEY).await
}

pub async fn load_or_create_ovn(db: &PgPool) -> anyhow::Result<Ca> {
    load_or_create_named(
        db,
        OVN_CA_NAME,
        "metastratum OVN CA",
        crate::secrets::OVN_CA_KEY,
    )
    .await
}

pub async fn load_or_create_ovn_nb(db: &PgPool) -> anyhow::Result<Ca> {
    load_or_create_named(
        db,
        OVN_NB_CA_NAME,
        "metastratum OVN northbound CA",
        crate::secrets::OVN_NB_CA_KEY,
    )
    .await
}

async fn load_or_create_named(
    db: &PgPool,
    name: &str,
    common_name: &str,
    context: &str,
) -> anyhow::Result<Ca> {
    if mst_db::pki::get(db, name).await?.is_none() {
        let ca = Ca::generate(common_name, CA_LIFETIME)?;
        let key = crate::secrets::seal(context, &ca.key_pem());
        if mst_db::pki::insert_if_absent(db, name, ca.cert_pem(), &key).await? {
            tracing::info!(%name, fingerprint = %ca.fingerprint(), "generated new CA");
        }
    }
    let (cert, key) = mst_db::pki::get(db, name)
        .await?
        .with_context(|| format!("{name} vanished"))?;
    let key = crate::secrets::open(context, &key)?;
    Ok(Ca::from_pem(&cert, &key)?)
}

/// Issues this replica's agent-endpoint certificate, for the fixed name
/// agents and replicas check. Returns the PEM chain (leaf then CA, so agents
/// can find the CA to pin) and the key.
pub fn server_identity(ca: &Ca) -> anyhow::Result<(String, String)> {
    let (cert, key) = ca.issue_server(&[SERVER_NAME.to_owned()], SERVER_LIFETIME)?;
    Ok((format!("{cert}{}", ca.cert_pem()), key))
}
