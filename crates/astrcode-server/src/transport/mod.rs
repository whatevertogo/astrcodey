//! 传输层抽象。
//!
//! [`ServerTransport`] trait 定义了服务器与客户端之间的通信接口，
//! 允许多种传输实现（stdio、WebSocket 等）。

mod stdio;

use astrcode_protocol::commands::ClientCommand;
pub use astrcode_protocol::transport::TransportError;
pub use stdio::{StdioTransport, write_error_response, write_initialize_response};

/// Transport trait for server communication.
///
/// Allows future expansion to WebSocket, TCP, etc.
#[async_trait::async_trait]
pub trait ServerTransport: Send + Sync {
    /// Read the next command from the transport.
    async fn read_command(&mut self) -> Option<ClientCommand>;

    /// Initialize the connection with version negotiation.
    async fn initialize(
        &mut self,
    ) -> Result<astrcode_protocol::version::InitializeRequest, TransportError>;
}
