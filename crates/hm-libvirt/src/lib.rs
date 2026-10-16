//! libvirt integration for the hypermon agent: deterministic domain XML and an
//! async facade over the blocking libvirt API.

pub mod conn;
pub mod domain;
pub mod xml;

pub use conn::{Libvirt, ManagedDomain, NodeFacts, Power};
pub use domain::{Disk, DiskDevice, DomainDef, Firmware, Metadata, Nic};
pub use virt::stream::Stream;
