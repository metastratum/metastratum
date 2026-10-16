use std::ops::Deref;
use std::sync::Arc;

use hm_db::sqlx::PgPool;
use hm_ovsdb::Northbound;
use hm_pki::Ca;

use crate::agents::Agents;
use crate::events::Hub;

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

pub struct Inner {
    pub db: PgPool,
    pub ca: Ca,
    pub agents: Agents,
    pub nb: Northbound,
    /// Identifies this controller replica (agent session ownership, task
    /// leases).
    pub replica: String,
    /// Mark session cookies `Secure` (set when served over HTTPS).
    pub secure_cookies: bool,
    /// Base URL users reach the controller at, for OIDC redirect URIs.
    pub public_url: String,
    /// Change notifications for live updates.
    pub events: Hub,
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
