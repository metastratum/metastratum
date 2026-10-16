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
    /// From a `virDomainState`.
    fn from_state(state: i32) -> Self {
        match state {
            1 | 2 => Self::Running,  // RUNNING, BLOCKED
            5 => Self::Stopped,      // SHUTOFF
            3 | 7 => Self::Paused,   // PAUSED, PMSUSPENDED
            6 => Self::Crashed,      // CRASHED
            4 => Self::ShuttingDown, // SHUTDOWN
            _ => Self::Unknown,      // NOSTATE
        }
    }
}

/// A hypermon-managed domain as seen on this host.
#[derive(Clone, Debug)]
pub struct ManagedDomain {
    pub meta: Metadata,
    pub power: Power,
}

#[derive(Clone, Debug)]
pub struct NodeFacts {
    pub hostname: String,
    pub cpus: u32,
    pub memory_mib: u64,
    pub libvirt_version: String,
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

    async fn state(&self, c: &Client, dom: &NonnullDomain) -> Result<Power> {
        let st = c
            .domain_get_state(&DomainGetStateArgs {
                dom: dom.clone(),
                flags: 0,
            })
            .await?;
        Ok(Power::from_state(st.state))
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
        if self.state(&c, &dom).await? != Power::Running {
            c.domain_create(&DomainCreateArgs { dom }).await?;
        }
        Ok(())
    }

    /// ACPI shutdown request; the guest decides when to power off.
    pub async fn shutdown(&self, uuid: &str) -> Result<()> {
        let c = self.client().await?;
        let dom = self.lookup(&c, uuid).await?;
        if self.state(&c, &dom).await? != Power::Stopped {
            c.domain_shutdown(&DomainShutdownArgs { dom }).await?;
        }
        Ok(())
    }

    /// Immediate power off.
    pub async fn destroy(&self, uuid: &str) -> Result<()> {
        let c = self.client().await?;
        let dom = self.lookup(&c, uuid).await?;
        if self.state(&c, &dom).await? != Power::Stopped {
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
        if self.state(&c, &dom).await? != Power::Stopped {
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
            match self.state(&c, &dom).await {
                Ok(power) => out.push(ManagedDomain { meta, power }),
                Err(Error::Rpc(e)) if e.code() == Some(err::NO_DOMAIN) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    pub async fn power(&self, uuid: &str) -> Result<Power> {
        let c = self.client().await?;
        let dom = self.lookup(&c, uuid).await?;
        self.state(&c, &dom).await
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
