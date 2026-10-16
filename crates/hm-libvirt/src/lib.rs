//! libvirt integration for the hypermon agent: deterministic domain XML and an
//! async facade over libvirt's RPC protocol (no C library).

pub mod conn;
pub mod cpu;
pub mod domain;
pub mod xml;

pub use conn::{
    Change, JobStats, Libvirt, ManagedDomain, MigrateTo, NodeFacts, Power, State, system_socket,
};
pub use cpu::CpuModel;
pub use domain::{Disk, DiskDevice, DiskSource, DomainDef, Firmware, Metadata, Nic};
pub use hm_virtrpc::{Stream, StreamRx, StreamTx};
