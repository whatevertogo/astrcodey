//! 泛型 fan-out 广播器。
//!
//! ## 背压与慢消费者策略
//! - 内部使用 bounded mpsc channel，默认容量 1024
//! - `send()` 中如果某个订阅者的 channel 已满（`TrySendError::Full`）或已关闭， 则断开该订阅者（从
//!   sender 列表中移除）
//! - 慢消费者被断开后，SSE 客户端可凭 cursor 重新 replay 恢复
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

use crate::sync::lock_parking;

/// 默认 channel 容量。生产环境中 LLM streaming ~1-2 events/sec，
/// 1024 条约缓冲 8-15 分钟，足够应对短暂拥塞。
const DEFAULT_CAPACITY: usize = 1024;

pub struct EventFanout<T: Clone> {
    senders: Mutex<Vec<mpsc::Sender<T>>>,
    capacity: usize,
}

impl<T: Clone> EventFanout<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            senders: Mutex::new(Vec::new()),
            capacity,
        }
    }

    /// 向所有订阅者广播。自动清理已关闭或过慢的接收端。
    ///
    /// `event` 会被 clone N-1 次（N = 订阅者数）。仅 in-process 场景使用。
    ///
    /// 慢消费者（channel 已满）会被断开并移除，统一以 debug 级别记录数量。
    pub fn send(&self, event: T) {
        let mut senders = lock_parking(&self.senders);
        let before = senders.len();
        if before == 0 {
            return;
        }
        // 单订阅者直接 move；多订阅者只额外 clone 一次作为后续广播副本。
        let backup = if before > 1 {
            Some(event.clone())
        } else {
            None
        };
        let mut owned = Some(event);
        senders.retain(|tx| {
            let payload = owned
                .take()
                .unwrap_or_else(|| backup.as_ref().expect("backup set when len > 1").clone());
            match tx.try_send(payload) {
                Ok(()) => true,
                Err(TrySendError::Full(e)) | Err(TrySendError::Closed(e)) => {
                    owned = Some(e);
                    false
                },
            }
        });
        let dropped = before - senders.len();
        if dropped > 0 {
            tracing::warn!(
                dropped,
                "removed slow or closed event fanout subscribers (backpressure)"
            );
        }
    }

    /// 创建新订阅。
    pub fn subscribe(&self) -> mpsc::Receiver<T> {
        let (tx, rx) = mpsc::channel(self.capacity);
        lock_parking(&self.senders).push(tx);
        rx
    }

    /// 当前订阅者数量。**不精确**（TOCTOU），仅供调试/监控。
    pub fn subscriber_count(&self) -> usize {
        lock_parking(&self.senders).len()
    }
}

impl<T: Clone> Default for EventFanout<T> {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

use mpsc::error::TrySendError;
