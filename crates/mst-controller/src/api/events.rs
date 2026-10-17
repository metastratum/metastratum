//! Live updates as Server-Sent Events.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures::Stream;
use mst_core::tenant::TenantPath;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::authz::{Perm, Principal};
use crate::events::{Change, Event, Notice};
use crate::state::AppState;

pub fn routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(events))
}

/// How often a long-lived stream re-checks its credentials and roles.
const RECHECK: Duration = Duration::from_secs(60);

/// A stream of changes the caller can see, as `text/event-stream`. Each event
/// is named for the kind of thing that changed (`vm`, `task`, `host`, `image`,
/// `tenant`, `cluster`, `storage_pool`, `vpc`, `network`, `security_group`,
/// `external_ip`, `network_policy`, `external_network`) and carries `{"id", "op"}`; fetch it
/// through the API to see what changed. `resync` means changes may have been
/// missed and everything shown should be refetched; it is also the first
/// event, so a reconnecting client catches up.
#[utoipa::path(get, path = "/events", tag = "events",
    responses((status = 200, description = "Server-Sent Events", content_type = "text/event-stream"), (status = 401)))]
async fn events(
    State(state): State<AppState>,
    p: Principal,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let secret = super::session_secret(&headers).unwrap_or_default();
    let mut sub = state.events.subscribe();
    let (tx, rx) = mpsc::channel::<SseEvent>(64);
    tokio::spawn(async move {
        let mut p = p;
        let mut root = root_path(&state, &p).await;
        let mut recheck = tokio::time::interval(RECHECK);
        recheck.tick().await;
        if tx.send(resync()).await.is_err() {
            return;
        }
        loop {
            let event = tokio::select! {
                _ = tx.closed() => return,
                _ = recheck.tick() => {
                    // Signed out, disabled, or roles changed since connecting.
                    match super::principal_for_secret(&state, &secret).await {
                        Ok(fresh) => {
                            p = fresh;
                            root = root_path(&state, &p).await;
                            continue;
                        }
                        Err(_) => return,
                    }
                }
                e = sub.recv() => e,
            };
            let out = match event {
                Ok(Event::Change(c)) => {
                    if !visible(&p, root.as_ref(), &c) {
                        continue;
                    }
                    match SseEvent::default()
                        .event(&c.kind)
                        .json_data(Notice { id: c.id, op: c.op })
                    {
                        Ok(e) => e,
                        Err(_) => continue,
                    }
                }
                Ok(Event::Resync) | Err(broadcast::error::RecvError::Lagged(_)) => resync(),
                Err(broadcast::error::RecvError::Closed) => return,
            };
            if tx.send(out).await.is_err() {
                return;
            }
        }
    });
    Sse::new(ReceiverStream::new(rx).map(Ok)).keep_alive(KeepAlive::default())
}

fn resync() -> SseEvent {
    SseEvent::default().event("resync").data("{}")
}

async fn root_path(state: &AppState, p: &Principal) -> Option<TenantPath> {
    if !p.is_provider_admin() && p.scopes(Perm::InfraRead).is_empty() {
        return None;
    }
    mst_db::tenants::root(&state.db)
        .await
        .ok()
        .flatten()
        .map(|t| t.path)
}

/// Whether the principal may learn that this changed.
fn visible(p: &Principal, root: Option<&TenantPath>, c: &Change) -> bool {
    match (c.kind.as_str(), &c.tenant) {
        // VM state shown to tenants depends on their host being reachable;
        // host IDs are already part of what they see of their VMs.
        ("host", _) => true,
        ("cluster" | "storage_pool" | "external_network" | "backup_repository", _) => {
            root.is_some_and(|r| p.can(Perm::InfraRead, r))
        }
        ("vpc" | "network" | "security_group" | "external_ip", Some(t)) => {
            p.can(Perm::NetworkRead, t)
        }
        // Guardrails apply to (and are shown in) the subtree below them.
        ("network_policy", Some(t)) => {
            p.can(Perm::NetworkRead, t)
                || p.bindings.iter().any(|b| {
                    t.is_ancestor_or_self_of(&b.tenant) && b.role.allows(Perm::NetworkRead)
                })
        }
        ("vm" | "backup" | "snapshot", Some(t)) => p.can(Perm::VmRead, t),
        ("task", Some(t)) => p.can(Perm::TaskRead, t),
        ("tenant", Some(t)) => p.can(Perm::TenantRead, t),
        // Images may be shared with sub-tenants.
        ("image", Some(t)) => {
            p.can(Perm::ImageRead, t)
                || p.bindings
                    .iter()
                    .any(|b| t.is_ancestor_or_self_of(&b.tenant) && b.role.allows(Perm::ImageRead))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use mst_authz::Binding;
    use mst_core::ids::{TenantId, UserId};
    use uuid::Uuid;

    use super::*;
    use crate::authz::Role;

    fn change(kind: &str, tenant: Option<&TenantPath>) -> Change {
        Change {
            kind: kind.into(),
            id: Uuid::nil(),
            op: "update".into(),
            tenant: tenant.cloned(),
        }
    }

    #[test]
    fn changes_reach_only_those_who_may_see_them() {
        let root = TenantPath::root(TenantId::new());
        let acme = root.child(TenantId::new());
        let dev = acme.child(TenantId::new());
        let other = root.child(TenantId::new());
        let p = Principal {
            user_id: UserId::new(),
            username: "op".into(),
            tenant_id: acme.tenant(),
            tenant_path: acme.clone(),
            bindings: vec![Binding {
                tenant: acme.clone(),
                role: Role::Operator,
            }],
            limited: false,
        };
        assert!(visible(&p, None, &change("vm", Some(&dev))));
        assert!(!visible(&p, None, &change("vm", Some(&other))));
        assert!(!visible(&p, None, &change("task", Some(&root))));
        // Shared images come from above.
        assert!(visible(&p, None, &change("image", Some(&root))));
        assert!(!visible(&p, None, &change("image", Some(&other))));
        assert!(visible(&p, None, &change("host", None)));
        assert!(!visible(&p, Some(&root), &change("cluster", None)));
        assert!(!visible(&p, None, &change("vm", None)));
        assert!(visible(&p, None, &change("backup", Some(&dev))));
        assert!(!visible(&p, None, &change("backup", Some(&other))));
    }
}
