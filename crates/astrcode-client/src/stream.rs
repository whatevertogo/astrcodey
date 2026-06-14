//! 会话事件流封装。
//!
//! 直接包装 mpsc 接收端，提供异步接收与批量 drain 能力。

use astrcode_protocol::events::ClientNotification;
use tokio::sync::mpsc;

/// 服务端事件流的订阅接收器。
pub struct ConversationStream {
    rx: mpsc::Receiver<ClientNotification>,
}

impl ConversationStream {
    /// 从 mpsc 接收端创建事件流。
    pub fn new(rx: mpsc::Receiver<ClientNotification>) -> Self {
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

#[cfg(test)]
mod tests {
    use astrcode_core::types::SessionId;

    use super::*;

    #[tokio::test]
    async fn conversation_stream_recv_returns_events() {
        let (tx, rx) = mpsc::channel::<ClientNotification>(1);
        let mut stream = ConversationStream::new(rx);

        let event = astrcode_core::event::Event::new(
            astrcode_core::types::SessionId::new("s1"),
            None,
            astrcode_core::event::EventPayload::TurnStarted,
        );
        tx.send(ClientNotification::Event(event.clone()))
            .await
            .unwrap();

        let received = stream.recv().await.unwrap();
        match received {
            ClientNotification::Event(e) => assert_eq!(e.session_id, SessionId::new("s1")),
            _ => panic!("expected Event notification"),
        }
    }

    #[tokio::test]
    async fn conversation_stream_recv_returns_disconnected() {
        let (_, rx) = mpsc::channel::<ClientNotification>(1);
        let mut stream = ConversationStream::new(rx);
        // tx is dropped immediately, so recv should return Disconnected
        let err = stream.recv().await.unwrap_err();
        assert!(matches!(err, StreamError::Disconnected));
    }

    #[tokio::test]
    async fn conversation_stream_drain_pending_collects_buffered() {
        let (tx, rx) = mpsc::channel::<ClientNotification>(2);
        let mut stream = ConversationStream::new(rx);

        let notification = ClientNotification::ExtensionRegistryChanged;
        tx.send(notification.clone()).await.unwrap();
        tx.send(notification.clone()).await.unwrap();
        drop(tx); // close so drain stops

        let items = stream.drain_pending();
        assert_eq!(items.len(), 2);
    }
}
