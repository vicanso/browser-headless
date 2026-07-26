//! Transport-neutral capture errors.
//!
//! Used by [`crate::capture`] end-to-end so the engine never depends on axum
//! (or any HTTP framework). The HTTP layer maps these to
//! `axum::http::StatusCode`; the worker writes the numeric `status` into
//! Redis job results.

use std::fmt;

/// Failure of a single capture (or of pre-flight validation before the
/// browser is touched). The `message` is safe to return to callers.
#[derive(Debug, Clone)]
pub(crate) struct CaptureError {
    pub(crate) kind: CaptureErrorKind,
    pub(crate) message: String,
}

/// Stable error categories that map 1:1 onto HTTP status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureErrorKind {
    BadRequest,
    Unauthorized,
    Forbidden,
    NotFound,
    /// Soft page-wait timeout (`timeout_ms` elapsed on a selector / function).
    Timeout,
    /// Hard total deadline exceeded.
    GatewayTimeout,
    /// Upstream page returned a failing status that we surface as 502.
    BadGateway,
    /// Pool saturated / unavailable.
    ServiceUnavailable,
    /// Rate limit exceeded.
    TooManyRequests,
    Internal,
}

impl CaptureError {
    pub(crate) fn new(kind: CaptureErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self::new(CaptureErrorKind::BadRequest, message)
    }

    pub(crate) fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(CaptureErrorKind::Unauthorized, message)
    }

    pub(crate) fn forbidden(message: impl Into<String>) -> Self {
        Self::new(CaptureErrorKind::Forbidden, message)
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(CaptureErrorKind::NotFound, message)
    }

    pub(crate) fn timeout(message: impl Into<String>) -> Self {
        Self::new(CaptureErrorKind::Timeout, message)
    }

    pub(crate) fn gateway_timeout(message: impl Into<String>) -> Self {
        Self::new(CaptureErrorKind::GatewayTimeout, message)
    }

    pub(crate) fn bad_gateway(message: impl Into<String>) -> Self {
        Self::new(CaptureErrorKind::BadGateway, message)
    }

    pub(crate) fn service_unavailable(message: impl Into<String>) -> Self {
        Self::new(CaptureErrorKind::ServiceUnavailable, message)
    }

    pub(crate) fn too_many_requests(message: impl Into<String>) -> Self {
        Self::new(CaptureErrorKind::TooManyRequests, message)
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(CaptureErrorKind::Internal, message)
    }

    /// HTTP status code this error should surface as.
    pub(crate) fn status_u16(&self) -> u16 {
        match self.kind {
            CaptureErrorKind::BadRequest => 400,
            CaptureErrorKind::Unauthorized => 401,
            CaptureErrorKind::Forbidden => 403,
            CaptureErrorKind::NotFound => 404,
            CaptureErrorKind::Timeout => 408,
            CaptureErrorKind::TooManyRequests => 429,
            CaptureErrorKind::GatewayTimeout => 504,
            CaptureErrorKind::BadGateway => 502,
            CaptureErrorKind::ServiceUnavailable => 503,
            CaptureErrorKind::Internal => 500,
        }
    }
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CaptureError {}

/// Map a browser-layer error onto a transport-neutral [`CaptureError`].
pub(crate) fn from_browser(e: crate::browser::Error) -> CaptureError {
    use crate::browser::Error;
    match &e {
        Error::NotFound(_) => CaptureError::not_found(e.to_string()),
        Error::Timeout(_) => CaptureError::timeout(e.to_string()),
        Error::UpstreamFailure { .. } => CaptureError::bad_gateway(e.to_string()),
        Error::InvalidInput(_) => CaptureError::bad_request(e.to_string()),
        Error::Cdp(_) => CaptureError::internal(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping() {
        assert_eq!(CaptureError::bad_request("x").status_u16(), 400);
        assert_eq!(CaptureError::unauthorized("x").status_u16(), 401);
        assert_eq!(CaptureError::forbidden("x").status_u16(), 403);
        assert_eq!(CaptureError::not_found("x").status_u16(), 404);
        assert_eq!(CaptureError::timeout("x").status_u16(), 408);
        assert_eq!(CaptureError::bad_gateway("x").status_u16(), 502);
        assert_eq!(CaptureError::service_unavailable("x").status_u16(), 503);
        assert_eq!(CaptureError::too_many_requests("x").status_u16(), 429);
        assert_eq!(CaptureError::gateway_timeout("x").status_u16(), 504);
        assert_eq!(CaptureError::internal("x").status_u16(), 500);
    }
}
