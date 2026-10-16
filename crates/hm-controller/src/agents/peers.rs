//! Reaching agents connected to other controller replicas.
//!
//! Each agent holds one session, with whichever replica it reached. A replica
//! that needs a host whose session is elsewhere (an API call or task landed on
//! it) forwards the command, or the console tunnel, to the replica holding
//! the session, over the agent endpoint with its replica certificate.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use hm_core::ids::{HostId, VmId};
use hm_db::sqlx::PgPool;
use hm_proto::replica_control_client::ReplicaControlClient;
use hm_proto::{Command, ConsoleKind, ForwardedCommand, TunnelChunk, command};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use super::{AgentError, Tunnel};

/// Common names of replica certificates start with this.
pub const REPLICA_CN_PREFIX: &str = "replica:";

pub struct Peers {
    db: PgPool,
    me: String,
    tls: ClientTlsConfig,
    channels: Mutex<HashMap<String, Channel>>,
}

impl Peers {
    pub fn new(db: PgPool, me: String, tls: ClientTlsConfig) -> Self {
        Self {
            db,
            me,
            tls,
            channels: Mutex::default(),
        }
    }

    async fn client(&self, host: HostId) -> Result<ReplicaControlClient<Channel>, AgentError> {
        let (id, addr) = hm_db::replicas::holding(&self.db, host)
            .await
            .map_err(|e| AgentError::Failed(format!("finding the host's controller: {e}")))?
            .ok_or(AgentError::NotConnected)?;
        if id == self.me {
            // Recorded as ours, but the session is gone.
            return Err(AgentError::NotConnected);
        }
        let channel = {
            let mut channels = self.channels.lock().unwrap();
            match channels.get(&addr) {
                Some(c) => c.clone(),
                None => {
                    let c = Endpoint::from_shared(format!("https://{addr}"))
                        .map_err(|e| AgentError::Failed(e.to_string()))?
                        .tls_config(self.tls.clone())
                        .map_err(|e| AgentError::Failed(e.to_string()))?
                        .connect_timeout(Duration::from_secs(5))
                        .http2_keep_alive_interval(Duration::from_secs(15))
                        .connect_lazy();
                    channels.insert(addr.clone(), c.clone());
                    c
                }
            }
        };
        Ok(ReplicaControlClient::new(channel))
    }

    pub async fn forward(
        &self,
        host: HostId,
        cmd: command::Cmd,
        timeout: Duration,
    ) -> Result<String, AgentError> {
        let mut client = self.client(host).await?;
        let mut req = tonic::Request::new(ForwardedCommand {
            host_id: host.to_string(),
            command: Some(Command {
                request_id: String::new(),
                cmd: Some(cmd),
            }),
            timeout_ms: timeout.as_millis() as u64,
        });
        // The other replica applies `timeout`; allow for the hop.
        req.set_timeout(timeout + Duration::from_secs(5));
        match client.call(req).await {
            Ok(r) => {
                let r = r.into_inner();
                if r.error.is_empty() {
                    Ok(r.payload)
                } else {
                    Err(AgentError::Failed(r.error))
                }
            }
            Err(s) => Err(from_status(s, timeout)),
        }
    }

    pub async fn console(
        &self,
        host: HostId,
        vm: VmId,
        kind: ConsoleKind,
    ) -> Result<Tunnel, AgentError> {
        let mut client = self.client(host).await?;
        let (to_agent, rx) = mpsc::channel(64);
        let outbound =
            ReceiverStream::new(rx).filter_map(|c: Result<TunnelChunk, tonic::Status>| c.ok());
        let mut req = tonic::Request::new(outbound);
        let md = req.metadata_mut();
        let kind = match kind {
            ConsoleKind::Serial => "serial",
            _ => "vnc",
        };
        for (k, v) in [
            ("host-id", host.to_string()),
            ("vm-id", vm.to_string()),
            ("kind", kind.to_owned()),
        ] {
            md.insert(
                k,
                v.parse()
                    .map_err(|_| AgentError::Failed("bad metadata".into()))?,
            );
        }
        let from_agent = client
            .console(req)
            .await
            .map_err(|s| from_status(s, Duration::from_secs(15)))?
            .into_inner();
        Ok(Tunnel {
            from_agent,
            to_agent,
        })
    }
}

/// How the other replica's answers map back. It says `unavailable` when the
/// agent has no session there (any more).
pub fn from_status(s: tonic::Status, timeout: Duration) -> AgentError {
    match s.code() {
        tonic::Code::Unavailable if s.message() == NOT_CONNECTED => AgentError::NotConnected,
        tonic::Code::DeadlineExceeded => AgentError::Timeout(timeout),
        _ => AgentError::Failed(format!("via another controller: {}", s.message())),
    }
}

pub const NOT_CONNECTED: &str = "host not connected here";
