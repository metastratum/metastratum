//! gRPC services agents connect to.

use std::collections::HashSet;
use std::pin::Pin;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use futures::Stream;
use mst_core::ids::{HostId, VmId};
use mst_proto::agent_control_server::AgentControl;
use mst_proto::enrollment_server::Enrollment;
use mst_proto::{
    AgentMessage, ControllerMessage, DomainState, EnrollRequest, EnrollResponse, HostFacts,
    PowerState, TunnelChunk, agent_message, tunnel_chunk,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use super::Tunnel;
use crate::state::AppState;

/// Short, so a leaked certificate is not useful for long; agents renew it
/// when a third is left.
pub const HOST_CERT_LIFETIME: Duration = Duration::from_secs(30 * 86400);
/// Agents heartbeat every few seconds; a silent session is presumed dead.
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send>>;

fn internal(e: impl std::fmt::Display) -> Status {
    tracing::error!(error = %e, "agent service error");
    Status::internal("internal error")
}

/// Whether a name a host reports about itself may name it, in its
/// certificate among other places: a DNS host name, not an address, and
/// never the name the controllers' own certificates carry, which agents and
/// replicas check theirs by. Hosts choose these names, so they must not be
/// able to choose that one.
fn valid_host_name(name: &str) -> bool {
    name.len() <= 253
        && name.parse::<std::net::IpAddr>().is_err()
        && name.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
        && !name.eq_ignore_ascii_case(crate::ca::SERVER_NAME)
}

/// What a host's certificate names: its name (when it is a valid one) and
/// its address.
fn host_sans(name: &str, address: &str) -> Vec<String> {
    let mut sans = vec![address.to_owned()];
    if valid_host_name(name) {
        sans.insert(0, name.to_owned());
    }
    sans
}

fn check_facts(f: &HostFacts) -> Result<(), Status> {
    if !valid_host_name(&f.hostname) || f.address.parse::<std::net::IpAddr>().is_err() {
        return Err(Status::invalid_argument(format!(
            "a host name ({:?}: letters, digits, hyphens and dots, not {}) and an IP address are required",
            f.hostname,
            crate::ca::SERVER_NAME
        )));
    }
    Ok(())
}

/// Splits what a host reports about itself into what is taken as it is, and
/// what would give it more than it has been accepted with: a new name,
/// address or OVN chassis, physical networks it did not have (it could make
/// itself a gateway for other tenants' VPCs), more CPUs or memory (to
/// attract VMs). Those wait, with what it reported, for an administrator.
/// Less of anything is taken at once.
fn screen_facts<'a>(
    host: &'a mst_db::hosts::Host,
    f: &'a HostFacts,
    networks: &'a mut Vec<String>,
) -> (mst_db::hosts::Facts<'a>, Option<serde_json::Value>) {
    let mut pending = serde_json::Map::new();
    let mut accepted = facts_row(f);
    if f.hostname != host.name {
        pending.insert("name".into(), f.hostname.clone().into());
        accepted.name = &host.name;
    }
    if f.address != host.address {
        pending.insert("address".into(), f.address.clone().into());
        accepted.address = &host.address;
    }
    if f.ovn_chassis != host.ovn_chassis && !host.ovn_chassis.is_empty() {
        pending.insert("ovn_chassis".into(), f.ovn_chassis.clone().into());
        accepted.ovn_chassis = &host.ovn_chassis;
    }
    if accepted.cpu_count > host.cpu_count {
        pending.insert("cpu_count".into(), accepted.cpu_count.into());
        accepted.cpu_count = host.cpu_count;
    }
    if accepted.memory_mib > host.memory_mib {
        pending.insert("memory_mib".into(), accepted.memory_mib.into());
        accepted.memory_mib = host.memory_mib;
    }
    *networks = f
        .physical_networks
        .iter()
        .filter(|n| host.physical_networks.contains(n))
        .cloned()
        .collect();
    if networks.len() < f.physical_networks.len() {
        pending.insert(
            "physical_networks".into(),
            f.physical_networks.clone().into(),
        );
    }
    accepted.physical_networks = networks;
    let pending = (!pending.is_empty()).then_some(serde_json::Value::Object(pending));
    (accepted, pending)
}

fn facts_row(f: &HostFacts) -> mst_db::hosts::Facts<'_> {
    mst_db::hosts::Facts {
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
        check_facts(&facts)?;
        let token: mst_pki::JoinToken = req
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
                &host_sans(&facts.hostname, &facts.address),
                HOST_CERT_LIFETIME,
            )
            .map_err(|e| Status::invalid_argument(format!("invalid CSR: {e}")))?;
        let der = mst_pki::pem_to_der(&cert_pem).map_err(internal)?;
        let not_after = mst_pki::not_after(&der).map_err(internal)?;

        let mut tx = state.db.begin().await.map_err(internal)?;
        let Some(cluster) = mst_db::join_tokens::consume(&mut *tx, &token.id, &token.secret_hash())
            .await
            .map_err(internal)?
        else {
            return Err(Status::unauthenticated("invalid join token"));
        };
        let host = mst_db::hosts::create(
            &mut *tx,
            host_id,
            &facts_row(&facts),
            &mst_pki::fingerprint(&der),
            Utc.timestamp_opt(not_after, 0)
                .single()
                .unwrap_or_else(Utc::now),
            &mst_pki::serial_of(&der).map_err(internal)?,
        )
        .await
        .map_err(|e| match e {
            mst_db::Error::Domain(mst_core::Error::Conflict(_)) => Status::already_exists(format!(
                "a host named {} is already enrolled",
                facts.hostname
            )),
            e => internal(e),
        })?;
        mst_db::hosts::set_cluster(&mut *tx, host.id, cluster)
            .await
            .map_err(internal)?;
        mst_db::join_tokens::attach_host(&mut *tx, &token.id, host.id)
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
        let host_id: HostId = mst_pki::common_name(leaf)
            .ok()
            .and_then(|cn| cn.parse().ok())
            .ok_or_else(|| Status::unauthenticated("not a host certificate"))?;
        let host = match mst_db::hosts::get(&self.0.db, host_id).await {
            Ok(h) => h,
            Err(mst_db::Error::Domain(mst_core::Error::NotFound { .. })) => {
                return Err(Status::unauthenticated("unknown host"));
            }
            Err(e) => return Err(internal(e)),
        };
        // The current certificate, or the one it renewed (in case the agent
        // never stored the new one).
        let accepted =
            mst_db::hosts::accepts_cert(&self.0.db, host_id, &mst_pki::fingerprint(leaf))
                .await
                .map_err(internal)?;
        if !accepted {
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
            mst_db::vms::observe_power(&state.db, vm, host, power_name(d.power), reason).await
        {
            tracing::warn!(error = %e, %vm, "recording power state");
        }
    }
    if !complete {
        return;
    }
    // Domains of VMs that now live elsewhere (the host was fenced and they
    // restarted on another): remove the stale, stopped definitions.
    let stopped: Vec<VmId> = domains
        .iter()
        .filter(|d| d.power == PowerState::Stopped as i32)
        .filter_map(|d| d.vm_id.parse().ok())
        .collect();
    if !stopped.is_empty() {
        match mst_db::sqlx::query_scalar::<_, VmId>(
            "SELECT v.id FROM vms v WHERE v.id = ANY($1) AND v.host_id IS DISTINCT FROM $2 AND v.status = 'ready'
               AND NOT EXISTS (SELECT 1 FROM migrations m WHERE m.vm_id = v.id)",
        )
        .bind(&stopped)
        .bind(host)
        .fetch_all(&state.db)
        .await
        {
            Ok(stale) => {
                for vm in stale {
                    let agents = state.agents.clone();
                    tokio::spawn(async move {
                        tracing::info!(%host, %vm, "removing a stale definition of a VM that lives elsewhere");
                        let cmd = mst_proto::command::Cmd::UndefineVm(mst_proto::UndefineVm { vm_id: vm.to_string() });
                        if let Err(e) = agents.call(host, cmd, Duration::from_secs(60)).await {
                            tracing::warn!(%host, %vm, error = %e, "removing a stale definition");
                        }
                    });
                }
            }
            Err(e) => tracing::warn!(error = %e, "looking for stale domains"),
        }
    }
    match mst_db::vms::list_on_host(&state.db, host).await {
        Ok(vms) => {
            for vm in vms
                .iter()
                .filter(|v| v.status == "ready" && !seen.contains(&v.id))
            {
                let _ = mst_db::vms::observe_power(&state.db, vm.id, host, "missing", None).await;
            }
        }
        Err(e) => tracing::warn!(error = %e, %host, "listing VMs on host"),
    }
}

#[tonic::async_trait]
impl AgentControl for AgentControlService {
    type SessionStream = BoxStream<ControllerMessage>;
    type ConsoleTunnelStream = BoxStream<TunnelChunk>;

    async fn renew(
        &self,
        req: Request<mst_proto::RenewRequest>,
    ) -> Result<Response<mst_proto::RenewResponse>, Status> {
        let host_id = self.authenticate(&req).await?;
        let state = &self.0;
        let host = mst_db::hosts::get(&state.db, host_id)
            .await
            .map_err(internal)?;
        // Same identity as before: the host's ID, name and address.
        let cert_pem = state
            .ca
            .sign_host_csr(
                &req.get_ref().csr_pem,
                &host_id.to_string(),
                &host_sans(&host.name, &host.address),
                HOST_CERT_LIFETIME,
            )
            .map_err(|e| Status::invalid_argument(format!("invalid CSR: {e}")))?;
        let der = mst_pki::pem_to_der(&cert_pem).map_err(internal)?;
        let not_after = mst_pki::not_after(&der).map_err(internal)?;
        let expires = Utc
            .timestamp_opt(not_after, 0)
            .single()
            .ok_or_else(|| Status::internal("bad expiry"))?;
        let presented = req
            .peer_certs()
            .and_then(|c| c.first().map(|leaf| mst_pki::fingerprint(leaf)))
            .unwrap_or_default();
        let mut conn = state.db.acquire().await.map_err(internal)?;
        let unused = mst_db::hosts::renew_cert(
            &mut conn,
            host_id,
            &presented,
            &mst_pki::fingerprint(&der),
            expires,
            &mst_pki::serial_of(&der).map_err(internal)?,
        )
        .await
        .map_err(internal)?;
        drop(conn);
        crate::crl::revoke(state, host_id, &unused, "superseded").await;
        tracing::info!(host = %host_id, %expires, "host certificate renewed");
        crate::authz::audit(
            &state.db,
            None,
            None,
            "host.cert_renew",
            Some(("host", host_id.0)),
            serde_json::json!({"expires_at": expires}),
        )
        .await;
        Ok(Response::new(mst_proto::RenewResponse { cert_pem }))
    }

    async fn ovn_credentials(
        &self,
        req: Request<mst_proto::OvnCredentialsRequest>,
    ) -> Result<Response<mst_proto::OvnCredentialsResponse>, Status> {
        let host_id = self.authenticate(&req).await?;
        let state = &self.0;
        if state.ovn_sb_remote.is_empty() {
            return Err(Status::failed_precondition(
                "this controller does not manage OVN southbound access",
            ));
        }
        let host = mst_db::hosts::get(&state.db, host_id)
            .await
            .map_err(internal)?;
        // The chassis this host reported, which no other live host may
        // have (a unique index): OVN's RBAC trusts the certificate's name.
        if host.ovn_chassis.is_empty() {
            return Err(Status::failed_precondition(
                "the host reports no OVN chassis name (Open vSwitch system-id)",
            ));
        }
        let cert_pem = state
            .ovn_ca
            .sign_host_csr(
                &req.get_ref().csr_pem,
                &host.ovn_chassis,
                &[],
                HOST_CERT_LIFETIME,
            )
            .map_err(|e| Status::invalid_argument(format!("invalid CSR: {e}")))?;
        crate::authz::audit(
            &state.db,
            None,
            None,
            "host.ovn_cert",
            Some(("host", host_id.0)),
            serde_json::json!({"chassis": host.ovn_chassis}),
        )
        .await;
        Ok(Response::new(mst_proto::OvnCredentialsResponse {
            cert_pem,
            ca_pem: state.ovn_ca.cert_pem().to_owned(),
        }))
    }

    async fn session(
        &self,
        req: Request<Streaming<AgentMessage>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        let host = self.authenticate(&req).await?;
        let state = self.0.clone();
        let presented = req.peer_certs().and_then(|c| {
            c.first()
                .map(|leaf| (mst_pki::fingerprint(leaf), mst_pki::serial_of(leaf).ok()))
        });
        let mut inbound = req.into_inner();

        let hello = match tokio::time::timeout(SESSION_IDLE_TIMEOUT, inbound.message()).await {
            Ok(Ok(Some(AgentMessage {
                msg: Some(agent_message::Msg::Hello(h)),
            }))) => h,
            _ => return Err(Status::invalid_argument("expected hello")),
        };
        // An agent too old is refused before anything it says is kept (a
        // host claiming an old revision must not get older, weaker ways of
        // migrating, or its peers' libvirt listening for it). 0: before
        // versioning, revision 1.
        let version = hello.protocol_version.max(1);
        if version < mst_proto::MIN_AGENT_PROTOCOL {
            return Err(Status::failed_precondition(format!(
                "agent protocol {version} is too old; this controller needs {} or later",
                mst_proto::MIN_AGENT_PROTOCOL
            )));
        }
        // Only then what it says about itself.
        if let Some(f) = &hello.facts {
            check_facts(f)?;
            let known = mst_db::hosts::get(&state.db, host)
                .await
                .map_err(internal)?;
            let mut networks = vec![];
            let (accepted, pending) = screen_facts(&known, f, &mut networks);
            if pending.is_some() && pending != known.pending_facts {
                tracing::warn!(%host, changes = ?pending, "host reports changes that wait for an administrator");
            }
            mst_db::hosts::update_facts(&state.db, host, &accepted, pending.as_ref())
                .await
                .map_err(|e| match e {
                    mst_db::Error::Domain(mst_core::Error::Conflict(m)) => {
                        Status::already_exists(m)
                    }
                    e => internal(e),
                })?;
        }

        let (tx, rx) = mpsc::channel(64);
        let welcome = ControllerMessage {
            msg: Some(mst_proto::controller_message::Msg::Welcome(
                mst_proto::Welcome {
                    protocol_version: mst_proto::PROTOCOL_VERSION,
                    controller_version: env!("CARGO_PKG_VERSION").to_owned(),
                    ovn_sb_remote: state.ovn_sb_remote.clone(),
                    inventory: crate::cleanup::inventory(&state.db).await.ok(),
                    crl_pem: crate::crl::current(&state).await.unwrap_or_else(|e| {
                        tracing::error!(error = %format!("{e:#}"), "signing the revocation list");
                        String::new()
                    }),
                },
            )),
        };
        let _ = tx.send(Ok(welcome)).await;
        let conn = state.agents.register(host, tx, &hello.capabilities);
        tracing::info!(%host, protocol = version, "agent protocol");
        mst_db::hosts::set_agent_protocol(&state.db, host, version, &hello.capabilities)
            .await
            .map_err(internal)?;
        mst_db::hosts::set_connected(&state.db, host, Some(&state.replica))
            .await
            .map_err(internal)?;
        observe_domains(&state, host, &hello.domains, true).await;
        tracing::info!(%host, "agent session established");
        renew_if_outlived(&state, host).await;
        // Connected with its current certificate: the one it replaced is
        // no longer needed.
        if let Some((fp, serial)) = &presented {
            if let Some(serial) = serial {
                let _ = mst_db::hosts::note_cert_serial(&state.db, host, fp, serial).await;
            }
            if let Ok(Some(old)) = mst_db::hosts::retire_previous_cert(&state.db, host, fp).await {
                crate::crl::revoke(&state, host, &[old], "superseded").await;
            }
        }
        if let Ok(h) = mst_db::hosts::get(&state.db, host).await {
            tokio::spawn(libvirt_tls_policy(state.clone(), h.cluster_id, host));
        }

        tokio::spawn(async move {
            loop {
                let next = tokio::select! {
                    m = tokio::time::timeout(SESSION_IDLE_TIMEOUT, inbound.message()) => m,
                    () = conn.close.notified() => {
                        tracing::info!(%host, "ending the agent session");
                        break;
                    }
                };
                let msg = match next {
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
                        let _ = mst_db::hosts::touch(&state.db, host).await;
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
            // Answers can no longer arrive on this session: fail what waits
            // for one now rather than at its timeout.
            conn.fail_pending();
            if state.agents.unregister(host, conn.session) {
                let _ = mst_db::hosts::set_connected(&state.db, host, None).await;
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

/// Tells the hosts of a cluster whether their libvirt must listen for each
/// other over TLS: only while one of them has an agent too old for
/// migrations the controller drives, which migrates peer to peer. A host
/// outside clusters migrates nowhere and never needs to.
pub async fn libvirt_tls_policy(
    state: AppState,
    cluster: Option<mst_core::ids::ClusterId>,
    host: HostId,
) {
    let Ok(hosts) = mst_db::hosts::list(&state.db).await else {
        return;
    };
    // Hosts' libvirt never listens for each other: the controller drives
    // migrations on each host's own (agents from before that are refused).
    // Hosts that once listened, under an older controller, stop.
    for h in hosts
        .iter()
        .filter(|h| h.state != "decommissioned" && h.is_live())
        .filter(|h| match cluster {
            Some(c) => h.cluster_id == Some(c),
            None => h.id == host,
        })
    {
        let cmd = mst_proto::command::Cmd::LibvirtTls(mst_proto::LibvirtTls { listen: false });
        if let Err(e) = state.agents.call(h.id, cmd, Duration::from_secs(120)).await {
            tracing::warn!(host = %h.id, error = %e, "closing libvirt's TLS listener");
        }
    }
}

/// Asks a host to renew a certificate issued for longer than hosts get now
/// (under an earlier policy), so it does not stay valid that long.
async fn renew_if_outlived(state: &AppState, host: HostId) {
    let Ok(h) = mst_db::hosts::get(&state.db, host).await else {
        return;
    };
    let limit = chrono::Utc::now()
        + chrono::Duration::from_std(HOST_CERT_LIFETIME).unwrap_or_default()
        + chrono::Duration::days(1);
    if h.cert_expires_at <= limit {
        return;
    }
    tracing::info!(%host, expires = %h.cert_expires_at, "certificate outlives the current policy; asking the host to renew");
    let agents = state.agents.clone();
    tokio::spawn(async move {
        let renew = mst_proto::command::Cmd::RenewCertificate(mst_proto::RenewCertificate {});
        if let Err(e) = agents
            .send_local(host, renew, Duration::from_secs(60))
            .await
        {
            tracing::warn!(%host, error = %e, "asking the host to renew its certificate");
        }
    });
}

/// Serves other replicas: commands and consoles for agents connected here.
pub struct ReplicaControlService(pub AppState);

impl ReplicaControlService {
    /// Accepts certificates of live replicas only, so the certificate of one
    /// that is gone (they are long-lived) opens nothing.
    async fn authenticate<T>(&self, req: &Request<T>) -> Result<(), Status> {
        let leaf = req
            .peer_certs()
            .and_then(|c| c.first().cloned())
            .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        let replica = match mst_pki::common_name(&leaf) {
            Ok(cn) => cn
                .strip_prefix(super::peers::REPLICA_CN_PREFIX)
                .map(str::to_owned),
            Err(_) => None,
        }
        .ok_or_else(|| Status::permission_denied("not a controller replica"))?;
        if mst_db::replicas::is_live(&self.0.db, &replica)
            .await
            .map_err(internal)?
        {
            Ok(())
        } else {
            Err(Status::permission_denied("not a live controller replica"))
        }
    }
}

fn agent_status(e: super::AgentError) -> Status {
    match e {
        super::AgentError::NotConnected => Status::unavailable(super::peers::NOT_CONNECTED),
        super::AgentError::Timeout(_) => Status::deadline_exceeded(e.to_string()),
        e => Status::unavailable(e.to_string()),
    }
}

#[tonic::async_trait]
impl mst_proto::replica_control_server::ReplicaControl for ReplicaControlService {
    type ConsoleStream = BoxStream<TunnelChunk>;

    async fn call(
        &self,
        req: Request<mst_proto::ForwardedCommand>,
    ) -> Result<Response<mst_proto::CommandResult>, Status> {
        self.authenticate(&req).await?;
        let req = req.into_inner();
        let host: HostId = req
            .host_id
            .parse()
            .map_err(|_| Status::invalid_argument("bad host ID"))?;
        let cmd = req
            .command
            .and_then(|c| c.cmd)
            .ok_or_else(|| Status::invalid_argument("no command"))?;
        // Only sessions held here: a forwarded command is never forwarded again.
        let timeout = Duration::from_millis(req.timeout_ms.clamp(1_000, 6 * 3_600_000));
        let (error, payload) = match self.0.agents.send_local(host, cmd, timeout).await {
            Ok(p) => (String::new(), p),
            Err(super::AgentError::Failed(e)) => (e, String::new()),
            Err(e) => return Err(agent_status(e)),
        };
        Ok(Response::new(mst_proto::CommandResult {
            request_id: String::new(),
            error,
            payload,
        }))
    }

    async fn console(
        &self,
        req: Request<Streaming<TunnelChunk>>,
    ) -> Result<Response<Self::ConsoleStream>, Status> {
        self.authenticate(&req).await?;
        let md = req.metadata();
        let get = |k: &str| md.get(k).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let host: HostId = get("host-id")
            .and_then(|h| h.parse().ok())
            .ok_or_else(|| Status::invalid_argument("host-id"))?;
        let vm: VmId = get("vm-id")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| Status::invalid_argument("vm-id"))?;
        let kind = match get("kind").as_deref() {
            Some("serial") => mst_proto::ConsoleKind::Serial,
            _ => mst_proto::ConsoleKind::Vnc,
        };
        let tunnel = self
            .0
            .agents
            .open_console_local(host, vm, kind)
            .await
            .map_err(agent_status)?;
        let mut inbound = req.into_inner();
        let to_agent = tunnel.to_agent;
        tokio::spawn(async move {
            while let Ok(Some(chunk)) = inbound.message().await {
                if to_agent.send(Ok(chunk)).await.is_err() {
                    break;
                }
            }
        });
        Ok(Response::new(Box::pin(tunnel.from_agent)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_cannot_name_themselves_as_the_controller() {
        for ok in ["mst-host1", "mst-host1.lab.example.net", "h0"] {
            assert!(valid_host_name(ok), "{ok}");
        }
        for bad in [
            "mst-controller",
            "MST-Controller",
            "",
            "10.0.0.5",
            "::1",
            "*.example.net",
            "a..b",
            "host.",
            "-host",
            "host_1",
            "host name",
        ] {
            assert!(!valid_host_name(bad), "{bad}");
        }
        assert_eq!(
            host_sans("mst-controller", "10.0.0.5"),
            ["10.0.0.5".to_owned()]
        );
        assert_eq!(host_sans("h1", "10.0.0.5"), ["h1", "10.0.0.5"]);
    }

    fn known() -> mst_db::hosts::Host {
        mst_db::hosts::Host {
            id: HostId::new(),
            name: "h1".into(),
            address: "10.0.0.5".into(),
            state: "ready".into(),
            cpu_count: 8,
            memory_mib: 16384,
            cpu_model: String::new(),
            kvm: true,
            libvirt_version: String::new(),
            ovn_chassis: "h1".into(),
            physical_networks: vec!["external".into()],
            cpu_json: String::new(),
            migration_ready: true,
            fence_kind: None,
            connected_since: None,
            agent_protocol: 4,
            agent_version: String::new(),
            cert_fingerprint: String::new(),
            cert_expires_at: Utc::now(),
            cluster_id: None,
            connected_to: None,
            last_seen_at: None,
            created_at: Utc::now(),
            pending_facts: None,
        }
    }

    #[test]
    fn hosts_get_more_only_once_accepted() {
        let host = known();
        let same = HostFacts {
            hostname: "h1".into(),
            address: "10.0.0.5".into(),
            cpu_count: 8,
            memory_mib: 16384,
            ovn_chassis: "h1".into(),
            physical_networks: vec!["external".into()],
            ..HostFacts::default()
        };
        let mut nets = vec![];
        let (f, pending) = screen_facts(&host, &same, &mut nets);
        assert!(pending.is_none());
        assert_eq!(f.physical_networks, ["external"]);

        // More, and another identity: held back.
        let more = HostFacts {
            hostname: "h9".into(),
            cpu_count: 64,
            memory_mib: 1 << 20,
            ovn_chassis: "other".into(),
            physical_networks: vec!["external".into(), "tenant-b".into()],
            ..same.clone()
        };
        let (f, pending) = screen_facts(&host, &more, &mut nets);
        assert_eq!(
            (f.name, f.cpu_count, f.memory_mib, f.ovn_chassis),
            ("h1", 8, 16384, "h1")
        );
        assert_eq!(f.physical_networks, ["external"]);
        let pending = pending.unwrap();
        assert_eq!(pending["name"], "h9");
        assert_eq!(pending["cpu_count"], 64);
        assert_eq!(pending["ovn_chassis"], "other");
        assert_eq!(pending["physical_networks"][1], "tenant-b");

        // Less: taken at once.
        let less = HostFacts {
            cpu_count: 4,
            memory_mib: 8192,
            physical_networks: vec![],
            ..same
        };
        let (f, pending) = screen_facts(&host, &less, &mut nets);
        assert!(pending.is_none());
        assert_eq!((f.cpu_count, f.memory_mib), (4, 8192));
        assert!(f.physical_networks.is_empty());
    }
}
