//! 会话事件流封装。
//!
//! 直接包装 broadcast 接收端，提供异步接收与批量 drain 能力。

use astrcode_protocol::events::ClientNotification;
use tokio::sync::broadcast;

/// 服务端事件流的订阅接收器。
pub struct ConversationStream {
    rx: broadcast::Receiver<ClientNotification>,
    disconnected: bool,
    lagged: Option<u64>,
}

impl ConversationStream {
    /// 从 broadcast 接收端创建事件流。
    pub fn new(rx: broadcast::Receiver<ClientNotification>) -> Self {
        Self {
            rx,
            disconnected: false,
            lagged: None,
        }
    }

    /// 异步接收下一条事件。
    ///
    /// - 返回 `Ok(ClientNotification)` 表示成功收到一条事件。
    /// - 返回 `Err(StreamError::Disconnected)` 表示事件流已关闭。
    /// - 返回 `Err(StreamError::Lagged)` 表示事件流不再完整，调用方应重新同步状态。
    pub async fn recv(&mut self) -> Result<ClientNotification, StreamError> {
        if let Some(skipped) = self.lagged.take() {
            return Err(StreamError::Lagged { skipped });
        }
        if self.disconnected {
            return Err(StreamError::Disconnected);
        }
        match self.rx.recv().await {
            Ok(notification) => Ok(notification),
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                Err(StreamError::Lagged { skipped })
            },
            Err(broadcast::error::RecvError::Closed) => {
                self.disconnected = true;
                Err(StreamError::Disconnected)
            },
        }
    }

    /// 非阻塞地批量 drain 通道中已累积的所有事件。
    pub fn drain_pending(&mut self) -> Vec<ClientNotification> {
        let mut items = Vec::new();
        if self.disconnected {
            return items;
        }
        loop {
            match self.rx.try_recv() {
                Ok(event) => items.push(event),
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Closed) => {
                    self.disconnected = true;
                    break;
                },
                Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                    self.lagged = Some(skipped);
                    break;
                },
            }
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
    /// 消费者落后于广播缓冲区，部分通知已丢失。
    #[error("Stream lagged and skipped {skipped} notifications")]
    Lagged { skipped: u64 },
}

#[cfg(test)]
mod tests {
    use astrcode_core::types::SessionId;

    use super::*;

    #[tokio::test]
    async fn conversation_stream_recv_returns_events() {
        let (tx, rx) = broadcast::channel::<ClientNotification>(1);
        let mut stream = ConversationStream::new(rx);

        let event = astrcode_core::event::Event::from(astrcode_core::event::StoredEvent::new(
            1,
            astrcode_core::event::DurableEvent::session(
                astrcode_core::types::SessionId::new("s1"),
                astrcode_core::event::DurableEventPayload::TurnStarted,
            ),
        ));
        tx.send(ClientNotification::Event(event.clone())).unwrap();

        let received = stream.recv().await.unwrap();
        match received {
            ClientNotification::Event(e) => assert_eq!(e.session_id, SessionId::new("s1")),
            _ => panic!("expected Event notification"),
        }
    }

    #[tokio::test]
    async fn conversation_stream_recv_returns_disconnected() {
        let (tx, rx) = broadcast::channel::<ClientNotification>(1);
        drop(tx);
        let mut stream = ConversationStream::new(rx);
        // tx is dropped immediately, so recv should return Disconnected
        let err = stream.recv().await.unwrap_err();
        assert!(matches!(err, StreamError::Disconnected));
    }

    #[tokio::test]
    async fn conversation_stream_drain_pending_collects_buffered() {
        let (tx, rx) = broadcast::channel::<ClientNotification>(2);
        let mut stream = ConversationStream::new(rx);

        let notification = ClientNotification::ExtensionRegistryChanged;
        tx.send(notification.clone()).unwrap();
        tx.send(notification.clone()).unwrap();
        drop(tx); // close so drain stops

        let items = stream.drain_pending();
        assert_eq!(items.len(), 2);
        assert!(matches!(
            stream.recv().await,
            Err(StreamError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn conversation_stream_reports_lag_after_drain() {
        let (tx, rx) = broadcast::channel::<ClientNotification>(1);
        let mut stream = ConversationStream::new(rx);

        tx.send(ClientNotification::ExtensionRegistryChanged)
            .unwrap();
        tx.send(ClientNotification::ExtensionRegistryChanged)
            .unwrap();

        assert!(stream.drain_pending().is_empty());
        assert!(matches!(
            stream.recv().await,
            Err(StreamError::Lagged { skipped: 1 })
        ));
    }

    #[tokio::test]
    async fn conversation_stream_recv_reports_lag() {
        let (tx, rx) = broadcast::channel::<ClientNotification>(1);
        let mut stream = ConversationStream::new(rx);

        tx.send(ClientNotification::ExtensionRegistryChanged)
            .unwrap();
        tx.send(ClientNotification::ExtensionRegistryChanged)
            .unwrap();

        assert!(matches!(
            stream.recv().await,
            Err(StreamError::Lagged { skipped: 1 })
        ));
    }
}
