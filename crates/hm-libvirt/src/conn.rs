//! The agent's view of libvirt, over the native RPC client.
//!
//! One connection serves calls and events. A dropped connection (daemon
//! restart) is replaced on the next call, and the event watcher re-registers
//! and asks its consumer for a full resync.

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hm_virtrpc::protocol::*;
use hm_virtrpc::{Client, Stream, err, xdr};
use tokio::sync::{Mutex, broadcast, mpsc};
use uuid::Uuid;

use crate::domain::{METADATA_NS, Metadata, parse_metadata};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("libvirt: {0}")]
    Rpc(#[from] hm_virtrpc::Error),
    #[error("domain {0} not found")]
    NoDomain(String),
    #[error("invalid domain UUID {0:?}")]
    BadUuid(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    Running,
    Stopped,
    Paused,
    Crashed,
    ShuttingDown,
    Unknown,
}

impl Power {
    /// From a `virDomainState` and its reason.
    fn from_state(state: i32, reason: i32) -> Self {
        const SHUTOFF_CRASHED: i32 = 3;
        match state {
            1 | 2 => Self::Running, // RUNNING, BLOCKED
            // With on_crash=destroy a crashed guest is shut off, not left in
            // CRASHED; the reason tells the two apart.
            5 if reason == SHUTOFF_CRASHED => Self::Crashed,
            5 => Self::Stopped,      // SHUTOFF
            3 | 7 => Self::Paused,   // PAUSED, PMSUSPENDED
            6 => Self::Crashed,      // CRASHED
            4 => Self::ShuttingDown, // SHUTDOWN
            _ => Self::Unknown,      // NOSTATE
        }
    }
}

/// A domain's power state, and why it stopped when libvirt knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct State {
    pub power: Power,
    /// For shut-off domains: `shutdown` (by the guest, or an ACPI request it
    /// honoured), `destroyed` (forced off), `crashed`, `migrated`, `saved`,
    /// `failed` (failed to start), `from_snapshot` or `daemon` (killed by
    /// libvirt). `None` when libvirt does not know, e.g. after the host
    /// restarted.
    pub reason: Option<&'static str>,
}

impl State {
    fn from_libvirt(state: i32, raw: i32) -> Self {
        const SHUTOFF: i32 = 5;
        let reason = match (state, raw) {
            (SHUTOFF, 1) => Some("shutdown"),
            (SHUTOFF, 2) => Some("destroyed"),
            (SHUTOFF, 3) => Some("crashed"),
            (SHUTOFF, 4) => Some("migrated"),
            (SHUTOFF, 5) => Some("saved"),
            (SHUTOFF, 6) => Some("failed"),
            (SHUTOFF, 7) => Some("from_snapshot"),
            (SHUTOFF, 8) => Some("daemon"),
            _ => None,
        };
        Self {
            power: Power::from_state(state, raw),
            reason,
        }
    }
}

/// A hypermon-managed domain as seen on this host.
#[derive(Clone, Debug)]
pub struct ManagedDomain {
    pub meta: Metadata,
    pub state: State,
}

#[derive(Clone, Debug)]
pub struct NodeFacts {
    pub hostname: String,
    pub cpus: u32,
    pub memory_mib: u64,
    pub libvirt_version: String,
}

/// Where and how to live-migrate a domain.
#[derive(Clone, Debug, Default)]
pub struct MigrateTo<'a> {
    /// The destination host's address; its daemon must accept TLS.
    pub address: &'a str,
    /// Disks (target names such as `vda`) on host-local storage, to copy
    /// while the guest runs. Shared and network disks are not copied.
    pub copy_disks: &'a [String],
    pub max_bandwidth_mib: Option<u64>,
}

/// Progress of a running migration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobStats {
    pub active: bool,
    pub elapsed_ms: u64,
    pub data_total: u64,
    pub data_processed: u64,
    pub data_remaining: u64,
    pub dirty_pages_per_s: u64,
    /// The pause at the switch-over, once known.
    pub downtime_ms: Option<u64>,
}

/// What [`Libvirt::watch`] reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// A domain's lifecycle changed (defined, started, stopped, ...).
    Domain(String),
    /// Events may have been missed (new connection, lagging consumer):
    /// re-read everything.
    Resync,
}

const VIR_DOMAIN_EVENT_ID_LIFECYCLE: i32 = 0;
const VIR_DOMAIN_METADATA_ELEMENT: i32 = 2;
const UNDEFINE_MANAGED_SAVE: u32 = 1;
const UNDEFINE_SNAPSHOTS_METADATA: u32 = 2;
const UNDEFINE_NVRAM: u32 = 4;
const UNDEFINE_CHECKPOINTS_METADATA: u32 = 16;
const OPEN_GRAPHICS_SKIPAUTH: u32 = 1;
const CONSOLE_FORCE: u32 = 1;

/// The daemon socket serving `qemu:///system`: modular `virtqemud` if
/// present, the monolithic `libvirtd` otherwise.
pub fn system_socket() -> PathBuf {
    let modular = Path::new("/run/libvirt/virtqemud-sock");
    if modular.exists() {
        modular.into()
    } else {
        "/run/libvirt/libvirt-sock".into()
    }
}

fn uuid_bytes(uuid: &str) -> Result<[u8; 16]> {
    Uuid::parse_str(uuid)
        .map(|u| *u.as_bytes())
        .map_err(|_| Error::BadUuid(uuid.into()))
}

#[derive(Clone)]
pub struct Libvirt {
    socket: Arc<Path>,
    uri: Arc<str>,
    client: Arc<Mutex<Option<Client>>>,
}

impl Libvirt {
    pub fn new(socket: impl Into<PathBuf>, uri: &str) -> Self {
        Self {
            socket: socket.into().into(),
            uri: uri.into(),
            client: Arc::default(),
        }
    }

    async fn client(&self) -> Result<Client> {
        let mut slot = self.client.lock().await;
        if let Some(c) = slot.as_ref().filter(|c| !c.is_closed()) {
            return Ok(c.clone());
        }
        if slot.is_some() {
            tracing::warn!("libvirt connection lost; reconnecting");
        }
        let c = Client::connect(&*self.socket, &self.uri).await?;
        *slot = Some(c.clone());
        Ok(c)
    }

    async fn lookup(&self, c: &Client, uuid: &str) -> Result<NonnullDomain> {
        match c
            .domain_lookup_by_uuid(&DomainLookupByUuidArgs {
                uuid: uuid_bytes(uuid)?,
            })
            .await
        {
            Ok(r) => Ok(r.dom),
            Err(e) if e.code() == Some(err::NO_DOMAIN) => Err(Error::NoDomain(uuid.into())),
            Err(e) => Err(e.into()),
        }
    }

    async fn dom_state(&self, c: &Client, dom: &NonnullDomain) -> Result<State> {
        let st = c
            .domain_get_state(&DomainGetStateArgs {
                dom: dom.clone(),
                flags: 0,
            })
            .await?;
        Ok(State::from_libvirt(st.state, st.reason))
    }

    async fn power_of(&self, c: &Client, dom: &NonnullDomain) -> Result<Power> {
        Ok(self.dom_state(c, dom).await?.power)
    }

    pub async fn node_facts(&self) -> Result<NodeFacts> {
        let c = self.client().await?;
        let info = c.node_get_info().await?;
        let v = c.connect_get_lib_version().await?.lib_ver;
        Ok(NodeFacts {
            hostname: c.connect_get_hostname().await?.hostname,
            cpus: info.cpus.max(0) as u32,
            memory_mib: info.memory / 1024,
            libvirt_version: format!("{}.{}.{}", v / 1_000_000, v / 1000 % 1000, v % 1000),
        })
    }

    /// Defines or redefines a domain from XML.
    pub async fn define(&self, xml: String) -> Result<()> {
        self.client()
            .await?
            .domain_define_xml(&DomainDefineXmlArgs { xml })
            .await?;
        Ok(())
    }

    pub async fn start(&self, uuid: &str) -> Result<()> {
        let c = self.client().await?;
        let dom = self.lookup(&c, uuid).await?;
        if self.power_of(&c, &dom).await? != Power::Running {
            c.domain_create(&DomainCreateArgs { dom }).await?;
        }
        Ok(())
    }

    /// ACPI shutdown request; the guest decides when to power off.
    pub async fn shutdown(&self, uuid: &str) -> Result<()> {
        let c = self.client().await?;
        let dom = self.lookup(&c, uuid).await?;
        if self.power_of(&c, &dom).await? != Power::Stopped {
            c.domain_shutdown(&DomainShutdownArgs { dom }).await?;
        }
        Ok(())
    }

    /// Immediate power off.
    pub async fn destroy(&self, uuid: &str) -> Result<()> {
        let c = self.client().await?;
        let dom = self.lookup(&c, uuid).await?;
        if self.power_of(&c, &dom).await? != Power::Stopped {
            c.domain_destroy(&DomainDestroyArgs { dom }).await?;
        }
        Ok(())
    }

    pub async fn reboot(&self, uuid: &str) -> Result<()> {
        let c = self.client().await?;
        let dom = self.lookup(&c, uuid).await?;
        c.domain_reboot(&DomainRebootArgs { dom, flags: 0 }).await?;
        Ok(())
    }

    /// Powers off and undefines the domain, including its UEFI variable
    /// store. Succeeds if the domain does not exist.
    pub async fn undefine(&self, uuid: &str) -> Result<()> {
        let c = self.client().await?;
        let dom = match self.lookup(&c, uuid).await {
            Ok(d) => d,
            Err(Error::NoDomain(_)) => return Ok(()),
            Err(e) => return Err(e),
        };
        if self.power_of(&c, &dom).await? != Power::Stopped {
            c.domain_destroy(&DomainDestroyArgs { dom: dom.clone() })
                .await?;
        }
        let basic = UNDEFINE_MANAGED_SAVE | UNDEFINE_SNAPSHOTS_METADATA;
        let full = basic | UNDEFINE_NVRAM | UNDEFINE_CHECKPOINTS_METADATA;
        match c
            .domain_undefine_flags(&DomainUndefineFlagsArgs {
                dom: dom.clone(),
                flags: full,
            })
            .await
        {
            // Drivers without UEFI/checkpoint support reject those flags;
            // nothing of that kind exists there.
            Err(e) if e.code() == Some(err::INVALID_ARG) => {
                c.domain_undefine_flags(&DomainUndefineFlagsArgs { dom, flags: basic })
                    .await?;
            }
            r => {
                r?;
            }
        }
        Ok(())
    }

    /// All domains carrying hypermon metadata. Others on the host are ignored.
    pub async fn list_managed(&self) -> Result<Vec<ManagedDomain>> {
        let c = self.client().await?;
        let all = c
            .connect_list_all_domains(&ConnectListAllDomainsArgs {
                need_results: 1,
                flags: 0,
            })
            .await?;
        let mut out = Vec::new();
        for dom in all.domains {
            let args = DomainGetMetadataArgs {
                dom: dom.clone(),
                r#type: VIR_DOMAIN_METADATA_ELEMENT,
                uri: Some(METADATA_NS.into()),
                flags: 0,
            };
            let xml = match c.domain_get_metadata(&args).await {
                Ok(r) => r.metadata,
                // Not ours, or removed while we were listing.
                Err(e) if matches!(e.code(), Some(err::NO_DOMAIN_METADATA | err::NO_DOMAIN)) => {
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let Some(meta) = parse_metadata(&xml) else {
                continue;
            };
            match self.dom_state(&c, &dom).await {
                Ok(state) => out.push(ManagedDomain { meta, state }),
                Err(Error::Rpc(e)) if e.code() == Some(err::NO_DOMAIN) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    pub async fn power(&self, uuid: &str) -> Result<Power> {
        Ok(self.state(uuid).await?.power)
    }

    pub async fn state(&self, uuid: &str) -> Result<State> {
        let c = self.client().await?;
        let dom = self.lookup(&c, uuid).await?;
        self.dom_state(&c, &dom).await
    }

    /// A socket connected directly to the domain's VNC server, passed by the
    /// daemon. Authentication is skipped: the controller already authorised
    /// the user.
    pub async fn open_vnc(&self, uuid: &str) -> Result<OwnedFd> {
        let c = self.client().await?;
        let dom = self.lookup(&c, uuid).await?;
        let args = DomainOpenGraphicsFdArgs {
            dom,
            idx: 0,
            flags: OPEN_GRAPHICS_SKIPAUTH,
        };
        let ((), fds) = c
            .call_with_fds::<_, ()>(proc::DOMAIN_OPEN_GRAPHICS_FD, &args)
            .await?;
        fds.into_iter()
            .next()
            .ok_or_else(|| hm_virtrpc::Error::Protocol("graphics descriptor missing".into()).into())
    }

    /// A stream attached to the domain's serial console. `force` disconnects
    /// any other session, so a stale one cannot lock users out.
    pub async fn open_serial(&self, uuid: &str) -> Result<Stream> {
        let c = self.client().await?;
        let dom = self.lookup(&c, uuid).await?;
        let args = DomainOpenConsoleArgs {
            dom,
            dev_name: None,
            flags: CONSOLE_FORCE,
        };
        Ok(c.open_stream(proc::DOMAIN_OPEN_CONSOLE, &args).await?)
    }

    /// Makes sure a Ceph secret with this UUID exists and holds `key` (the
    /// base64 key from `ceph auth get-key`), for RBD disks to authenticate
    /// with. The value is set every time, so a rotated key takes effect.
    pub async fn ensure_ceph_secret(&self, uuid: &str, usage: &str, key: &str) -> Result<()> {
        use base64::Engine;
        let value = base64::engine::general_purpose::STANDARD
            .decode(key.trim())
            .map_err(|_| hm_virtrpc::Error::Protocol("Ceph key is not valid base64".into()))?;
        let c = self.client().await?;
        let secret = match c
            .secret_lookup_by_uuid(&SecretLookupByUuidArgs {
                uuid: uuid_bytes(uuid)?,
            })
            .await
        {
            Ok(r) => r.secret,
            Err(e) if e.code() == Some(err::NO_SECRET) => {
                let xml = format!(
                    "<secret ephemeral='no' private='yes'><uuid>{uuid}</uuid><usage type='ceph'><name>{}</name></usage></secret>",
                    crate::xml::escape(usage)
                );
                c.secret_define_xml(&SecretDefineXmlArgs { xml, flags: 0 })
                    .await?
                    .secret
            }
            Err(e) => return Err(e.into()),
        };
        c.secret_set_value(&SecretSetValueArgs {
            secret,
            value,
            flags: 0,
        })
        .await?;
        Ok(())
    }

    /// The host's host-model CPU, for cluster baselines.
    pub async fn host_cpu(&self) -> Result<Option<crate::cpu::CpuModel>> {
        let caps = self
            .client()
            .await?
            .connect_get_domain_capabilities(&ConnectGetDomainCapabilitiesArgs {
                emulatorbin: None,
                arch: None,
                machine: None,
                virttype: Some("kvm".into()),
                flags: 0,
            })
            .await?
            .capabilities;
        Ok(crate::cpu::host_model(&caps))
    }

    /// The most capable CPU all of `cpus` can run, limited to features that
    /// survive migration.
    pub async fn baseline_cpu(
        &self,
        cpus: &[crate::cpu::CpuModel],
    ) -> Result<Option<crate::cpu::CpuModel>> {
        const BASELINE_MIGRATABLE: u32 = 2;
        let r = self
            .client()
            .await?
            .connect_baseline_hypervisor_cpu(&ConnectBaselineHypervisorCpuArgs {
                emulator: None,
                arch: None,
                machine: None,
                virttype: Some("kvm".into()),
                xml_cp_us: cpus.iter().map(crate::cpu::CpuModel::to_xml).collect(),
                flags: BASELINE_MIGRATABLE,
            })
            .await?;
        Ok(crate::cpu::parse_cpu(&r.cpu))
    }

    /// Live-migrates a running domain to another host, peer to peer: this
    /// host's daemon connects to the destination's over TLS and drives the
    /// whole migration, including copying local disks. Returns when it has
    /// finished (the domain then runs on the destination and is gone here)
    /// or failed (it keeps running here).
    pub async fn migrate(&self, uuid: &str, to: &MigrateTo<'_>) -> Result<()> {
        // virDomainMigrateFlags
        const LIVE: u32 = 1;
        const PEER2PEER: u32 = 2;
        const PERSIST_DEST: u32 = 8;
        const UNDEFINE_SOURCE: u32 = 16;
        const NON_SHARED_INC: u32 = 128;
        const ABORT_ON_ERROR: u32 = 4096;
        const AUTO_CONVERGE: u32 = 8192;
        const TLS: u32 = 65536;

        let c = self.client().await?;
        let dom = self.lookup(&c, uuid).await?;
        let s = |field: &str, v: &str| TypedParam {
            field: field.into(),
            value: TypedParamValue::VirTypedParamString(v.into()),
        };
        // Name the destination by address throughout: hosts need not resolve
        // each other's names, and certificates carry the address.
        let mut params = vec![
            s("migrate_uri", &format!("tcp://{}", to.address)),
            s("tls.destination", to.address),
        ];
        let mut flags = LIVE
            | PEER2PEER
            | PERSIST_DEST
            | UNDEFINE_SOURCE
            | ABORT_ON_ERROR
            | AUTO_CONVERGE
            | TLS;
        if !to.copy_disks.is_empty() {
            // Only the top layer: the destination has the base image.
            flags |= NON_SHARED_INC;
            params.extend(to.copy_disks.iter().map(|d| s("migrate_disks", d)));
        }
        if let Some(mib) = to.max_bandwidth_mib {
            params.push(TypedParam {
                field: "bandwidth".into(),
                value: TypedParamValue::VirTypedParamUllong(mib),
            });
        }
        c.domain_migrate_perform3_params(&DomainMigratePerform3ParamsArgs {
            dom,
            dconnuri: Some(format!("qemu+tls://{}/system", to.address)),
            params,
            cookie_in: vec![],
            flags,
        })
        .await?;
        Ok(())
    }

    /// Progress of the domain's current job (a migration), if any.
    pub async fn job_stats(&self, uuid: &str) -> Result<JobStats> {
        let c = self.client().await?;
        let dom = self.lookup(&c, uuid).await?;
        let r = c
            .domain_get_job_stats(&DomainGetJobStatsArgs { dom, flags: 0 })
            .await?;
        let mut st = JobStats {
            // virDomainJobType: BOUNDED or UNBOUNDED while running.
            active: matches!(r.r#type, 1 | 2),
            ..JobStats::default()
        };
        for p in r.params {
            let TypedParamValue::VirTypedParamUllong(v) = p.value else {
                continue;
            };
            match p.field.as_str() {
                "time_elapsed" => st.elapsed_ms = v,
                "data_total" => st.data_total = v,
                "data_processed" => st.data_processed = v,
                "data_remaining" => st.data_remaining = v,
                "memory_dirty_rate" => st.dirty_pages_per_s = v,
                "downtime" => st.downtime_ms = Some(v),
                _ => {}
            }
        }
        Ok(st)
    }

    /// Aborts the domain's current job. A migration then fails and the domain
    /// keeps running here.
    pub async fn abort_job(&self, uuid: &str) -> Result<()> {
        let c = self.client().await?;
        let dom = self.lookup(&c, uuid).await?;
        c.domain_abort_job(&DomainAbortJobArgs { dom }).await?;
        Ok(())
    }

    /// Streams domain lifecycle changes. Survives daemon restarts: after each
    /// (re)connection it re-registers and sends [`Change::Resync`].
    pub fn watch(&self) -> mpsc::Receiver<Change> {
        let (tx, rx) = mpsc::channel(256);
        let this = self.clone();
        tokio::spawn(async move {
            let mut backoff = Duration::from_millis(500);
            while !tx.is_closed() {
                match this.watch_once(&tx).await {
                    Ok(()) => return,
                    Err(e) => tracing::warn!(error = %e, "libvirt event watch interrupted"),
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
        });
        rx
    }

    /// Watches until the connection drops (error) or the consumer goes away.
    async fn watch_once(&self, tx: &mpsc::Sender<Change>) -> Result<()> {
        let c = self.client().await?;
        // Subscribe before registering so nothing slips through in between.
        let mut events = c.events();
        let args = ConnectDomainEventCallbackRegisterAnyArgs {
            event_id: VIR_DOMAIN_EVENT_ID_LIFECYCLE,
            dom: None,
        };
        c.connect_domain_event_callback_register_any(&args).await?;
        if tx.send(Change::Resync).await.is_err() {
            return Ok(());
        }
        loop {
            let change = match events.recv().await {
                Ok(ev) if ev.proc == proc::DOMAIN_EVENT_CALLBACK_LIFECYCLE => {
                    match xdr::from_bytes::<DomainEventCallbackLifecycleMsg>(&ev.body) {
                        Ok(m) => Change::Domain(Uuid::from_bytes(m.msg.dom.uuid).to_string()),
                        Err(e) => {
                            tracing::warn!(error = %e, "undecodable lifecycle event");
                            Change::Resync
                        }
                    }
                }
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => Change::Resync,
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(hm_virtrpc::Error::Closed.into());
                }
            };
            if tx.send(change).await.is_err() {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reasons_are_reported_and_crashes_are_not_stops() {
        let st = |s, r| State::from_libvirt(s, r);
        assert_eq!(
            st(5, 1),
            State {
                power: Power::Stopped,
                reason: Some("shutdown")
            }
        );
        assert_eq!(
            st(5, 2),
            State {
                power: Power::Stopped,
                reason: Some("destroyed")
            }
        );
        assert_eq!(
            st(5, 3),
            State {
                power: Power::Crashed,
                reason: Some("crashed")
            }
        );
        assert_eq!(
            st(5, 0),
            State {
                power: Power::Stopped,
                reason: None
            }
        );
        assert_eq!(
            st(6, 0),
            State {
                power: Power::Crashed,
                reason: None
            }
        );
        assert_eq!(
            st(1, 1),
            State {
                power: Power::Running,
                reason: None
            }
        );
    }
}
