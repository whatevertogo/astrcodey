//! 会话事件流封装。
//!
//! 提供对服务端事件流的异步订阅与接收能力，处理事件丢失（lagged）
//! 和连接断开等异常情况。

use astrcode_protocol::events::ClientNotification;
use tokio::sync::broadcast;

/// 服务端事件流的订阅接收器。
///
/// 包装了 `broadcast::Receiver`，提供异步逐条接收事件的能力。
pub struct ConversationStream {
    /// 广播通道的接收端。
    rx: broadcast::Receiver<ClientNotification>,
}

impl ConversationStream {
    /// 从已有的广播接收端创建事件流。
    pub fn new(rx: broadcast::Receiver<ClientNotification>) -> Self {
        Self { rx }
    }

    /// 异步接收下一条事件。
    ///
    /// - 返回 `Ok(StreamItem::Event)` 表示成功收到一条事件。
    /// - 返回 `Ok(StreamItem::Lagged(n))` 表示消费者处理速度落后，跳过了 `n` 条事件。
    /// - 返回 `Err(StreamError::Disconnected)` 表示事件流已关闭。
    pub async fn recv(&mut self) -> Result<StreamItem, StreamError> {
        match self.rx.recv().await {
            Ok(event) => Ok(StreamItem::Event(event)),
            Err(broadcast::error::RecvError::Lagged(n)) => Ok(StreamItem::Lagged(n)),
            Err(broadcast::error::RecvError::Closed) => Err(StreamError::Disconnected),
        }
    }
}

/// 从事件流中接收到的条目类型。
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum StreamItem {
    /// 正常的服务端事件通知。
    Event(ClientNotification),
    /// 消费者处理速度落后，`n` 条事件被跳过。
    Lagged(u64),
}

/// 事件流错误类型。
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// 事件流连接已断开，无法继续接收。
    #[error("Stream disconnected")]
    Disconnected,
}
