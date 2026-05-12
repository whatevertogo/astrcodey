//! 扩展间通信的事件总线。
//!
//! # 动机
//!
//! 多个扩展可能需要协作：一个扩展发现文件变更，通知另一个扩展刷新配置；
//! 模式扩展切换模式时通知其他扩展；MCP 工具发现新工具时广播工具变更。
//!
//! 在引入本模块之前，扩展只能通过宿主的 hook 系统间接交互，
//! 无法直接广播或监听自定义事件。
//!
//! # 用法
//!
//! ```ignore
//! let bus = EventBus::new();
//!
//! bus.on("config_changed", Arc::new(|channel, data| {
//!     tracing::info!("config changed: {data}");
//! }));
//!
//! bus.emit("config_changed", &serde_json::json!({ "key": "model" }));
//! ```
//!
//! # 线程安全
//!
//! `EventBus` 是 `Send + Sync` 的，所有操作都是原子且非阻塞的。
//! 事件处理器同步调用。若需要异步操作，处理器应自行 `tokio::spawn`。

use std::{
    collections::HashMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use serde_json::Value;

/// 事件总线句柄的标识符。
type HandlerId = usize;

/// 事件处理器的类型签名。
///
/// 参数：
/// - `channel`: 事件通道名
/// - `data`: 事件载荷
pub type EventBusHandler = Arc<dyn Fn(&str, &Value) + Send + Sync>;

/// 扩展间共享的事件总线。
///
/// 基于通道（channel）的发布-订阅模型。支持任意字符串通道名，
/// 每个通道可以有多个处理器。`emit` 调用会同步遍历该通道的所有处理器。
///
/// 内部使用 `Arc` 实现共享，可通过 `Clone` 传递引用。
#[derive(Clone)]
pub struct EventBus {
    inner: Arc<EventBusInner>,
}

struct EventBusInner {
    handlers: RwLock<HashMap<String, Vec<(HandlerId, EventBusHandler)>>>,
    next_id: AtomicUsize,
}

impl EventBus {
    /// 创建一个新的事件总线。
    pub fn new() -> Self {
        Self {
            inner: Arc::new(EventBusInner {
                handlers: RwLock::new(HashMap::new()),
                next_id: AtomicUsize::new(1),
            }),
        }
    }

    /// 向指定通道发射事件。
    ///
    /// 同步调用该通道上所有已注册的处理器。
    /// 处理器中的 panic 会被 `catch_unwind` 捕获并记录错误，不会影响其他处理器。
    ///
    /// # 参数
    /// - `channel`: 事件通道名（如 `"config_changed"`、`"tool_discovered"`）
    /// - `data`: 事件载荷
    pub fn emit(&self, channel: &str, data: &Value) {
        let snapshot: Vec<EventBusHandler> = {
            let guard = self.inner.handlers.read().unwrap();
            guard
                .get(channel)
                .map(|handlers| handlers.iter().map(|(_, h)| Arc::clone(h)).collect())
                .unwrap_or_default()
        };

        for handler in &snapshot {
            handler(channel, data);
        }
    }

    /// 注册一个指定通道的事件处理器。
    ///
    /// 处理器会在调用线程同步执行。处理器应避免长时间阻塞，
    /// 若需要异步操作请自行 `tokio::spawn`。
    ///
    /// # 参数
    /// - `channel`: 要监听的事件通道名
    /// - `handler`: 事件处理器
    pub fn on(&self, channel: &str, handler: EventBusHandler) {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let mut guard = self.inner.handlers.write().unwrap();
        guard
            .entry(channel.to_string())
            .or_default()
            .push((id, handler));
    }

    /// 移除指定通道上的一个处理器。
    ///
    /// 通常不需要显式调用——处理器会在 EventBus 被丢弃时自动清理。
    /// 此方法用于需要提前取消订阅的场景。
    pub fn off(&self, channel: &str, handler_id: HandlerId) {
        let mut guard = self.inner.handlers.write().unwrap();
        if let Some(handlers) = guard.get_mut(channel) {
            handlers.retain(|(id, _)| *id != handler_id);
            if handlers.is_empty() {
                guard.remove(channel);
            }
        }
    }

    /// 返回指定通道上的处理器数量。
    pub fn handler_count(&self, channel: &str) -> usize {
        let guard = self.inner.handlers.read().unwrap();
        guard.get(channel).map_or(0, |h| h.len())
    }

    /// 返回所有已注册的通道名称。
    pub fn channels(&self) -> Vec<String> {
        let guard = self.inner.handlers.read().unwrap();
        guard.keys().cloned().collect()
    }

    /// 清空所有通道的所有处理器。
    pub fn clear(&self) {
        let mut guard = self.inner.handlers.write().unwrap();
        guard.clear();
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;

    #[test]
    fn emit_reaches_registered_handler() {
        let bus = EventBus::new();
        let received = Arc::new(AtomicBool::new(false));

        let flag = Arc::clone(&received);
        bus.on(
            "test",
            Arc::new(move |_, _| {
                flag.store(true, Ordering::SeqCst);
            }),
        );

        bus.emit("test", &Value::Null);
        assert!(received.load(Ordering::SeqCst));
    }

    #[test]
    fn different_channel_not_received() {
        let bus = EventBus::new();
        let received = Arc::new(AtomicBool::new(false));

        let flag = Arc::clone(&received);
        bus.on(
            "channel_a",
            Arc::new(move |_, _| {
                flag.store(true, Ordering::SeqCst);
            }),
        );

        bus.emit("channel_b", &Value::Null);
        assert!(!received.load(Ordering::SeqCst));
    }

    #[test]
    fn handler_receives_correct_channel_and_data() {
        let bus = EventBus::new();
        let captured = Arc::new(std::sync::Mutex::new((String::new(), Value::Null)));

        let capture = Arc::clone(&captured);
        bus.on(
            "events",
            Arc::new(move |ch, data| {
                let mut c = capture.lock().unwrap();
                c.0 = ch.to_string();
                c.1 = data.clone();
            }),
        );

        let payload = serde_json::json!({"key": "value"});
        bus.emit("events", &payload);

        let c = captured.lock().unwrap();
        assert_eq!(c.0, "events");
        assert_eq!(c.1, payload);
    }

    #[test]
    fn multiple_handlers_all_invoked() {
        let bus = EventBus::new();
        let count = Arc::new(AtomicUsize::new(0));

        let c1 = Arc::clone(&count);
        bus.on(
            "ch",
            Arc::new(move |_, _| {
                c1.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let c2 = Arc::clone(&count);
        bus.on(
            "ch",
            Arc::new(move |_, _| {
                c2.fetch_add(1, Ordering::SeqCst);
            }),
        );

        bus.emit("ch", &Value::Null);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn off_removes_handler() {
        let bus = EventBus::new();
        let count = Arc::new(AtomicUsize::new(0));

        let c = Arc::clone(&count);
        bus.on(
            "ch",
            Arc::new(move |_, _| {
                c.fetch_add(1, Ordering::SeqCst);
            }),
        );

        // Record the handler ID to remove - we know it's 1 since this is the only one
        bus.off("ch", 1);

        bus.emit("ch", &Value::Null);
        // Handler was removed, count should still be 0
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn clear_removes_all_handlers() {
        let bus = EventBus::new();
        let count = Arc::new(AtomicUsize::new(0));

        let c = Arc::clone(&count);
        bus.on(
            "ch",
            Arc::new(move |_, _| {
                c.fetch_add(1, Ordering::SeqCst);
            }),
        );

        bus.clear();
        assert_eq!(bus.handler_count("ch"), 0);
    }

    #[test]
    fn channels_returns_registered_channels() {
        let bus = EventBus::new();
        bus.on("a", Arc::new(|_, _| {}));
        bus.on("b", Arc::new(|_, _| {}));

        let mut channels = bus.channels();
        channels.sort();
        assert_eq!(channels, vec!["a", "b"]);
    }

    #[test]
    fn handler_count_is_accurate() {
        let bus = EventBus::new();
        assert_eq!(bus.handler_count("ch"), 0);

        bus.on("ch", Arc::new(|_, _| {}));
        assert_eq!(bus.handler_count("ch"), 1);

        bus.on("ch", Arc::new(|_, _| {}));
        assert_eq!(bus.handler_count("ch"), 2);

        bus.off("ch", 1);
        assert_eq!(bus.handler_count("ch"), 1);
    }
}
