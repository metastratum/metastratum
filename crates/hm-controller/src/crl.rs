//! Revoking host certificates.
//!
//! The controllers check a host's certificate against the database at every
//! connection, so they refuse a revoked one at once. QEMU (migration
//! streams) and libvirt only know the CA, so hosts install the CA's
//! revocation list as well: agents get it in the controller's welcome, and
//! are sent a new one whenever a certificate is revoked.
//!
//! Revoked: a host's previous certificate once it has connected with the
//! renewed one, and all of a decommissioned host's certificates.

use std::time::Duration;

use hm_core::ids::HostId;
use hm_db::hosts::Revocable;

use crate::state::AppState;

/// Lists are re-signed at every connection and revocation; this only bounds
/// how long a host may go without one before QEMU and libvirt distrust it.
const VALID_FOR: Duration = Duration::from_secs(365 * 86400);

/// The CA's current revocation list (PEM).
pub async fn current(state: &AppState) -> anyhow::Result<String> {
    let revoked: Vec<(String, i64)> = hm_db::pki::revoked(&state.db)
        .await?
        .into_iter()
        .map(|(serial, at)| (serial, at.timestamp()))
        .collect();
    // Later lists carry larger numbers.
    let number = chrono::Utc::now().timestamp_millis() as u64;
    Ok(state.ca.crl(number, &revoked, VALID_FOR)?)
}

/// Revokes certificates and sends hosts the new list.
pub async fn revoke(state: &AppState, host: HostId, certs: &[Revocable], reason: &str) {
    if certs.is_empty() {
        return;
    }
    for (serial, expires) in certs {
        if let Err(e) = hm_db::pki::revoke(&state.db, serial, Some(host), reason, *expires).await {
            tracing::error!(%host, %serial, error = %e, "revoking a certificate");
        }
    }
    tracing::info!(%host, count = certs.len(), %reason, "revoked host certificates");
    tokio::spawn(push(state.clone()));
}

/// Sends the current list to every connected host.
pub async fn push(state: AppState) {
    let pem = match current(&state).await {
        Ok(p) => p,
        Err(e) => return tracing::error!(error = %format!("{e:#}"), "signing the revocation list"),
    };
    let Ok(hosts) = hm_db::hosts::list(&state.db).await else {
        return;
    };
    for h in hosts
        .iter()
        .filter(|h| h.state != "decommissioned" && h.is_live())
    {
        let cmd = hm_proto::command::Cmd::InstallCrl(hm_proto::InstallCrl {
            crl_pem: pem.clone(),
        });
        if let Err(e) = state.agents.call(h.id, cmd, Duration::from_secs(30)).await {
            // It gets the list again when it reconnects.
            tracing::warn!(host = %h.id, error = %e, "sending the revocation list");
        }
    }
}
