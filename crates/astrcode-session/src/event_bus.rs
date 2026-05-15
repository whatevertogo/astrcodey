//! 事件总线 — TurnRunner 发射事件的抽象出口。
//!
//! TurnRunner 通过此 trait 发射事件，不知道谁在听。
//! Server 提供实现（持久化 + 广播），测试可提供空实现。

use astrcode_core::{event::EventPayload, types::SessionId};

/// 事件发射目标。
///
/// TurnRunner 每产生一个事件就调 `emit()`。
/// 实现方负责持久化和/或广播。
#[async_trait::async_trait]
pub trait EventBus: Send + Sync {
    /// 发射一个事件。实现应同时处理持久化和客户端广播。
    async fn emit(&self, session_id: &SessionId, payload: EventPayload);
}

/// 丢弃所有事件的空实现，用于测试。
pub struct NoopEventBus;

#[async_trait::async_trait]
impl EventBus for NoopEventBus {
    async fn emit(&self, _session_id: &SessionId, _payload: EventPayload) {}
}
