//! Application error type mapped to HTTP JSON responses.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug)]
pub enum AppError {
    /// Bad parameter, invalid regex, etc.
    BadRequest(String),
    /// Missing/invalid bearer token.
    Unauthorized,
    /// Path escapes the log root, or permission denied.
    Forbidden(String),
    /// File not found.
    NotFound(String),
    /// Search/stream limit reached.
    TooManyRequests,
    /// Unexpected I/O or internal error. The detail is logged server-side;
    /// clients only ever see a generic message (no OS error internals).
    Internal(String),
}

impl AppError {
    fn parts(&self) -> (StatusCode, String) {
        match self {
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".into()),
            AppError::Forbidden(m) => (StatusCode::FORBIDDEN, m.clone()),
            AppError::NotFound(m) => (StatusCode::NOT_FOUND, m.clone()),
            AppError::TooManyRequests => {
                (StatusCode::TOO_MANY_REQUESTS, "search limit reached".into())
            }
            AppError::Internal(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal server error".into(),
            ),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if let AppError::Internal(detail) = &self {
            tracing::error!("internal error: {detail}");
        }
        let (status, msg) = self.parts();
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => AppError::NotFound("file not found".into()),
            std::io::ErrorKind::PermissionDenied => AppError::Forbidden("permission denied".into()),
            _ => AppError::Internal(e.to_string()),
        }
    }
}

impl From<crate::grep::GrepError> for AppError {
    fn from(e: crate::grep::GrepError) -> Self {
        AppError::BadRequest(e.to_string())
    }
}
