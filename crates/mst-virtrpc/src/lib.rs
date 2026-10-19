//! Native async client for libvirt's RPC protocol.
//!
//! libvirt daemons (`libvirtd`, `virtqemud`, ...) speak an XDR-encoded RPC
//! protocol over a unix socket; the C library is one client of it. This crate
//! is another: the protocol types and a typed method per procedure are
//! generated from libvirt's `remote_protocol.x` (vendored in `proto/` under its
//! own licence, LGPL-2.1 or later; see LICENSING.md at the repository root),
//! and the connection is plain tokio. No C
//! library is linked.
//!
//! The protocol is not formally documented, but it is stable in practice:
//! newer daemons keep serving older clients, and the procedure list only
//! grows.

mod client;
pub mod xdr;

/// Types and procedure numbers generated from `remote_protocol.x`.
#[allow(
    clippy::all,
    non_camel_case_types,
    non_upper_case_globals,
    dead_code,
    unused_parens
)]
pub mod protocol {
    use crate::xdr::{self, Xdr};
    include!(concat!(env!("OUT_DIR"), "/remote_protocol.rs"));
}

/// The procedures of libvirt's QEMU program metastratum needs
/// (`qemu_protocol.x`, written out by hand).
pub mod qemu {
    use crate::protocol::NonnullDomain;
    use crate::xdr::{self, Xdr};

    /// `QEMU_PROC_DOMAIN_MONITOR_COMMAND`.
    pub const PROC_DOMAIN_MONITOR_COMMAND: i32 = 1;
    /// `REMOTE_STRING_MAX`.
    const STRING_MAX: usize = 4_194_304;

    /// Passes a QMP command to the domain's QEMU, as `virsh
    /// qemu-monitor-command` does. libvirt marks the domain as tainted.
    pub struct DomainMonitorCommandArgs {
        pub dom: NonnullDomain,
        pub cmd: String,
        pub flags: i32,
    }

    impl Xdr for DomainMonitorCommandArgs {
        fn encode(&self, w: &mut Vec<u8>) -> xdr::Result<()> {
            self.dom.encode(w)?;
            xdr::put_string(w, &self.cmd, STRING_MAX)?;
            xdr::put_i32(w, self.flags);
            Ok(())
        }

        fn decode(r: &mut xdr::Reader<'_>) -> xdr::Result<Self> {
            Ok(Self {
                dom: NonnullDomain::decode(r)?,
                cmd: xdr::get_string(r, STRING_MAX)?,
                flags: xdr::get_i32(r)?,
            })
        }
    }

    /// `QEMU_PROC_DOMAIN_AGENT_COMMAND`.
    pub const PROC_DOMAIN_AGENT_COMMAND: i32 = 3;

    /// Passes a command (JSON) to the domain's guest agent, as `virsh
    /// qemu-agent-command` does, waiting `timeout` seconds at most (or
    /// libvirt's: -2 its default, -1 for ever).
    pub struct DomainAgentCommandArgs {
        pub dom: NonnullDomain,
        pub cmd: String,
        pub timeout: i32,
        pub flags: u32,
    }

    impl Xdr for DomainAgentCommandArgs {
        fn encode(&self, w: &mut Vec<u8>) -> xdr::Result<()> {
            self.dom.encode(w)?;
            xdr::put_string(w, &self.cmd, STRING_MAX)?;
            xdr::put_i32(w, self.timeout);
            xdr::put_u32(w, self.flags);
            Ok(())
        }

        fn decode(r: &mut xdr::Reader<'_>) -> xdr::Result<Self> {
            Ok(Self {
                dom: NonnullDomain::decode(r)?,
                cmd: xdr::get_string(r, STRING_MAX)?,
                timeout: xdr::get_i32(r)?,
                flags: xdr::get_u32(r)?,
            })
        }
    }

    /// The guest agent's reply, as JSON (`remote_string`: may be absent).
    pub struct DomainAgentCommandRet {
        pub result: Option<String>,
    }

    impl Xdr for DomainAgentCommandRet {
        fn encode(&self, w: &mut Vec<u8>) -> xdr::Result<()> {
            xdr::put_bool(w, self.result.is_some());
            match &self.result {
                Some(s) => xdr::put_string(w, s, STRING_MAX),
                None => Ok(()),
            }
        }

        fn decode(r: &mut xdr::Reader<'_>) -> xdr::Result<Self> {
            Ok(Self {
                result: match xdr::get_bool(r)? {
                    true => Some(xdr::get_string(r, STRING_MAX)?),
                    false => None,
                },
            })
        }
    }

    /// QEMU's reply, as JSON.
    pub struct DomainMonitorCommandRet {
        pub result: String,
    }

    impl Xdr for DomainMonitorCommandRet {
        fn encode(&self, w: &mut Vec<u8>) -> xdr::Result<()> {
            xdr::put_string(w, &self.result, STRING_MAX)
        }

        fn decode(r: &mut xdr::Reader<'_>) -> xdr::Result<Self> {
            Ok(Self {
                result: xdr::get_string(r, STRING_MAX)?,
            })
        }
    }
}

pub use client::{Client, Event, Stream, StreamRx, StreamTx};

/// libvirt error codes (`virErrorNumber`) callers commonly match on.
pub mod err {
    pub const NO_SUPPORT: i32 = 3;
    pub const INVALID_ARG: i32 = 8;
    pub const NO_DOMAIN: i32 = 42;
    pub const OPERATION_INVALID: i32 = 55;
    pub const NO_SECRET: i32 = 66;
    pub const NO_DOMAIN_METADATA: i32 = 80;
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("xdr: {0}")]
    Xdr(#[from] xdr::Error),
    #[error("connection closed")]
    Closed,
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    /// An error reported by libvirt; `code` is a `virErrorNumber`.
    #[error("libvirt error {code}: {message}")]
    Libvirt {
        code: i32,
        domain: i32,
        message: String,
    },
}

impl Error {
    /// The libvirt error code, if libvirt reported the error.
    pub fn code(&self) -> Option<i32> {
        match self {
            Self::Libvirt { code, .. } => Some(*code),
            _ => None,
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
