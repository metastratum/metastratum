//! Minimal OVSDB (RFC 7047) client and the OVN northbound operations hypermon
//! needs.

pub mod client;
pub mod nb;
pub mod tls;

pub use client::{Client, Error, Result};
pub use nb::Northbound;
