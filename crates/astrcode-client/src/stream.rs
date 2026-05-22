//! 会话事件流封装。
//!
//! 直接包装 mpsc 接收端，提供异步接收与批量 drain 能力。

use astrcode_protocol::events::ClientNotification;
use tokio::sync::mpsc;

/// 服务端事件流的订阅接收器。
pub struct ConversationStream {
    rx: mpsc::UnboundedReceiver<ClientNotification>,
}

impl ConversationStream {
    /// 从 mpsc 接收端创建事件流。
    pub fn new(rx: mpsc::UnboundedReceiver<ClientNotification>) -> Self {
        Self { rx }
    }

    /// 异步接收下一条事件。
    ///
    /// - 返回 `Ok(ClientNotification)` 表示成功收到一条事件。
    /// - 返回 `Err(StreamError::Disconnected)` 表示事件流已关闭。
    pub async fn recv(&mut self) -> Result<ClientNotification, StreamError> {
        self.rx.recv().await.ok_or(StreamError::Disconnected)
    }

    /// 非阻塞地批量 drain 通道中已累积的所有事件。
    pub fn drain_pending(&mut self) -> Vec<ClientNotification> {
        let mut items = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            items.push(event);
        }
        items
    }
}

/// 事件流错误类型。
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// 事件流连接已断开，无法继续接收。
    #[error("Stream disconnected")]
    Disconnected,
}
