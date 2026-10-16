//! libvirt integration for the hypermon agent: deterministic domain XML and an
//! async facade over libvirt's RPC protocol (no C library).

pub mod conn;
pub mod domain;
pub mod xml;

pub use conn::{Change, Libvirt, ManagedDomain, NodeFacts, Power, system_socket};
pub use domain::{Disk, DiskDevice, DomainDef, Firmware, Metadata, Nic};
pub use hm_virtrpc::{Stream, StreamRx, StreamTx};
