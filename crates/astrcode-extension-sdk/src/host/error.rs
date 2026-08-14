use serde_json::Value;

use crate::wire::{WireErrorCode, protocol::ErrorPayload};

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
    pub fn new(code: WireErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.as_str().into(),
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

    pub fn code_enum(&self) -> Option<WireErrorCode> {
        WireErrorCode::parse(&self.code)
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
        assert_eq!(error.code_enum(), None);
        let round_trip = ErrorPayload::from(error);
        assert_eq!(round_trip.code, payload.code);
        assert_eq!(round_trip.message, payload.message);
        assert_eq!(round_trip.hint, payload.hint);
        assert_eq!(round_trip.retryable, payload.retryable);
        assert_eq!(round_trip.details, payload.details);
    }
}
