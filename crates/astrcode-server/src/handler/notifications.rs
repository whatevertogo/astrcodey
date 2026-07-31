//! 客户端通知辅助方法。

use astrcode_protocol::events::ClientNotification;

use super::CommandHandler;

impl CommandHandler {
    pub(super) fn send_error(&self, code: i32, message: &str) {
        self.event_bus.send_notification(ClientNotification::Error {
            code,
            message: message.into(),
        });
    }
}
