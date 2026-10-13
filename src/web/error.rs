//! Structured error codes and the framework's error type.
//!
//! Mirrors `web/error.go`: a single [`Error`] type carries both the HTTP status
//! code and the business code, and is used as the framework-wide error type.

use std::fmt;

/// Business / HTTP status codes used by the framework.
pub mod code {
    pub const OK: i32 = 200;
    pub const BAD_REQUEST: i32 = 400;
    pub const UNAUTHORIZED: i32 = 401;
    pub const FORBIDDEN: i32 = 403;
    pub const NOT_FOUND: i32 = 404;
    pub const METHOD_NOT_ALLOWED: i32 = 405;
    pub const TOO_MANY_REQUESTS: i32 = 429;
    pub const INTERNAL_ERROR: i32 = 500;
    pub const SERVICE_UNAVAILABLE: i32 = 503;

    // Business codes (1000+), following the Go original.
    pub const VALIDATION_FAILED: i32 = 1001;
    pub const DUPLICATE_ENTRY: i32 = 1002;
    pub const TOKEN_EXPIRED: i32 = 1003;
    pub const TOKEN_INVALID: i32 = 1004;

    /// Maps a business code to the HTTP status that should be sent on the wire.
    ///
    /// Codes outside the HTTP range (e.g. `1000+` business codes) are reported to the
    /// client with HTTP 200 so that browsers and proxies do not mistake them for
    /// transport failures — the envelope's `code` field carries the real meaning.
    pub fn http_status(code: i32) -> u16 {
        if (100..=599).contains(&code) {
            code as u16
        } else {
            200
        }
    }
}

/// A structured error: an HTTP/business code, a short message, optional detail and an
/// optional wrapped source error.
///
/// This is the framework's single error type. Anything that implements
/// [`std::error::Error`] + `Send` + `Sync` + `'static` converts into it via the explicit
/// [`From`] impls below, and plain `&str` / [`String`] convert into a 500.
#[derive(Debug, Clone)]
pub struct Error {
    /// HTTP status code, or a business code (1000+).
    pub code: i32,
    /// Short, stable message. Serialised to clients as `message`.
    pub message: String,
    /// Optional human readable detail, shown in place of `message` when present.
    pub detail: Option<String>,
    /// Optional wrapped source error.
    pub source: Option<std::sync::Arc<dyn std::error::Error + Send + Sync>>,
}

impl Error {
    /// Creates a new error with the given code and message.
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
            source: None,
        }
    }

    /// Creates a new error carrying an existing error as its source.
    pub fn with_source<E>(code: i32, err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self {
            code,
            message: err.to_string(),
            detail: None,
            source: Some(std::sync::Arc::new(err)),
        }
    }

    /// Attaches human readable detail. Chainable.
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Attaches the originating error. Chainable.
    pub fn caused_by<E>(mut self, err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        self.source = Some(std::sync::Arc::new(err));
        self
    }

    /// The HTTP status code to send on the wire.
    pub fn http_status(&self) -> u16 {
        code::http_status(self.code)
    }

    /// The message shown to clients: `detail` if present, else the wrapped source, else
    /// `message`.
    pub fn client_message(&self) -> String {
        if let Some(detail) = &self.detail {
            return detail.clone();
        }
        if let Some(source) = &self.source {
            return source.to_string();
        }
        self.message.clone()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.client_message())
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|e| e.as_ref() as &(dyn std::error::Error + 'static))
    }
}

impl Error {
    /// A fresh `400 bad request`.
    #[must_use]
    pub fn bad_request() -> Self {
        Self::new(code::BAD_REQUEST, "bad request")
    }

    /// A fresh `401 unauthorized`.
    #[must_use]
    pub fn unauthorized() -> Self {
        Self::new(code::UNAUTHORIZED, "unauthorized")
    }

    /// A fresh `403 forbidden`.
    #[must_use]
    pub fn forbidden() -> Self {
        Self::new(code::FORBIDDEN, "forbidden")
    }

    /// A fresh `404 not found`.
    #[must_use]
    pub fn not_found() -> Self {
        Self::new(code::NOT_FOUND, "not found")
    }

    /// A fresh `405 method not allowed`.
    #[must_use]
    pub fn method_not_allowed() -> Self {
        Self::new(code::METHOD_NOT_ALLOWED, "method not allowed")
    }

    /// A fresh `429 too many requests`.
    #[must_use]
    pub fn too_many_requests() -> Self {
        Self::new(code::TOO_MANY_REQUESTS, "too many requests")
    }

    /// A fresh `500 internal server error`.
    #[must_use]
    pub fn internal() -> Self {
        Self::new(code::INTERNAL_ERROR, "internal server error")
    }

    /// A fresh `503 service unavailable`.
    #[must_use]
    pub fn service_unavailable() -> Self {
        Self::new(code::SERVICE_UNAVAILABLE, "service unavailable")
    }

    /// A fresh `1001 validation failed` — a *business* code, sent over HTTP 200.
    #[must_use]
    pub fn validation_failed() -> Self {
        Self::new(code::VALIDATION_FAILED, "validation failed")
    }

    /// A fresh `1002 duplicate entry`.
    #[must_use]
    pub fn duplicate_entry() -> Self {
        Self::new(code::DUPLICATE_ENTRY, "duplicate entry")
    }

    /// A fresh `1003 token expired`.
    #[must_use]
    pub fn token_expired() -> Self {
        Self::new(code::TOKEN_EXPIRED, "token expired")
    }

    /// A fresh `1004 token invalid`.
    #[must_use]
    pub fn token_invalid() -> Self {
        Self::new(code::TOKEN_INVALID, "token invalid")
    }
}

// ---------------------------------------------------------------------------
// Conversions
//
// A blanket `impl<E: Error> From<E> for Error` is impossible in Rust (it would
// collide with the reflexive `From<T> for T`), so the handful of error types an
// application actually meets are listed explicitly.
// ---------------------------------------------------------------------------

impl From<&str> for Error {
    fn from(value: &str) -> Self {
        Error::new(code::INTERNAL_ERROR, value)
    }
}

impl From<String> for Error {
    fn from(value: String) -> Self {
        Error::new(code::INTERNAL_ERROR, value)
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        let code = match value.kind() {
            std::io::ErrorKind::NotFound => code::NOT_FOUND,
            std::io::ErrorKind::PermissionDenied => code::FORBIDDEN,
            _ => code::INTERNAL_ERROR,
        };
        Error::with_source(code, value)
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Error::with_source(code::BAD_REQUEST, value).detail("invalid JSON payload")
    }
}

impl From<sea_orm::DbErr> for Error {
    fn from(value: sea_orm::DbErr) -> Self {
        use sea_orm::DbErr;
        let code = match &value {
            DbErr::RecordNotInserted | DbErr::RecordNotUpdated => code::NOT_FOUND,
            _ => code::INTERNAL_ERROR,
        };
        Error::with_source(code, value)
    }
}

impl From<sea_orm::TransactionError<Error>> for Error {
    fn from(value: sea_orm::TransactionError<Error>) -> Self {
        match value {
            sea_orm::TransactionError::Connection(err) => err.into(),
            sea_orm::TransactionError::Transaction(err) => err,
        }
    }
}

impl From<multer::Error> for Error {
    fn from(value: multer::Error) -> Self {
        use multer::Error as MulterError;
        match &value {
            // The two limits a client can actually hit.
            MulterError::StreamSizeExceeded { limit } => Error::new(413, "request body too large")
                .detail(format!(
                    "the upload exceeds {limit} bytes — raise `web.server.maxBodySize` to accept it"
                )),
            MulterError::FieldSizeExceeded { limit, field_name } => {
                Error::new(413, "uploaded part too large").detail(match field_name {
                    Some(name) => format!("`{name}` exceeds {limit} bytes"),
                    None => format!("a part exceeds {limit} bytes"),
                })
            }
            MulterError::NoBoundary
            | MulterError::DecodeContentType(_)
            | MulterError::NoMultipart => Error::with_source(code::BAD_REQUEST, value)
                .detail("the request is not a well-formed multipart body"),
            _ => Error::with_source(code::BAD_REQUEST, value)
                .detail("the multipart body could not be read"),
        }
    }
}

impl From<tempfile::PersistError> for Error {
    fn from(value: tempfile::PersistError) -> Self {
        Error::with_source(code::INTERNAL_ERROR, value.error)
            .detail("could not write the upload to a temporary file")
    }
}

impl From<axum::http::Error> for Error {
    fn from(value: axum::http::Error) -> Self {
        Error::with_source(code::INTERNAL_ERROR, value)
    }
}

impl From<axum::Error> for Error {
    fn from(value: axum::Error) -> Self {
        Error::with_source(code::BAD_REQUEST, value)
    }
}

impl From<tokio::task::JoinError> for Error {
    fn from(value: tokio::task::JoinError) -> Self {
        Error::with_source(code::INTERNAL_ERROR, value)
    }
}

impl From<Box<dyn std::error::Error + Send + Sync>> for Error {
    fn from(value: Box<dyn std::error::Error + Send + Sync>) -> Self {
        Error::new(code::INTERNAL_ERROR, value.to_string())
    }
}

/// The framework's result alias. Everything fallible returns this.
///
/// The second parameter defaults to [`Error`], so both `Result<T>` and
/// `Result<T, SomeOtherError>` are valid.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_errors_carry_expected_codes() {
        assert_eq!(Error::not_found().http_status(), 404);
        assert_eq!(Error::forbidden().client_message(), "forbidden");
        assert_eq!(Error::internal().code, 500);
    }

    #[test]
    fn business_codes_travel_over_http_200() {
        assert_eq!(code::http_status(code::TOKEN_EXPIRED), 200);
        assert_eq!(code::http_status(404), 404);
    }

    #[test]
    fn detail_wins_over_message() {
        let err = Error::not_found().detail("user 7 does not exist");
        assert_eq!(err.client_message(), "user 7 does not exist");
        assert_eq!(err.http_status(), 404);
    }

    #[test]
    fn io_errors_map_to_sensible_codes() {
        let missing = std::io::Error::new(std::io::ErrorKind::NotFound, "nope");
        assert_eq!(Error::from(missing).code, 404);

        let denied = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no");
        assert_eq!(Error::from(denied).code, 403);
    }

    #[test]
    fn plain_strings_become_internal_errors() {
        let err: Error = "boom".into();
        assert_eq!(err.code, 500);
        assert_eq!(err.client_message(), "boom");
    }
}
