//! 泛型 fan-out 广播器。
//!
//! ## 背压与慢消费者策略
//! - 内部使用 **unbounded** mpsc channel，避免 live delta 等高频事件填满 bounded buffer 后把 SSE
//!   订阅者踢掉（live-only 事件无法 replay，丢订阅 = UI 假死）
//! - `send()` 仅在接收端已关闭时移除对应订阅者
//!
//! ## 订阅者生命周期
//! - 订阅者 drop 后在**下一次** `send()` 时自动清理
//! - `subscriber_count()` 不精确（TOCTOU），仅供调试/监控，不可做业务逻辑判断
//!
//! ## 使用约束
//! - 仅用于 in-process 事件分发（订阅者 ≤ 两位数）
//! - 不用于远程/多进程场景
//! - 内存由消费者速度约束；慢消费者会导致内存增长而非静默丢事件

use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use tokio::sync::mpsc;

use crate::sync::lock_parking;

/// Fan-out 运行指标（进程内累计，供监控/调试）。
#[derive(Default)]
pub struct EventFanoutStats {
    send_total: AtomicU64,
    dropped_subscribers: AtomicU64,
    dropped_subscribers_closed: AtomicU64,
}

/// [`EventFanoutStats`] 快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventFanoutStatsSnapshot {
    pub send_total: u64,
    pub dropped_subscribers: u64,
    /// unbounded 模式下恒为 0（保留字段便于监控面板兼容）。
    pub dropped_subscribers_full: u64,
    pub dropped_subscribers_closed: u64,
    /// unbounded 模式下无队列深度；恒为 0。
    pub max_queue_depth: usize,
}

impl EventFanoutStats {
    pub fn snapshot(&self) -> EventFanoutStatsSnapshot {
        EventFanoutStatsSnapshot {
            send_total: self.send_total.load(Ordering::Relaxed),
            dropped_subscribers: self.dropped_subscribers.load(Ordering::Relaxed),
            dropped_subscribers_full: 0,
            dropped_subscribers_closed: self.dropped_subscribers_closed.load(Ordering::Relaxed),
            max_queue_depth: 0,
        }
    }
}

pub struct EventFanout<T: Clone> {
    senders: Mutex<Vec<mpsc::UnboundedSender<T>>>,
    stats: EventFanoutStats,
}

impl<T: Clone> EventFanout<T> {
    /// `capacity` 保留以兼容既有调用点，当前实现使用 unbounded channel。
    pub fn new(_capacity: usize) -> Self {
        Self {
            senders: Mutex::new(Vec::new()),
            stats: EventFanoutStats::default(),
        }
    }

    pub fn stats(&self) -> EventFanoutStatsSnapshot {
        self.stats.snapshot()
    }

    /// 向所有订阅者广播。自动清理已关闭的接收端。
    pub fn send(&self, event: T) {
        self.stats.send_total.fetch_add(1, Ordering::Relaxed);
        let mut senders = lock_parking(&self.senders);
        let before = senders.len();
        if before == 0 {
            return;
        }
        let backup = if before > 1 {
            Some(event.clone())
        } else {
            None
        };
        let mut owned = Some(event);
        let mut dropped_closed = 0u64;
        senders.retain(|tx| {
            let payload = owned
                .take()
                .unwrap_or_else(|| backup.as_ref().expect("backup set when len > 1").clone());
            match tx.send(payload) {
                Ok(()) => true,
                Err(err) => {
                    owned = Some(err.0);
                    dropped_closed += 1;
                    false
                },
            }
        });
        let dropped = before - senders.len();
        if dropped > 0 {
            self.stats
                .dropped_subscribers
                .fetch_add(dropped as u64, Ordering::Relaxed);
            self.stats
                .dropped_subscribers_closed
                .fetch_add(dropped_closed, Ordering::Relaxed);
            tracing::debug!(dropped, "removed closed event fanout subscribers");
        }
    }

    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<T> {
        let (tx, rx) = mpsc::unbounded_channel();
        lock_parking(&self.senders).push(tx);
        rx
    }

    pub fn subscriber_count(&self) -> usize {
        lock_parking(&self.senders).len()
    }
}

impl<T: Clone> Default for EventFanout<T> {
    fn default() -> Self {
        Self::new(1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn slow_consumer_does_not_get_dropped_under_burst() {
        let fanout = EventFanout::new(1024);
        let mut rx = fanout.subscribe();
        for i in 0..5000 {
            fanout.send(i);
        }
        assert_eq!(fanout.subscriber_count(), 1);
        assert_eq!(fanout.stats().dropped_subscribers, 0);
        assert_eq!(rx.recv().await, Some(0));
    }
}
