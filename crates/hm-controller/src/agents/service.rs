//! gRPC services agents connect to.

use std::collections::HashSet;
use std::pin::Pin;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use futures::Stream;
use hm_core::ids::{HostId, VmId};
use hm_proto::agent_control_server::AgentControl;
use hm_proto::enrollment_server::Enrollment;
use hm_proto::{
    AgentMessage, ControllerMessage, DomainState, EnrollRequest, EnrollResponse, HostFacts,
    PowerState, TunnelChunk, agent_message, tunnel_chunk,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use super::Tunnel;
use crate::state::AppState;

const HOST_CERT_LIFETIME: Duration = Duration::from_secs(365 * 86400);
/// Agents heartbeat every few seconds; a silent session is presumed dead.
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

fn internal(e: impl std::fmt::Display) -> Status {
    tracing::error!(error = %e, "agent service error");
    Status::internal("internal error")
}

fn facts_row(f: &HostFacts) -> hm_db::hosts::Facts<'_> {
    hm_db::hosts::Facts {
        name: &f.hostname,
        address: &f.address,
        cpu_count: f.cpu_count as i32,
        memory_mib: f.memory_mib as i64,
        cpu_model: &f.cpu_model,
        kvm: f.kvm,
        libvirt_version: &f.libvirt_version,
        ovn_chassis: &f.ovn_chassis,
        agent_version: &f.agent_version,
        physical_networks: &f.physical_networks,
        cpu_json: &f.cpu_json,
        migration_ready: f.migration_ready,
    }
}

pub fn power_name(p: i32) -> &'static str {
    match PowerState::try_from(p).unwrap_or(PowerState::Unspecified) {
        PowerState::Running => "running",
        PowerState::Stopped => "stopped",
        PowerState::Paused => "paused",
        PowerState::Crashed => "crashed",
        PowerState::ShuttingDown => "shutting_down",
        PowerState::Unspecified => "unknown",
    }
}

pub struct EnrollmentService(pub AppState);

#[tonic::async_trait]
impl Enrollment for EnrollmentService {
    async fn enroll(
        &self,
        req: Request<EnrollRequest>,
    ) -> Result<Response<EnrollResponse>, Status> {
        let state = &self.0;
        let req = req.into_inner();
        let facts = req
            .facts
            .ok_or_else(|| Status::invalid_argument("facts are required"))?;
        if facts.hostname.is_empty() || facts.address.parse::<std::net::IpAddr>().is_err() {
            return Err(Status::invalid_argument(
                "hostname and a valid address are required",
            ));
        }
        let token: hm_pki::JoinToken = req
            .token
            .parse()
            .map_err(|_| Status::unauthenticated("invalid join token"))?;
        // The agent pinned this fingerprint; a token for another CA means the
        // agent was pointed at the wrong control plane.
        if token.ca_fingerprint != state.ca.fingerprint() {
            return Err(Status::unauthenticated("invalid join token"));
        }

        let host_id = HostId::new();
        let cert_pem = state
            .ca
            .sign_host_csr(
                &req.csr_pem,
                &host_id.to_string(),
                &[facts.hostname.clone(), facts.address.clone()],
                HOST_CERT_LIFETIME,
            )
            .map_err(|e| Status::invalid_argument(format!("invalid CSR: {e}")))?;
        let der = hm_pki::pem_to_der(&cert_pem).map_err(internal)?;
        let not_after = hm_pki::not_after(&der).map_err(internal)?;

        let mut tx = state.db.begin().await.map_err(internal)?;
        let Some(cluster) = hm_db::join_tokens::consume(&mut *tx, &token.id, &token.secret_hash())
            .await
            .map_err(internal)?
        else {
            return Err(Status::unauthenticated("invalid join token"));
        };
        let host = hm_db::hosts::create(
            &mut *tx,
            host_id,
            &facts_row(&facts),
            &hm_pki::fingerprint(&der),
            Utc.timestamp_opt(not_after, 0)
                .single()
                .unwrap_or_else(Utc::now),
        )
        .await
        .map_err(|e| match e {
            hm_db::Error::Domain(hm_core::Error::Conflict(_)) => Status::already_exists(format!(
                "a host named {} is already enrolled",
                facts.hostname
            )),
            e => internal(e),
        })?;
        hm_db::hosts::set_cluster(&mut *tx, host.id, cluster)
            .await
            .map_err(internal)?;
        hm_db::join_tokens::attach_host(&mut *tx, &token.id, host.id)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;

        tracing::info!(host = %host.id, name = %host.name, address = %host.address, "host enrolled");
        Ok(Response::new(EnrollResponse {
            host_id: host.id.to_string(),
            cert_pem,
            ca_pem: state.ca.cert_pem().to_owned(),
        }))
    }
}

pub struct AgentControlService(pub AppState);

impl AgentControlService {
    /// Identifies the host from its verified client certificate. The
    /// certificate must be the one most recently issued to the host.
    async fn authenticate<T>(&self, req: &Request<T>) -> Result<HostId, Status> {
        let certs = req
            .peer_certs()
            .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        let leaf = certs
            .first()
            .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        let host_id: HostId = hm_pki::common_name(leaf)
            .ok()
            .and_then(|cn| cn.parse().ok())
            .ok_or_else(|| Status::unauthenticated("not a host certificate"))?;
        let host = match hm_db::hosts::get(&self.0.db, host_id).await {
            Ok(h) => h,
            Err(hm_db::Error::Domain(hm_core::Error::NotFound { .. })) => {
                return Err(Status::unauthenticated("unknown host"));
            }
            Err(e) => return Err(internal(e)),
        };
        if host.cert_fingerprint != hm_pki::fingerprint(leaf) {
            return Err(Status::unauthenticated("certificate has been superseded"));
        }
        if host.state == "decommissioned" {
            return Err(Status::permission_denied("host is decommissioned"));
        }
        Ok(host_id)
    }
}

/// Applies an agent's report of the domains on its host. VMs the database
/// places on the host but the agent does not report are marked missing.
async fn observe_domains(state: &AppState, host: HostId, domains: &[DomainState], complete: bool) {
    let mut seen = HashSet::new();
    for d in domains {
        let Ok(vm) = d.vm_id.parse::<VmId>() else {
            continue;
        };
        seen.insert(vm);
        let reason = Some(d.reason.as_str()).filter(|r| !r.is_empty());
        if let Err(e) =
            hm_db::vms::observe_power(&state.db, vm, host, power_name(d.power), reason).await
        {
            tracing::warn!(error = %e, %vm, "recording power state");
        }
    }
    if !complete {
        return;
    }
    match hm_db::vms::list_on_host(&state.db, host).await {
        Ok(vms) => {
            for vm in vms
                .iter()
                .filter(|v| v.status == "ready" && !seen.contains(&v.id))
            {
                let _ = hm_db::vms::observe_power(&state.db, vm.id, host, "missing", None).await;
            }
        }
        Err(e) => tracing::warn!(error = %e, %host, "listing VMs on host"),
    }
}

#[tonic::async_trait]
impl AgentControl for AgentControlService {
    type SessionStream = BoxStream<ControllerMessage>;
    type ConsoleTunnelStream = BoxStream<TunnelChunk>;

    async fn session(
        &self,
        req: Request<Streaming<AgentMessage>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        let host = self.authenticate(&req).await?;
        let state = self.0.clone();
        let mut inbound = req.into_inner();

        let hello = match tokio::time::timeout(SESSION_IDLE_TIMEOUT, inbound.message()).await {
            Ok(Ok(Some(AgentMessage {
                msg: Some(agent_message::Msg::Hello(h)),
            }))) => h,
            _ => return Err(Status::invalid_argument("expected hello")),
        };
        if let Some(f) = &hello.facts {
            hm_db::hosts::update_facts(&state.db, host, &facts_row(f))
                .await
                .map_err(|e| match e {
                    hm_db::Error::Domain(hm_core::Error::Conflict(m)) => Status::already_exists(m),
                    e => internal(e),
                })?;
        }

        let (tx, rx) = mpsc::channel(64);
        let conn = state.agents.register(host, tx);
        hm_db::hosts::set_connected(&state.db, host, Some(&state.replica))
            .await
            .map_err(internal)?;
        observe_domains(&state, host, &hello.domains, true).await;
        tracing::info!(%host, "agent session established");

        tokio::spawn(async move {
            loop {
                let msg = match tokio::time::timeout(SESSION_IDLE_TIMEOUT, inbound.message()).await
                {
                    Ok(Ok(Some(m))) => m,
                    Ok(Ok(None)) => break,
                    Ok(Err(e)) => {
                        tracing::info!(%host, error = %e, "agent session error");
                        break;
                    }
                    Err(_) => {
                        tracing::warn!(%host, "agent session idle; dropping");
                        break;
                    }
                };
                match msg.msg {
                    Some(agent_message::Msg::Heartbeat(hb)) => {
                        let _ = hm_db::hosts::touch(&state.db, host).await;
                        observe_domains(&state, host, &hb.domains, hb.complete).await;
                    }
                    Some(agent_message::Msg::DomainState(d)) => {
                        observe_domains(&state, host, std::slice::from_ref(&d), false).await;
                    }
                    Some(agent_message::Msg::Result(r)) => {
                        conn.complete(&r.request_id, r.error, r.payload)
                    }
                    Some(agent_message::Msg::Hello(_)) | None => {}
                }
            }
            if state.agents.unregister(host, conn.session) {
                let _ = hm_db::hosts::set_connected(&state.db, host, None).await;
                tracing::info!(%host, "agent session closed");
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn console_tunnel(
        &self,
        req: Request<Streaming<TunnelChunk>>,
    ) -> Result<Response<Self::ConsoleTunnelStream>, Status> {
        let host = self.authenticate(&req).await?;
        let mut from_agent = req.into_inner();
        let session_id = match from_agent.message().await? {
            Some(TunnelChunk {
                msg: Some(tunnel_chunk::Msg::SessionId(id)),
            }) => id,
            _ => return Err(Status::invalid_argument("expected session id")),
        };
        let (to_agent, rx) = mpsc::channel(64);
        if !self.0.agents.accept_tunnel(
            host,
            &session_id,
            Tunnel {
                from_agent,
                to_agent,
            },
        ) {
            return Err(Status::not_found("no such console session"));
        }
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}
