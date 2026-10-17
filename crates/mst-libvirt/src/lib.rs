//! libvirt integration for the metastratum agent: deterministic domain XML and an
//! async facade over libvirt's RPC protocol (no C library).

pub mod conn;
pub mod cpu;
pub mod domain;
pub mod xml;

pub use conn::{
    Change, CompletedJob, JOB_OPERATION_BACKUP, JobStats, Libvirt, ManagedDomain, MemoryStats,
    MigrateTo, MigrationStep, NodeFacts, Power, State, StepOut, system_socket,
};
pub use cpu::CpuModel;
pub use domain::{Disk, DiskDevice, DiskLimits, DiskSource, DomainDef, Firmware, Metadata, Nic};
pub use mst_virtrpc::{Stream, StreamRx, StreamTx};
