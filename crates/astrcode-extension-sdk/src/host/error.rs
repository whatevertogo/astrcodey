use serde_json::Value;

use crate::s5r::ErrorPayload;

pub const HOST_ERROR_CODE_PERMISSION_DENIED: &str = "permission_denied";
pub const HOST_ERROR_CODE_BACKEND_UNAVAILABLE: &str = "backend_unavailable";
pub const HOST_ERROR_CODE_CONTEXT_UNAVAILABLE: &str = "context_unavailable";
pub const HOST_ERROR_CODE_INVALID_INPUT: &str = "invalid_input";
pub const HOST_ERROR_CODE_CANCELLED: &str = "cancelled";
pub const HOST_ERROR_CODE_TIMEOUT: &str = "timeout";
pub const HOST_ERROR_CODE_HOST_NOT_READY: &str = "host_not_ready";
pub const HOST_ERROR_CODE_PEER_BUSY: &str = "peer_busy";
pub const HOST_ERROR_CODE_PEER_CLOSED: &str = "peer_closed";
pub const HOST_ERROR_CODE_TRANSPORT: &str = "transport_error";
pub const HOST_ERROR_CODE_IO_ERROR: &str = "io_error";
pub const HOST_ERROR_CODE_UNKNOWN_CAPABILITY: &str = "unknown_capability";
pub const HOST_ERROR_CODE_STATE_TOO_LARGE: &str = "state_too_large";
pub const HOST_ERROR_CODE_READ_FAILED: &str = "read_failed";
pub const HOST_ERROR_CODE_SESSION_NOT_FOUND: &str = "session_not_found";
pub const HOST_ERROR_CODE_FILE_TOO_LARGE: &str = "file_too_large";
pub const HOST_ERROR_CODE_INVALID_REQUEST: &str = "invalid_request";
pub const HOST_ERROR_CODE_PROCESS_FAILED: &str = "process_failed";
pub const HOST_ERROR_CODE_SPAWN_FAILED: &str = "spawn_failed";
pub const HOST_ERROR_CODE_STDIN_FAILED: &str = "stdin_failed";
pub const HOST_ERROR_CODE_STDOUT_FAILED: &str = "stdout_failed";
pub const HOST_ERROR_CODE_STDERR_FAILED: &str = "stderr_failed";
pub const HOST_ERROR_CODE_HOST_RUNTIME_FAILED: &str = "host_runtime_failed";
pub const HOST_ERROR_CODE_INVALID_MANIFEST: &str = "invalid_manifest";
pub const HOST_ERROR_CODE_NOT_INITIALIZED: &str = "not_initialized";
pub const HOST_ERROR_CODE_EMIT_FAILED: &str = "emit_failed";
pub const HOST_ERROR_CODE_DISPATCH_FAILED: &str = "dispatch_failed";
pub const HOST_ERROR_CODE_UNKNOWN_PARENT_INVOKE: &str = "unknown_parent_invoke";
pub const HOST_ERROR_CODE_REENTRANCY_EXCEEDED: &str = "reentrancy_exceeded";
pub const HOST_ERROR_CODE_UNSUPPORTED_PROTOCOL_VERSION: &str = "unsupported_protocol_version";
pub const HOST_ERROR_CODE_UNSUPPORTED: &str = "unsupported";
pub const HOST_ERROR_CODE_SERIALIZATION_FAILED: &str = "serialization_failed";
pub const HOST_ERROR_CODE_INVALID_RESPONSE: &str = "invalid_host_response";

/// Stable high-level classification for common host failures.
///
/// The original wire `code` remains available on [`HostError`], so unknown and future
/// error codes are never collapsed into this classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostErrorClass {
    PermissionDenied,
    BackendUnavailable,
    ContextUnavailable,
    InvalidInput,
    Cancelled,
    Timeout,
    Transport,
    Other,
}

/// Lossless author-facing representation of an S5R host error payload.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct HostError {
    pub code: String,
    pub message: String,
    pub hint: Option<String>,
    pub retryable: bool,
    pub details: Option<Value>,
}

impl HostError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            hint: None,
            retryable: false,
            details: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub const fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn class(&self) -> HostErrorClass {
        match self.code.as_str() {
            HOST_ERROR_CODE_PERMISSION_DENIED => HostErrorClass::PermissionDenied,
            HOST_ERROR_CODE_BACKEND_UNAVAILABLE => HostErrorClass::BackendUnavailable,
            HOST_ERROR_CODE_CONTEXT_UNAVAILABLE => HostErrorClass::ContextUnavailable,
            HOST_ERROR_CODE_INVALID_INPUT => HostErrorClass::InvalidInput,
            HOST_ERROR_CODE_CANCELLED => HostErrorClass::Cancelled,
            HOST_ERROR_CODE_TIMEOUT => HostErrorClass::Timeout,
            HOST_ERROR_CODE_HOST_NOT_READY
            | HOST_ERROR_CODE_PEER_BUSY
            | HOST_ERROR_CODE_PEER_CLOSED
            | HOST_ERROR_CODE_TRANSPORT => HostErrorClass::Transport,
            _ => HostErrorClass::Other,
        }
    }

    pub fn is_permission_denied(&self) -> bool {
        self.class() == HostErrorClass::PermissionDenied
    }

    pub fn is_backend_unavailable(&self) -> bool {
        self.class() == HostErrorClass::BackendUnavailable
    }

    pub fn is_context_unavailable(&self) -> bool {
        self.class() == HostErrorClass::ContextUnavailable
    }
}

impl From<ErrorPayload> for HostError {
    fn from(payload: ErrorPayload) -> Self {
        Self {
            code: payload.code,
            message: payload.message,
            hint: payload.hint,
            retryable: payload.retryable,
            details: payload.details,
        }
    }
}

impl From<HostError> for ErrorPayload {
    fn from(error: HostError) -> Self {
        Self {
            code: error.code,
            message: error.message,
            hint: error.hint,
            retryable: error.retryable,
            details: error.details,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn error_payload_round_trip_is_lossless_for_unknown_codes() {
        let payload = ErrorPayload {
            code: "future_host_failure".into(),
            message: "future failure".into(),
            hint: Some("upgrade the extension".into()),
            retryable: true,
            details: Some(json!({ "revision": 3 })),
        };

        let error = HostError::from(payload.clone());
        assert_eq!(error.class(), HostErrorClass::Other);
        let round_trip = ErrorPayload::from(error);
        assert_eq!(round_trip.code, payload.code);
        assert_eq!(round_trip.message, payload.message);
        assert_eq!(round_trip.hint, payload.hint);
        assert_eq!(round_trip.retryable, payload.retryable);
        assert_eq!(round_trip.details, payload.details);
    }

    #[test]
    fn common_boundary_failures_have_stable_classifications() {
        let cases = [
            (
                HOST_ERROR_CODE_PERMISSION_DENIED,
                HostErrorClass::PermissionDenied,
            ),
            (
                HOST_ERROR_CODE_BACKEND_UNAVAILABLE,
                HostErrorClass::BackendUnavailable,
            ),
            (
                HOST_ERROR_CODE_CONTEXT_UNAVAILABLE,
                HostErrorClass::ContextUnavailable,
            ),
            (HOST_ERROR_CODE_INVALID_INPUT, HostErrorClass::InvalidInput),
            (HOST_ERROR_CODE_CANCELLED, HostErrorClass::Cancelled),
            (HOST_ERROR_CODE_TIMEOUT, HostErrorClass::Timeout),
            (HOST_ERROR_CODE_HOST_NOT_READY, HostErrorClass::Transport),
            (HOST_ERROR_CODE_PEER_BUSY, HostErrorClass::Transport),
            (HOST_ERROR_CODE_PEER_CLOSED, HostErrorClass::Transport),
            (HOST_ERROR_CODE_TRANSPORT, HostErrorClass::Transport),
        ];

        for (code, expected) in cases {
            assert_eq!(HostError::new(code, "failure").class(), expected);
        }
    }
}
