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
