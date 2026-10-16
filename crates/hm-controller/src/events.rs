//! Live change notifications.
//!
//! Database triggers announce changes on the `hm_events` channel (see the
//! `events` migration). Each replica listens and fans them out to its
//! browsers, which refetch what changed. Notifications say what changed and
//! which tenant owns it, never the contents, so nothing bypasses the API's
//! authorization.

use std::collections::HashMap;
use std::time::Duration;

use hm_core::ids::HostId;
use hm_core::tenant::TenantPath;
use hm_db::sqlx::PgPool;
use hm_db::sqlx::postgres::PgListener;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

const CHANNEL: &str = "hm_events";

/// Something changed.
#[derive(Clone, Debug, Deserialize)]
pub struct Change {
    /// `vm`, `task`, `host`, `image`, `tenant`, `cluster` or `storage_pool`.
    pub kind: String,
    pub id: Uuid,
    /// `insert`, `update` or `delete`.
    pub op: String,
    /// The owning tenant's path; none for provider infrastructure.
    #[serde(default, deserialize_with = "tenant_path")]
    pub tenant: Option<TenantPath>,
}

fn tenant_path<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<TenantPath>, D::Error> {
    Ok(Option::<String>::deserialize(d)?.and_then(|s| TenantPath::parse(&s)))
}

/// What a browser is sent.
#[derive(Clone, Debug, Serialize)]
pub struct Notice {
    pub id: Uuid,
    pub op: String,
}

#[derive(Clone, Debug)]
pub enum Event {
    Change(Change),
    /// Changes may have been missed; everything should be refetched.
    Resync,
}

#[derive(Clone)]
pub struct Hub {
    tx: broadcast::Sender<Event>,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            tx: broadcast::channel(1024).0,
        }
    }
}

impl Hub {
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    fn send(&self, e: Event) {
        // No subscribers is fine.
        let _ = self.tx.send(e);
    }

    /// Relays database notifications forever, reconnecting as needed.
    pub async fn listen(self, db: PgPool) {
        loop {
            if let Err(e) = self.relay(&db).await {
                tracing::warn!(error = %e, "change notifications interrupted");
            }
            // Whatever happened meanwhile went unannounced.
            self.send(Event::Resync);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn relay(&self, db: &PgPool) -> hm_db::sqlx::Result<()> {
        let mut listener = PgListener::connect_with(db).await?;
        listener.listen(CHANNEL).await?;
        tracing::info!("listening for change notifications");
        // A reconnect may follow a gap, so tell subscribers to catch up.
        self.send(Event::Resync);
        loop {
            match listener.try_recv().await? {
                Some(n) => match serde_json::from_str::<Change>(n.payload()) {
                    Ok(c) => self.send(Event::Change(c)),
                    Err(e) => {
                        tracing::warn!(error = %e, payload = n.payload(), "bad change notification")
                    }
                },
                // The connection dropped; the listener reconnects on the next
                // call, but notifications in between are lost.
                None => self.send(Event::Resync),
            }
        }
    }

    /// Announces hosts going quiet or coming back. Liveness depends on time
    /// since the last heartbeat, which no row change marks, so each replica
    /// watches for it itself.
    pub async fn watch_hosts(self, db: PgPool) {
        let mut live: HashMap<HostId, bool> = HashMap::new();
        loop {
            match hm_db::hosts::list(&db).await {
                Ok(hosts) => {
                    for h in hosts {
                        let now = h.is_live();
                        if live.insert(h.id, now).is_some_and(|before| before != now) {
                            self.send(Event::Change(Change {
                                kind: "host".into(),
                                id: h.id.0,
                                op: "update".into(),
                                tenant: None,
                            }));
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "checking host liveness"),
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }
}
