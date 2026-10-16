//! Domain types shared by the metastratum controller, agent and CLI.

pub mod error;
pub mod ids;
pub mod task;
pub mod tenant;
pub mod vm;

pub use error::{Error, Result};
