use serde_json::Value;
use thiserror::Error;

use crate::transport::TransportError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientErrorKind {
    AuthExpired,
    PermissionDenied,
    Validation,
    NotFound,
    Conflict,
    CursorExpired,
    StreamDisconnected,
    TransportUnavailable,
    UnexpectedResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct ClientError {
    pub kind: ClientErrorKind,
    pub message: String,
    pub status_code: Option<u16>,
    pub details: Option<Value>,
}

impl ClientError {
    pub fn new(kind: ClientErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status_code: None,
            details: None,
        }
    }

    pub(crate) fn with_status(mut self, status_code: u16) -> Self {
        self.status_code = Some(status_code);
        self
    }

    pub(crate) fn with_details(mut self, details: Option<Value>) -> Self {
        self.details = details;
        self
    }

    pub(crate) fn from_transport(error: TransportError) -> Self {
        match error {
            TransportError::Http { status, body } => from_http_error(status, &body),
            TransportError::Network { message } => {
                ClientError::new(ClientErrorKind::TransportUnavailable, message)
            },
            TransportError::StreamDisconnected { message } => {
                ClientError::new(ClientErrorKind::StreamDisconnected, message)
            },
            TransportError::UnexpectedResponse { message } => {
                ClientError::new(ClientErrorKind::UnexpectedResponse, message)
            },
        }
    }
}

fn from_http_error(status: u16, body: &str) -> ClientError {
    let parsed = serde_json::from_str::<Value>(body).ok();
    let message = parsed
        .as_ref()
        .and_then(|value| value.get("message").and_then(Value::as_str))
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|value| value.get("error").and_then(Value::as_str))
        })
        .unwrap_or_else(|| {
            if body.trim().is_empty() {
                "request failed"
            } else {
                body
            }
        })
        .to_string();

    let details = parsed
        .as_ref()
        .and_then(|value| value.get("details").cloned());
    let code = parsed
        .as_ref()
        .and_then(|value| value.get("code").and_then(Value::as_str));

    let kind = match (status, code) {
        (401, _) => ClientErrorKind::AuthExpired,
        (403, _) => ClientErrorKind::PermissionDenied,
        (404, _) => ClientErrorKind::NotFound,
        (409, Some("cursor_expired")) => ClientErrorKind::CursorExpired,
        (409, _) => ClientErrorKind::Conflict,
        (400, Some("cursor_expired")) => ClientErrorKind::CursorExpired,
        (400, _) => ClientErrorKind::Validation,
        _ => ClientErrorKind::UnexpectedResponse,
    };

    ClientError::new(kind, message)
        .with_status(status)
        .with_details(details)
}
