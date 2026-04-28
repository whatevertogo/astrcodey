//! Client error types.

use crate::transport::TransportError;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("Transport error: {0}")]
    Transport(#[from] TransportError),
    #[error("Server error: {0}")]
    Server(String),
    #[error("Unexpected response from server")]
    UnexpectedResponse,
    #[error("Auth expired")]
    AuthExpired,
    #[error("Auth denied")]
    AuthDenied,
    #[error("Session not found")]
    SessionNotFound,
    #[error("Stream disconnected")]
    StreamDisconnected,
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}
