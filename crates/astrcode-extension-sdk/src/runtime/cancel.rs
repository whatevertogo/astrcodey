//! s5r 取消令牌。

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Default)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
    reason: Arc<parking_lot::Mutex<Option<String>>>,
}

impl CancelToken {
    pub fn cancel(&self, reason: impl Into<String>) {
        *self.reason.lock() = Some(reason.into());
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
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
