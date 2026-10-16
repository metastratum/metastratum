//! Async facade over the blocking `virt` bindings.
//!
//! libvirt connections are thread-safe, so each call runs on tokio's blocking
//! pool against a shared connection. A dead connection (libvirtd restarted)
//! is detected with `is_alive` and transparently reopened on the next call.

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

use virt::connect::Connect;
use virt::domain::Domain;
use virt::error::ErrorNumber;
use virt::stream::Stream;
use virt::sys;

use crate::domain::{METADATA_NS, Metadata, parse_metadata};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("libvirt: {0}")]
    Libvirt(#[from] virt::error::Error),
    #[error("domain {0} not found")]
    NoDomain(String),
    #[error("blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// `virt::connect::Connect` has no `Drop`; this closes the connection once the
/// last user is done with it.
struct Conn(Connect);

impl Drop for Conn {
    fn drop(&mut self) {
        let _ = self.0.close();
    }
}

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
    fn from_state(state: sys::virDomainState) -> Self {
        match state {
            sys::VIR_DOMAIN_RUNNING | sys::VIR_DOMAIN_BLOCKED => Self::Running,
            sys::VIR_DOMAIN_SHUTOFF => Self::Stopped,
            sys::VIR_DOMAIN_PAUSED | sys::VIR_DOMAIN_PMSUSPENDED => Self::Paused,
            sys::VIR_DOMAIN_CRASHED => Self::Crashed,
            sys::VIR_DOMAIN_SHUTDOWN => Self::ShuttingDown,
            _ => Self::Unknown,
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
    pub cpu_model: String,
    pub libvirt_version: String,
}

#[derive(Clone)]
pub struct Libvirt {
    uri: Arc<str>,
    conn: Arc<Mutex<Option<Arc<Conn>>>>,
}

impl Libvirt {
    pub fn new(uri: &str) -> Self {
        // libvirt prints every error to stderr by default; errors are already
        // returned to (and reported by) callers.
        virt::error::clear_error_callback();
        Self {
            uri: uri.into(),
            conn: Arc::default(),
        }
    }

    fn connection(&self) -> Result<Arc<Conn>> {
        let mut slot = self.conn.lock().expect("libvirt connection mutex poisoned");
        if let Some(c) = slot.as_ref() {
            if c.0.is_alive().unwrap_or(false) {
                return Ok(c.clone());
            }
            tracing::warn!("libvirt connection lost; reconnecting");
        }
        let c = Arc::new(Conn(Connect::open(Some(&self.uri))?));
        *slot = Some(c.clone());
        Ok(c)
    }

    async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connect) -> Result<T> + Send + 'static,
    {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let conn = this.connection()?;
            f(&conn.0)
        })
        .await?
    }

    pub async fn node_facts(&self) -> Result<NodeFacts> {
        self.run(|c| {
            let info = c.get_node_info()?;
            let v = c.get_lib_version()?;
            Ok(NodeFacts {
                hostname: c.get_hostname()?,
                cpus: info.cpus,
                memory_mib: info.memory / 1024,
                cpu_model: info.model,
                libvirt_version: format!("{}.{}.{}", v / 1_000_000, v / 1000 % 1000, v % 1000),
            })
        })
        .await
    }

    /// Defines or redefines a domain from XML.
    pub async fn define(&self, xml: String) -> Result<()> {
        self.run(move |c| {
            Domain::define_xml(c, &xml)?;
            Ok(())
        })
        .await
    }

    pub async fn start(&self, uuid: &str) -> Result<()> {
        let uuid = uuid.to_owned();
        self.run(move |c| {
            let d = lookup(c, &uuid)?;
            if !d.is_active()? {
                d.create()?;
            }
            Ok(())
        })
        .await
    }

    /// ACPI shutdown request; the guest decides when to power off.
    pub async fn shutdown(&self, uuid: &str) -> Result<()> {
        let uuid = uuid.to_owned();
        self.run(move |c| {
            let d = lookup(c, &uuid)?;
            if d.is_active()? {
                d.shutdown()?;
            }
            Ok(())
        })
        .await
    }

    /// Immediate power off.
    pub async fn destroy(&self, uuid: &str) -> Result<()> {
        let uuid = uuid.to_owned();
        self.run(move |c| {
            let d = lookup(c, &uuid)?;
            if d.is_active()? {
                d.destroy()?;
            }
            Ok(())
        })
        .await
    }

    pub async fn reboot(&self, uuid: &str) -> Result<()> {
        let uuid = uuid.to_owned();
        self.run(move |c| {
            lookup(c, &uuid)?.reboot(0)?;
            Ok(())
        })
        .await
    }

    /// Powers off and undefines the domain, including its UEFI variable
    /// store. Succeeds if the domain does not exist.
    pub async fn undefine(&self, uuid: &str) -> Result<()> {
        let uuid = uuid.to_owned();
        self.run(move |c| {
            let d = match lookup(c, &uuid) {
                Ok(d) => d,
                Err(Error::NoDomain(_)) => return Ok(()),
                Err(e) => return Err(e),
            };
            if d.is_active()? {
                d.destroy()?;
            }
            let basic =
                sys::VIR_DOMAIN_UNDEFINE_MANAGED_SAVE | sys::VIR_DOMAIN_UNDEFINE_SNAPSHOTS_METADATA;
            let full = basic
                | sys::VIR_DOMAIN_UNDEFINE_NVRAM
                | sys::VIR_DOMAIN_UNDEFINE_CHECKPOINTS_METADATA;
            match d.undefine_flags(full) {
                // Drivers without UEFI/checkpoint support (e.g. the test
                // driver) reject those flags; nothing of that kind exists there.
                Err(e) if e.code() == ErrorNumber::InvalidArg => d.undefine_flags(basic)?,
                r => r?,
            }
            Ok(())
        })
        .await
    }

    /// All domains carrying hypermon metadata. Others on the host are ignored.
    pub async fn list_managed(&self) -> Result<Vec<ManagedDomain>> {
        self.run(|c| {
            let mut out = Vec::new();
            for d in c.list_all_domains(0)? {
                let Ok(xml) = d.get_metadata(
                    sys::VIR_DOMAIN_METADATA_ELEMENT as i32,
                    Some(METADATA_NS),
                    0,
                ) else {
                    continue;
                };
                let Some(meta) = parse_metadata(&xml) else {
                    continue;
                };
                let (state, _reason) = d.get_state()?;
                out.push(ManagedDomain {
                    meta,
                    power: Power::from_state(state),
                });
            }
            Ok(out)
        })
        .await
    }

    pub async fn power(&self, uuid: &str) -> Result<Power> {
        let uuid = uuid.to_owned();
        self.run(move |c| Ok(Power::from_state(lookup(c, &uuid)?.get_state()?.0)))
            .await
    }

    /// A socket connected directly to the domain's VNC server. Authentication
    /// is skipped because access control happens in the controller.
    pub async fn open_vnc(&self, uuid: &str) -> Result<OwnedFd> {
        let uuid = uuid.to_owned();
        self.run(move |c| {
            let fd =
                lookup(c, &uuid)?.open_graphics_fd(0, sys::VIR_DOMAIN_OPEN_GRAPHICS_SKIPAUTH)?;
            // SAFETY: virDomainOpenGraphicsFD returns a new descriptor that the
            // caller owns.
            #[allow(unsafe_code)]
            Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
        })
        .await
    }

    /// A blocking stream attached to the domain's serial console. `force`
    /// disconnects any other session, so a stale one cannot lock users out.
    pub async fn open_serial(&self, uuid: &str) -> Result<Stream> {
        let uuid = uuid.to_owned();
        self.run(move |c| {
            let d = lookup(c, &uuid)?;
            let stream = Stream::new(c, 0)?;
            d.open_console(None, &stream, sys::VIR_DOMAIN_CONSOLE_FORCE)?;
            Ok(stream)
        })
        .await
    }
}

fn lookup(c: &Connect, uuid: &str) -> Result<Domain> {
    match Domain::lookup_by_uuid_string(c, uuid) {
        Ok(d) => Ok(d),
        Err(e) if e.code() == ErrorNumber::NoDomain => Err(Error::NoDomain(uuid.to_owned())),
        Err(e) => Err(e.into()),
    }
}
