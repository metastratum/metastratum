use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors that carry domain meaning across crate boundaries. Infrastructure
/// errors (database, I/O) are wrapped by the crates that produce them.
#[derive(Debug, Error)]
pub enum Error {
    #[error("{kind} {id} not found")]
    NotFound { kind: &'static str, id: String },
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("permission denied")]
    PermissionDenied,
    #[error("quota exceeded for {resource} in tenant {tenant}")]
    QuotaExceeded { resource: String, tenant: String },
}
