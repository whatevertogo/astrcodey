//! 事件查询能力。
//!
//! 允许扩展查询会话历史，无需依赖 `EventReader`/`EventStore` trait
//! 或 `SessionReadModel`/`SessionSummary` 等核心类型。

use std::sync::Arc;

use super::view_types::{ConversationView, SessionSummaryView};
use super::Capability;

// ─── EventQueryInner ──────────────────────────────────────────────────

/// 事件查询的能力接口。由宿主侧实现。
#[async_trait::async_trait]
pub trait EventQueryInner: Send + Sync + 'static {
    /// 列出所有会话摘要。
    async fn list_session_summaries(&self) -> Result<Vec<SessionSummaryView>, String>;

    /// 读取指定会话的对话内容。
    ///
    /// 实现侧负责将 `SessionReadModel.messages` 转换为 `Vec<TurnView>`，
    /// 过滤掉 `Tool`/`ToolResult` 等非对话内容。
    async fn read_conversation(&self, session_id: &str) -> Result<ConversationView, String>;
}

// ─── EventQueryCap ────────────────────────────────────────────────────

/// 事件查询能力的 newtype 包装。
pub struct EventQueryCap(Arc<dyn EventQueryInner>);

impl EventQueryCap {
    pub fn new(inner: Arc<dyn EventQueryInner>) -> Self {
        Self(inner)
    }
}

impl Capability for EventQueryCap {}

impl EventQueryCap {
    pub async fn list_session_summaries(&self) -> Result<Vec<SessionSummaryView>, String> {
        self.0.list_session_summaries().await
    }

    pub async fn read_conversation(&self, session_id: &str) -> Result<ConversationView, String> {
        self.0.read_conversation(session_id).await
    }
}

impl std::fmt::Debug for EventQueryCap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventQueryCap").finish()
    }
}
