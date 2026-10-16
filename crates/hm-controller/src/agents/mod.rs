//! Connected agents and request/response over their sessions.
//!
//! Each agent holds one control session with one controller replica. The
//! registry maps hosts to their session on this replica. Commands carry a
//! request ID; the agent answers with a `CommandResult` that completes the
//! waiting caller.
//!
//! TODO(M5): route commands for hosts connected to another replica.

mod service;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hm_core::ids::{HostId, VmId};
use hm_proto::{
    Command, ConsoleKind, ControllerMessage, OpenConsole, TunnelChunk, command, controller_message,
};
use tokio::sync::{mpsc, oneshot};
use tonic::Status;

pub use service::{AgentControlService, EnrollmentService};

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("host is not connected to this controller")]
    NotConnected,
    #[error("agent did not answer within {0:?}")]
    Timeout(Duration),
    #[error("agent session closed")]
    Closed,
    #[error("{0}")]
    Failed(String),
}

type Outbound = mpsc::Sender<Result<ControllerMessage, Status>>;

pub struct AgentConn {
    session: u64,
    tx: Outbound,
    pending: Mutex<HashMap<String, oneshot::Sender<Result<(), String>>>>,
}

impl AgentConn {
    /// Completes a pending command with the agent's result.
    fn complete(&self, request_id: &str, error: String) {
        if let Some(waiter) = self.pending.lock().unwrap().remove(request_id) {
            let _ = waiter.send(if error.is_empty() { Ok(()) } else { Err(error) });
        }
    }
}

/// An established console tunnel: bytes from the agent, and a sender for
/// bytes to it.
pub struct Tunnel {
    pub from_agent: tonic::Streaming<TunnelChunk>,
    pub to_agent: mpsc::Sender<Result<TunnelChunk, Status>>,
}

struct PendingTunnel {
    host: HostId,
    ready: oneshot::Sender<Tunnel>,
}

#[derive(Clone, Default)]
pub struct Agents {
    conns: Arc<Mutex<HashMap<HostId, Arc<AgentConn>>>>,
    tunnels: Arc<Mutex<HashMap<String, PendingTunnel>>>,
    next_session: Arc<AtomicU64>,
}

impl Agents {
    /// Registers a new session for `host`, replacing any older one.
    fn register(&self, host: HostId, tx: Outbound) -> Arc<AgentConn> {
        let conn = Arc::new(AgentConn {
            session: self.next_session.fetch_add(1, Ordering::Relaxed),
            tx,
            pending: Mutex::default(),
        });
        self.conns.lock().unwrap().insert(host, conn.clone());
        conn
    }

    /// Removes the session unless a newer one has already replaced it.
    /// Returns whether it was removed.
    fn unregister(&self, host: HostId, session: u64) -> bool {
        let mut conns = self.conns.lock().unwrap();
        if conns.get(&host).is_some_and(|c| c.session == session) {
            conns.remove(&host);
            true
        } else {
            false
        }
    }

    /// Sends a command and waits for the agent to report completion.
    pub async fn call(
        &self,
        host: HostId,
        cmd: command::Cmd,
        timeout: Duration,
    ) -> Result<(), AgentError> {
        let conn = self
            .conns
            .lock()
            .unwrap()
            .get(&host)
            .cloned()
            .ok_or(AgentError::NotConnected)?;
        let request_id = uuid::Uuid::now_v7().to_string();
        let (done, wait) = oneshot::channel();
        conn.pending
            .lock()
            .unwrap()
            .insert(request_id.clone(), done);

        let msg = ControllerMessage {
            msg: Some(controller_message::Msg::Command(Command {
                request_id: request_id.clone(),
                cmd: Some(cmd),
            })),
        };
        if conn.tx.send(Ok(msg)).await.is_err() {
            conn.pending.lock().unwrap().remove(&request_id);
            return Err(AgentError::Closed);
        }
        match tokio::time::timeout(timeout, wait).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(e))) => Err(AgentError::Failed(e)),
            Ok(Err(_)) => Err(AgentError::Closed),
            Err(_) => {
                conn.pending.lock().unwrap().remove(&request_id);
                Err(AgentError::Timeout(timeout))
            }
        }
    }

    /// Asks the agent on `host` to open a console tunnel for `vm` and waits
    /// for it to dial back.
    pub async fn open_console(
        &self,
        host: HostId,
        vm: VmId,
        kind: ConsoleKind,
    ) -> Result<Tunnel, AgentError> {
        let session_id = uuid::Uuid::now_v7().to_string();
        let (ready, wait) = oneshot::channel();
        self.tunnels
            .lock()
            .unwrap()
            .insert(session_id.clone(), PendingTunnel { host, ready });

        let open = command::Cmd::OpenConsole(OpenConsole {
            session_id: session_id.clone(),
            vm_id: vm.to_string(),
            kind: kind as i32,
        });
        let timeout = Duration::from_secs(15);
        let result = async {
            self.call(host, open, timeout).await?;
            tokio::time::timeout(timeout, wait)
                .await
                .map_err(|_| AgentError::Timeout(timeout))?
                .map_err(|_| AgentError::Closed)
        }
        .await;
        self.tunnels.lock().unwrap().remove(&session_id);
        result
    }

    /// Hands an incoming tunnel to the console session waiting for it.
    /// Rejects tunnels nobody asked for, or that come from the wrong host.
    fn accept_tunnel(&self, host: HostId, session_id: &str, tunnel: Tunnel) -> bool {
        let mut tunnels = self.tunnels.lock().unwrap();
        match tunnels.get(session_id) {
            Some(p) if p.host == host => {
                let p = tunnels.remove(session_id).expect("checked above");
                p.ready.send(tunnel).is_ok()
            }
            _ => false,
        }
    }
}
