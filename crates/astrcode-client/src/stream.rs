//! 会话事件流封装。
//!
//! 直接包装 broadcast 接收端，提供异步接收与批量 drain 能力。

use astrcode_protocol::events::ClientNotification;
use tokio::sync::broadcast;

/// 服务端事件流的订阅接收器。
pub struct ConversationStream {
    rx: broadcast::Receiver<ClientNotification>,
    disconnected: bool,
}

impl ConversationStream {
    /// 从 broadcast 接收端创建事件流。
    pub fn new(rx: broadcast::Receiver<ClientNotification>) -> Self {
        Self {
            rx,
            disconnected: false,
        }
    }

    /// 异步接收下一条事件。
    ///
    /// - 返回 `Ok(ClientNotification)` 表示成功收到一条事件。
    /// - 返回 `Err(StreamError::Disconnected)` 表示事件流已关闭。
    pub async fn recv(&mut self) -> Result<ClientNotification, StreamError> {
        if self.disconnected {
            return Err(StreamError::Disconnected);
        }
        match self.rx.recv().await {
            Ok(notification) => Ok(notification),
            Err(_) => {
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
                Err(
                    broadcast::error::TryRecvError::Closed
                    | broadcast::error::TryRecvError::Lagged(_),
                ) => {
                    self.disconnected = true;
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
    async fn conversation_stream_lag_disconnects_during_drain() {
        let (tx, rx) = broadcast::channel::<ClientNotification>(1);
        let mut stream = ConversationStream::new(rx);

        tx.send(ClientNotification::ExtensionRegistryChanged)
            .unwrap();
        tx.send(ClientNotification::ExtensionRegistryChanged)
            .unwrap();

        assert!(stream.drain_pending().is_empty());
        assert!(matches!(
            stream.recv().await,
            Err(StreamError::Disconnected)
        ));
    }
}
