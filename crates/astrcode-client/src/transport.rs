//! 客户端传输层抽象。
//!
//! - [`InProcessTransport`]（在 `astrcode-cli` 中实现）：TUI / exec 进程内零成本启动
//! - 外部客户端应连接 HTTP/SSE API（`astrcode-server` / `astrcode-http-server`）

use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};
use tokio::sync::mpsc;

/// 客户端与服务端之间的传输层接口（进程内路径）。
#[async_trait::async_trait]
pub trait ClientTransport: Send + Sync {
    /// 发送命令但不等待响应；需配合 [`Self::subscribe`] 接收通知。
    async fn send(&self, command: &ClientCommand) -> Result<(), TransportError>;

    /// 发送命令并等待第一个响应事件。
    async fn execute(&self, command: &ClientCommand) -> Result<ClientNotification, TransportError> {
        let mut rx = self.subscribe().await?;
        self.send(command).await?;
        match rx.recv().await {
            Some(event) => Ok(event),
            None => Err(TransportError::StreamDisconnected),
        }
    }

    /// 订阅服务端事件流。
    async fn subscribe(&self) -> Result<mpsc::Receiver<ClientNotification>, TransportError>;
}

pub use astrcode_protocol::transport::TransportError;
