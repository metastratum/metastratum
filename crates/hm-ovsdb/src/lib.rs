//! Minimal OVSDB (RFC 7047) client and the OVN northbound operations hypermon
//! needs.

pub mod client;
pub mod nb;

pub use client::{Client, Error, Result};
pub use nb::{NetworkSpec, Northbound, PortSpec};
