//! API error type: domain errors map to HTTP statuses; anything unexpected is
//! logged and reported as a 500 without internals.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    /// Stable machine-readable code, e.g. `not_found`.
    pub error: &'static str,
    pub message: String,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_argument", message)
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "authentication required",
        )
    }

    pub fn forbidden() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "permission denied",
        )
    }

    pub fn not_found(kind: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("{kind} not found"),
        )
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", message)
    }

    pub fn internal(err: impl std::fmt::Display) -> Self {
        tracing::error!(error = %err, "internal error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error",
        )
    }
}

impl From<hm_core::Error> for ApiError {
    fn from(e: hm_core::Error) -> Self {
        use hm_core::Error as E;
        match e {
            E::NotFound { kind, .. } => Self::not_found(kind),
            E::Conflict(m) => Self::conflict(m),
            E::InvalidArgument(m) => Self::bad_request(m),
            E::PermissionDenied => Self::forbidden(),
            e @ E::QuotaExceeded { .. } => {
                Self::new(StatusCode::CONFLICT, "quota_exceeded", e.to_string())
            }
        }
    }
}

impl From<hm_db::Error> for ApiError {
    fn from(e: hm_db::Error) -> Self {
        match e {
            hm_db::Error::Domain(d) => d.into(),
            other => Self::internal(other),
        }
    }
}

impl From<hm_db::sqlx::Error> for ApiError {
    fn from(e: hm_db::sqlx::Error) -> Self {
        Self::internal(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.code,
                message: self.message,
            }),
        )
            .into_response()
    }
}
