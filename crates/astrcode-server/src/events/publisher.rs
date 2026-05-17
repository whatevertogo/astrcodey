//! 面向客户端的事件发布器。

use astrcode_protocol::events::ClientNotification;
use tokio::sync::broadcast;

/// 只负责把已成立的通知发布给客户端。
pub struct ClientEventPublisher {
    tx: broadcast::Sender<ClientNotification>,
}

impl ClientEventPublisher {
    pub fn new(tx: broadcast::Sender<ClientNotification>) -> Self {
        Self { tx }
    }

    pub fn sender(&self) -> &broadcast::Sender<ClientNotification> {
        &self.tx
    }

    pub fn publish(&self, notification: ClientNotification) {
        let _ = self.tx.send(notification);
    }
}
