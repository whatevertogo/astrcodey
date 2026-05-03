//! 传输层抽象。
//!
//! [`ServerTransport`] trait 定义了服务器与客户端之间的通信接口，
//! 允许多种传输实现（stdio、WebSocket 等）。

mod stdio;

use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};
pub use stdio::{StdioTransport, write_initialize_response};

/// Transport trait for server communication.
///
/// Allows future expansion to WebSocket, TCP, etc.
#[async_trait::async_trait]
pub trait ServerTransport: Send + Sync {
    /// Read the next command from the transport.
    async fn read_command(&mut self) -> Option<ClientCommand>;

    /// Write an event to the transport.
    async fn write_event(&self, event: &ClientNotification) -> Result<(), TransportError>;

    /// Initialize the connection with version negotiation.
    async fn initialize(
        &mut self,
    ) -> Result<astrcode_protocol::version::InitializeRequest, TransportError>;
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Connection closed")]
    Disconnected,
    #[error("Unsupported protocol version: {0}")]
    UnsupportedVersion(u32),
}
