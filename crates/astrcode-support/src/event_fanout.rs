//! 泛型 fan-out 广播器。替代 `tokio::sync::broadcast`，无容量限制。
//!
//! ## 订阅者生命周期
//! - 订阅者 drop 后在**下一次** `send()` 时自动清理
//! - `subscriber_count()` 不精确（TOCTOU），仅供调试/监控，不可做业务逻辑判断
//!
//! ## 使用约束
//! - 仅用于 in-process 事件分发（订阅者 ≤ 两位数）
//! - 不用于远程/多进程场景

use parking_lot::Mutex;
use tokio::sync::mpsc;

pub struct EventFanout<T: Clone> {
    senders: Mutex<Vec<mpsc::UnboundedSender<T>>>,
}

impl<T: Clone> EventFanout<T> {
    pub fn new() -> Self {
        Self {
            senders: Mutex::new(Vec::new()),
        }
    }

    /// 向所有订阅者广播。自动清理已关闭的接收端。
    ///
    /// `event` 会被 clone N-1 次（N = 订阅者数）。仅 in-process 场景使用。
    pub fn send(&self, event: T) {
        let mut senders = self.senders.lock();
        senders.retain(|tx| tx.send(event.clone()).is_ok());
    }

    /// 创建新订阅。
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<T> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.senders.lock().push(tx);
        rx
    }

    /// 当前订阅者数量。**不精确**（TOCTOU），仅供调试/监控。
    pub fn subscriber_count(&self) -> usize {
        self.senders.lock().len()
    }
}

impl<T: Clone> Default for EventFanout<T> {
    fn default() -> Self {
        Self::new()
    }
}
