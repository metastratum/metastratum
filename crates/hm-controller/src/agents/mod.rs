//! Connected agents and request/response over their sessions.
//!
//! Each agent holds one control session with one controller replica. The
//! registry maps hosts to their session on this replica. Commands carry a
//! request ID; the agent answers with a `CommandResult` that completes the
//! waiting caller. Commands and consoles for hosts whose session is on
//! another replica are forwarded there (see [`peers`]).

pub mod peers;
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

pub use service::{
    AgentControlService, EnrollmentService, ReplicaControlService, libvirt_tls_policy,
};

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
    /// Commands the agent handles; empty for agents from before
    /// capabilities, which get everything they always got.
    capabilities: std::collections::HashSet<String>,
    pending: Mutex<HashMap<String, oneshot::Sender<Result<String, String>>>>,
    /// Ends the session from this side.
    close: tokio::sync::Notify,
}

impl AgentConn {
    /// Fails every command waiting for an answer (the session ended).
    fn fail_pending(&self) {
        self.pending.lock().unwrap().clear();
    }

    /// Completes a pending command with the agent's result.
    fn complete(&self, request_id: &str, error: String, payload: String) {
        if let Some(waiter) = self.pending.lock().unwrap().remove(request_id) {
            let _ = waiter.send(if error.is_empty() {
                Ok(payload)
            } else {
                Err(error)
            });
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
    peers: Arc<std::sync::OnceLock<peers::Peers>>,
}

impl Agents {
    /// Lets commands for hosts connected elsewhere be forwarded.
    pub fn set_peers(&self, p: peers::Peers) {
        let _ = self.peers.set(p);
    }

    /// Agent sessions held by this replica.
    pub fn session_count(&self) -> usize {
        self.conns.lock().unwrap().len()
    }

    fn is_local(&self, host: HostId) -> bool {
        self.conns.lock().unwrap().contains_key(&host)
    }

    /// Registers a new session for `host`, replacing any older one.
    fn register(&self, host: HostId, tx: Outbound, capabilities: &[String]) -> Arc<AgentConn> {
        let conn = Arc::new(AgentConn {
            session: self.next_session.fetch_add(1, Ordering::Relaxed),
            tx,
            capabilities: capabilities.iter().cloned().collect(),
            pending: Mutex::default(),
            close: tokio::sync::Notify::new(),
        });
        self.conns.lock().unwrap().insert(host, conn.clone());
        conn
    }

    /// Ends the host's session, if it is held here.
    pub fn close(&self, host: HostId) {
        if let Some(c) = self.conns.lock().unwrap().get(&host) {
            c.close.notify_one();
        }
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
        self.send(host, cmd, timeout).await.map(drop)
    }

    /// Runs a command whose answer is JSON.
    pub async fn query<T: serde::de::DeserializeOwned>(
        &self,
        host: HostId,
        cmd: command::Cmd,
        timeout: Duration,
    ) -> Result<T, AgentError> {
        let payload = self.send(host, cmd, timeout).await?;
        serde_json::from_str(&payload)
            .map_err(|e| AgentError::Failed(format!("unexpected answer: {e}")))
    }

    /// Sends a command, here or via the replica holding the host's session,
    /// and waits for its result's payload.
    async fn send(
        &self,
        host: HostId,
        cmd: command::Cmd,
        timeout: Duration,
    ) -> Result<String, AgentError> {
        let (name, started) = (hm_proto::capability(&cmd), std::time::Instant::now());
        let result = match self.peers.get() {
            Some(p) if !self.is_local(host) => p.forward(host, cmd, timeout).await,
            _ => self.send_local(host, cmd, timeout).await,
        };
        let outcome = match &result {
            Ok(_) => "ok",
            Err(AgentError::Failed(_)) => "failed",
            Err(AgentError::Timeout(_)) => "timeout",
            Err(AgentError::NotConnected | AgentError::Closed) => "unreachable",
        };
        crate::metrics::agent_command(name, outcome, started);
        result
    }

    /// Sends a command to an agent whose session is on this replica.
    pub async fn send_local(
        &self,
        host: HostId,
        cmd: command::Cmd,
        timeout: Duration,
    ) -> Result<String, AgentError> {
        let conn = self
            .conns
            .lock()
            .unwrap()
            .get(&host)
            .cloned()
            .ok_or(AgentError::NotConnected)?;
        let want = hm_proto::capability(&cmd);
        if !conn.capabilities.is_empty() && !conn.capabilities.contains(want) {
            return Err(AgentError::Failed(format!(
                "the host's agent does not support {want}; upgrade it"
            )));
        }
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
            Ok(Ok(Ok(payload))) => Ok(payload),
            Ok(Ok(Err(e))) => Err(AgentError::Failed(e)),
            Ok(Err(_)) => Err(AgentError::Closed),
            Err(_) => {
                conn.pending.lock().unwrap().remove(&request_id);
                Err(AgentError::Timeout(timeout))
            }
        }
    }

    /// Opens a console tunnel to `vm` on `host`, here or via the replica
    /// holding the host's session.
    pub async fn open_console(
        &self,
        host: HostId,
        vm: VmId,
        kind: ConsoleKind,
    ) -> Result<Tunnel, AgentError> {
        match self.peers.get() {
            Some(p) if !self.is_local(host) => p.console(host, vm, kind).await,
            _ => self.open_console_local(host, vm, kind).await,
        }
    }

    /// Asks the agent on `host` (connected here) to open a console tunnel for
    /// `vm` and waits for it to dial back.
    pub async fn open_console_local(
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
            self.send_local(host, open, timeout).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn commands_an_agent_lacks_are_refused_without_sending() {
        let agents = Agents::default();
        let host = HostId::new();
        let (tx, mut rx) = mpsc::channel(4);
        agents.register(host, tx, &["start_vm".to_owned()]);
        let cmd = command::Cmd::MigrateVm(hm_proto::MigrateVm::default());
        let err = agents
            .send_local(host, cmd, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("upgrade"), "{err}");
        assert!(rx.try_recv().is_err(), "nothing was sent");
        // An agent from before capabilities gets everything.
        let old = HostId::new();
        let (tx, mut rx) = mpsc::channel(4);
        agents.register(old, tx, &[]);
        let cmd = command::Cmd::MigrateVm(hm_proto::MigrateVm::default());
        let _ = agents.send_local(old, cmd, Duration::from_millis(50)).await;
        assert!(rx.try_recv().is_ok(), "sent");
    }
}
