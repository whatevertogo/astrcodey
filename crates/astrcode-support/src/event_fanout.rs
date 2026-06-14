//! 泛型 fan-out 广播器。
//!
//! ## 背压与慢消费者策略
//! - 内部使用 bounded mpsc channel，避免慢消费者无限积压事件导致进程内存增长。
//! - `send()` 在接收端已关闭或队列已满时移除对应订阅者。
//!
//! ## 订阅者生命周期
//! - 订阅者 drop 后在**下一次** `send()` 时自动清理
//! - `subscriber_count()` 不精确（TOCTOU），仅供调试/监控，不可做业务逻辑判断
//!
//! ## 使用约束
//! - 仅用于 in-process 事件分发（订阅者 ≤ 两位数）
//! - 不用于远程/多进程场景
//! - 慢消费者会被移除；上层需要通过 snapshot/cursor 机制重连恢复。

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;
use tokio::sync::mpsc::{self, error::TrySendError};

use crate::sync::lock_parking;

/// Fan-out 运行指标（进程内累计，供监控/调试）。
#[derive(Default)]
pub struct EventFanoutStats {
    send_total: AtomicU64,
    dropped_subscribers: AtomicU64,
    dropped_subscribers_full: AtomicU64,
    dropped_subscribers_closed: AtomicU64,
    max_queue_depth: AtomicUsize,
}

/// [`EventFanoutStats`] 快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventFanoutStatsSnapshot {
    pub send_total: u64,
    pub dropped_subscribers: u64,
    /// 因 bounded 队列已满而移除的慢订阅者数。
    pub dropped_subscribers_full: u64,
    pub dropped_subscribers_closed: u64,
    /// 已观察到的单个订阅者最大排队深度。
    pub max_queue_depth: usize,
}

impl EventFanoutStats {
    pub fn snapshot(&self) -> EventFanoutStatsSnapshot {
        EventFanoutStatsSnapshot {
            send_total: self.send_total.load(Ordering::Relaxed),
            dropped_subscribers: self.dropped_subscribers.load(Ordering::Relaxed),
            dropped_subscribers_full: self.dropped_subscribers_full.load(Ordering::Relaxed),
            dropped_subscribers_closed: self.dropped_subscribers_closed.load(Ordering::Relaxed),
            max_queue_depth: self.max_queue_depth.load(Ordering::Relaxed),
        }
    }
}

pub struct EventFanout<T: Clone> {
    capacity: usize,
    senders: Mutex<Vec<mpsc::Sender<T>>>,
    stats: EventFanoutStats,
}

impl<T: Clone> EventFanout<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
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
        let mut dropped_full = 0u64;
        let mut dropped_closed = 0u64;
        senders.retain(|tx| match tx.try_send(event.clone()) {
            Ok(()) => {
                self.record_queue_depth(tx);
                true
            },
            Err(TrySendError::Full(_)) => {
                dropped_full += 1;
                self.record_queue_depth(tx);
                false
            },
            Err(TrySendError::Closed(_)) => {
                dropped_closed += 1;
                false
            },
        });
        let dropped = before - senders.len();
        if dropped > 0 {
            self.stats
                .dropped_subscribers
                .fetch_add(dropped as u64, Ordering::Relaxed);
            self.stats
                .dropped_subscribers_full
                .fetch_add(dropped_full, Ordering::Relaxed);
            self.stats
                .dropped_subscribers_closed
                .fetch_add(dropped_closed, Ordering::Relaxed);
            tracing::debug!(dropped, "removed closed event fanout subscribers");
        }
    }

    fn record_queue_depth(&self, tx: &mpsc::Sender<T>) {
        let depth = tx.max_capacity().saturating_sub(tx.capacity());
        let mut current = self.stats.max_queue_depth.load(Ordering::Relaxed);
        while depth > current {
            match self.stats.max_queue_depth.compare_exchange_weak(
                current,
                depth,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(next) => current = next,
            }
        }
    }

    pub fn subscribe(&self) -> mpsc::Receiver<T> {
        let (tx, rx) = mpsc::channel(self.capacity);
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
    async fn slow_consumer_is_dropped_when_buffer_fills() {
        let fanout = EventFanout::new(2);
        let _rx = fanout.subscribe();
        for i in 0..3 {
            fanout.send(i);
        }
        assert_eq!(fanout.subscriber_count(), 0);
        assert_eq!(fanout.stats().dropped_subscribers, 1);
        assert_eq!(fanout.stats().dropped_subscribers_full, 1);
        assert_eq!(fanout.stats().max_queue_depth, 2);
    }
}
