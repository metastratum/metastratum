use std::ops::Deref;
use std::sync::Arc;

use mst_db::sqlx::PgPool;
use mst_pki::Ca;

use crate::agents::Agents;
use crate::events::Hub;
use crate::net::Reconciler;

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

pub struct Inner {
    pub db: PgPool,
    pub ca: Ca,
    /// Signs hosts' certificates for the OVN southbound database.
    pub ovn_ca: Ca,
    /// Southbound remote(s) agents point ovn-controller at, over TLS; empty
    /// when southbound access is left to the deployment.
    pub ovn_sb_remote: String,
    pub agents: Agents,
    /// Keeps OVN in line with the network model.
    pub net: Reconciler,
    /// Identifies this controller replica (agent session ownership, task
    /// leases).
    pub replica: String,
    /// Base URL users reach the controller at, for OIDC redirect URIs.
    pub public_url: String,
    /// Change notifications for live updates.
    pub events: Hub,
    pub limits: crate::ratelimit::Limits,
    /// Proxies whose `X-Forwarded-For` names the client, and whose
    /// `X-Forwarded-Proto` says whether it came over HTTPS.
    pub trusted_proxies: Vec<crate::ratelimit::Cidr>,
    /// Identity provider issuers sub-tenants may use on internal addresses.
    pub trusted_oidc_issuers: Vec<String>,
}

impl AppState {
    pub fn new(inner: Inner) -> Self {
        Self(Arc::new(inner))
    }
}

impl Deref for AppState {
    type Target = Inner;

    fn deref(&self) -> &Inner {
        &self.0
    }
}
