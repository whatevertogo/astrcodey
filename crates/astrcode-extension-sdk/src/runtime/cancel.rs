//! s5r 取消令牌。

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
pub struct CancelToken {
    cancelled: CancellationToken,
    reason: Arc<parking_lot::Mutex<Option<String>>>,
}

impl CancelToken {
    pub fn cancel(&self, reason: impl Into<String>) {
        *self.reason.lock() = Some(reason.into());
        self.cancelled.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.is_cancelled()
    }

    /// 返回与当前 s5r token 共享取消状态的 Tokio token。
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancelled.clone()
    }

    pub fn reason(&self) -> Option<String> {
        self.reason.lock().clone()
    }

    pub fn raise_if_cancelled(&self) -> Result<(), String> {
        if self.is_cancelled() {
            Err(self.reason().unwrap_or_else(|| "cancelled".into()))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_tokio_token_propagates_cancellation() {
        let token = CancelToken::default();
        let shared = token.cancellation_token();

        token.cancel("test_cancel");

        assert!(shared.is_cancelled());
        assert_eq!(token.reason().as_deref(), Some("test_cancel"));
    }
}
