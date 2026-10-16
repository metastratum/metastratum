//! Domain types shared by the hypermon controller, agent and CLI.

pub mod error;
pub mod ids;
pub mod task;
pub mod tenant;

pub use error::{Error, Result};
