use astrcode_core::wire::WireErrorCode;
use serde_json::Value;

use crate::s5r::ErrorPayload;

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
        match self.code_enum() {
            Some(WireErrorCode::PermissionDenied) => HostErrorClass::PermissionDenied,
            Some(WireErrorCode::BackendUnavailable) => HostErrorClass::BackendUnavailable,
            Some(WireErrorCode::ContextUnavailable) => HostErrorClass::ContextUnavailable,
            Some(WireErrorCode::InvalidInput) => HostErrorClass::InvalidInput,
            Some(WireErrorCode::Cancelled) => HostErrorClass::Cancelled,
            Some(WireErrorCode::Timeout) => HostErrorClass::Timeout,
            Some(
                WireErrorCode::HostNotReady
                | WireErrorCode::PeerBusy
                | WireErrorCode::PeerClosed
                | WireErrorCode::Transport,
            ) => HostErrorClass::Transport,
            _ => HostErrorClass::Other,
        }
    }

    pub fn code_enum(&self) -> Option<WireErrorCode> {
        WireErrorCode::parse(&self.code)
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
                WireErrorCode::PermissionDenied,
                HostErrorClass::PermissionDenied,
            ),
            (
                WireErrorCode::BackendUnavailable,
                HostErrorClass::BackendUnavailable,
            ),
            (
                WireErrorCode::ContextUnavailable,
                HostErrorClass::ContextUnavailable,
            ),
            (WireErrorCode::InvalidInput, HostErrorClass::InvalidInput),
            (WireErrorCode::Cancelled, HostErrorClass::Cancelled),
            (WireErrorCode::Timeout, HostErrorClass::Timeout),
            (WireErrorCode::HostNotReady, HostErrorClass::Transport),
            (WireErrorCode::PeerBusy, HostErrorClass::Transport),
            (WireErrorCode::PeerClosed, HostErrorClass::Transport),
            (WireErrorCode::Transport, HostErrorClass::Transport),
        ];

        for (code, expected) in cases {
            assert_eq!(HostError::new(code.as_str(), "failure").class(), expected);
        }
    }
}
