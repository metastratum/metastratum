//! Revoking host certificates.
//!
//! The controllers check a host's certificate against the database at every
//! connection, so they refuse a revoked one at once. Nothing else trusts
//! the controller's CA for hosts: migrations and copies between hosts each
//! bring a CA of their own.
//!
//! Revoked: a host's previous certificate once it has connected with the
//! renewed one, and all of a decommissioned host's certificates.

use mst_core::ids::HostId;
use mst_db::hosts::Revocable;

use crate::state::AppState;

/// Revokes certificates.
pub async fn revoke(state: &AppState, host: HostId, certs: &[Revocable], reason: &str) {
    if certs.is_empty() {
        return;
    }
    for (serial, expires) in certs {
        if let Err(e) = mst_db::pki::revoke(&state.db, serial, Some(host), reason, *expires).await {
            tracing::error!(%host, %serial, error = %e, "revoking a certificate");
        }
    }
    tracing::info!(%host, count = certs.len(), %reason, "revoked host certificates");
}
