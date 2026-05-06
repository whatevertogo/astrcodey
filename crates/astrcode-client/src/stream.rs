//! 会话事件流封装。
//!
//! 提供对服务端事件流的异步订阅与接收能力。
//! 内部使用 forwarder bridge 将 broadcast 事件搬到 unbounded mpsc，
//! 确保即使 TUI 渲染慢也不会丢失任何事件。

use astrcode_protocol::events::ClientNotification;
use tokio::sync::{broadcast, mpsc};

/// 服务端事件流的订阅接收器。
///
/// 内部启动一个轻量 forwarder 任务，将 broadcast 事件搬到 unbounded mpsc。
/// forwarder 几乎不耗时，所以 broadcast 不会溢出；mpsc 无界，TUI 慢了只是积压不丢事件。
pub struct ConversationStream {
    rx: mpsc::UnboundedReceiver<ClientNotification>,
}

impl ConversationStream {
    /// 从广播接收端创建事件流，同时启动 forwarder 桥接。
    pub fn new(broadcast_rx: broadcast::Receiver<ClientNotification>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(forwarder(broadcast_rx, tx));
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

/// 将 broadcast 事件转发到 unbounded mpsc 的轻量任务。
async fn forwarder(
    mut broadcast_rx: broadcast::Receiver<ClientNotification>,
    tx: mpsc::UnboundedSender<ClientNotification>,
) {
    loop {
        match broadcast_rx.recv().await {
            Ok(event) => {
                if tx.send(event).is_err() {
                    break;
                }
            },
            // Lagged 说明有事件丢失，但 forwarder 继续运行
            Err(broadcast::error::RecvError::Lagged(_)) => {},
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// 事件流错误类型。
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// 事件流连接已断开，无法继续接收。
    #[error("Stream disconnected")]
    Disconnected,
}
