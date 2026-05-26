//! 客户端通知辅助方法。

use astrcode_core::event::Event;
use astrcode_protocol::events::ClientNotification;

use super::CommandHandler;

impl CommandHandler {
    pub(super) fn broadcast_event(&self, event: Event) {
        self.event_bus
            .send_notification(ClientNotification::Event(event));
    }

    pub(super) fn send_error(&self, code: i32, message: &str) {
        self.event_bus.send_notification(ClientNotification::Error {
            code,
            message: message.into(),
        });
    }
}
