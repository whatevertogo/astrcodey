//! 存储抽象 trait。
//!
//! 从 `storage` 根模块拆出:`EventReader`(只读)、`EventStore`(读写,继承
//! `EventReader`)、`ToolResultArtifactReader`。通过 trait upcasting(Rust 1.86+),
//! `Arc<dyn EventStore>` 可直接转 `Arc<dyn EventReader>`,不泄漏写入能力。

use super::{error::StorageError, read_model::*};
use crate::{event::Event, llm::LlmMessage, types::*};

/// 会话存储的只读查询能力。
///
/// 从 [`EventStore`] 拆分出来，满足接口隔离原则（ISP）：
/// 只需要查询会话状态的消费者（SSE 流、扩展、HTTP 列表接口等）
/// 应依赖 `Arc<dyn EventReader>` 而非 `Arc<dyn EventStore>`。
///
/// 由于 `EventStore: EventReader` 建立了 supertrait 关系，
/// `Arc<dyn EventStore>` 可通过 trait upcasting（Rust 1.86+）自动转换为
/// `Arc<dyn EventReader>`，无需 newtype wrapper。
#[async_trait::async_trait]
pub trait EventReader: Send + Sync {
    /// 从头开始重放会话的所有事件。
    async fn replay_events(&self, session_id: &SessionId) -> Result<Vec<Event>, StorageError>;

    /// 返回当前会话读模型。
    ///
    /// 读模型是事件日志的同步投影缓存，必须能够从事件日志重建；调用方不能把
    /// 它当作事实源或线缆协议类型。
    async fn session_read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionReadModel, StorageError>;

    /// 返回当前会话 provider 可见消息。
    ///
    /// 默认实现保留兼容性；存储实现应覆盖为从内存投影直接派生，避免为了读取
    /// LLM 历史先 clone 整个 [`SessionReadModel`]。
    async fn session_provider_messages(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<LlmMessage>, StorageError> {
        Ok(self
            .session_read_model(session_id)
            .await?
            .provider_messages())
    }

    /// 返回当前会话的 system_prompt，只读单个字段避免 clone 整个读模型。
    async fn session_system_prompt(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<String>, StorageError>;

    /// 返回当前会话是否已有普通 transcript 消息。
    async fn session_has_messages(&self, session_id: &SessionId) -> Result<bool, StorageError> {
        Ok(self.session_read_model(session_id).await?.has_messages())
    }

    /// 返回当前会话的子 agent 链接状态。
    async fn session_agent_sessions(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<AgentSessionLinkView>, StorageError> {
        Ok(self.session_read_model(session_id).await?.agent_sessions)
    }

    /// 统计 provider 可见的非合成 user 消息条数。
    async fn session_visible_user_message_count(
        &self,
        session_id: &SessionId,
    ) -> Result<usize, StorageError> {
        Ok(self
            .session_read_model(session_id)
            .await?
            .visible_user_message_count())
    }

    /// 返回所有会话摘要，供列表类接口使用。
    async fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError>;

    /// 返回当前会话最新 durable cursor。
    async fn latest_cursor(&self, session_id: &SessionId) -> Result<Option<Cursor>, StorageError>;

    /// 从指定的游标位置之后重放事件（exclusive: seq > cursor）。
    async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<Event>, StorageError>;

    /// 从游标后最多重放 `max_events` 条事件。
    ///
    /// 文件存储应覆盖此方法以在扫描阶段即停止；默认实现用于轻量或内存存储。
    async fn replay_from_limited(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
        max_events: usize,
    ) -> Result<Vec<Event>, StorageError> {
        let mut events = self.replay_from(session_id, cursor).await?;
        events.truncate(max_events);
        Ok(events)
    }

    /// 列出所有会话 ID。
    async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError>;

    /// 读取当前 session 关联工具结果 artifact 路径的一段文本。
    async fn read_tool_result_artifact_by_path(
        &self,
        session_id: &SessionId,
        path: &str,
        char_offset: usize,
        max_chars: usize,
    ) -> Result<ToolResultArtifactSlice, StorageError>;

    /// 返回指定会话在存储层中的真实目录路径。
    ///
    /// 工具需要往 session 目录写入附属数据（todos、mode、plan 等）时，
    /// 应通过此方法获取路径，而不是自行拼接——子 session 的真实目录
    /// 可能在 `subagents/{extension}/` 下，无法从 session_id + working_dir 推断。
    async fn session_store_dir(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<std::path::PathBuf>, StorageError>;
}

/// 会话事件存储 trait。
///
/// 继承 [`EventReader`] 的所有只读方法，并添加写入和生命周期管理方法。
/// 实现类负责持久化统一事件，并在事件进入 JSONL 日志时
/// 分配递增的会话内序号。
#[async_trait::async_trait]
pub trait EventStore: EventReader + Send + Sync {
    /// 创建新的会话事件日志，并写入初始的 SessionStarted 事件。
    ///
    /// - `session_id`：会话唯一标识
    /// - `working_dir`：工作目录路径
    /// - `model_id`：使用的模型标识
    /// - `parent_session_id`：父会话 ID（子会话场景），可为 `None`
    /// - `tool_selection`：session 初始工具选择，`None` 表示不限制
    /// - `source_extension`：创建该子 session 的扩展 ID，根会话为 `None`
    async fn create_session(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        model_id: &str,
        parent_session_id: Option<&SessionId>,
        tool_selection: Option<&crate::extension::SessionToolSelection>,
        source_extension: Option<&str>,
    ) -> Result<Event, StorageError>;

    /// 向会话的事件日志追加一个事件。
    ///
    /// 存储层会为事件分配递增序号。
    async fn append_event(&self, event: Event) -> Result<Event, StorageError>;

    /// 在当前位置创建检查点快照。
    async fn checkpoint(&self, session_id: &SessionId, cursor: &Cursor)
    -> Result<(), StorageError>;

    /// 从磁盘打开已有的会话，准备追加操作。
    ///
    /// 在恢复的会话上调用 `append_event` 之前必须先调用此方法。
    async fn open_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        self.replay_events(session_id).await.map(|_| ())
    }

    /// 删除会话及其所有数据。
    async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError>;

    /// 回收子 session：从活跃列表移除。
    ///
    /// 默认行为会退化为删除。持久化实现应覆盖为保留数据的回收语义，例如移动到
    /// `.recycled/` 目录。
    async fn recycle_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        tracing::warn!(
            session_id = %session_id,
            "EventStore::recycle_session fell back to delete_session; this storage implementation does not preserve recycled session data"
        );
        self.delete_session(session_id).await
    }

    /// 从 .recycled/ 恢复一个已回收的 session。
    ///
    /// 默认返回 Unsupported。文件系统实现应将 session 从 `.recycled/` 移回原位。
    async fn restore_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        let _ = session_id;
        Err(StorageError::Unsupported(
            "restore_session is not supported by this storage implementation".into(),
        ))
    }

    /// 写入 compact 前的 provider transcript snapshot。
    ///
    /// 返回值是可供用户或后续工具读取的快照路径；不支持快照的存储实现可以返回
    /// `Ok(None)`。
    async fn write_compact_snapshot(
        &self,
        _session_id: &SessionId,
        _snapshot: CompactSnapshotInput,
    ) -> Result<Option<String>, StorageError> {
        Ok(None)
    }

    /// 写入当前 session 关联的工具结果 artifact。
    ///
    /// 这类 artifact 不进入 JSONL event log，而是与 session 目录同生命周期保存。
    async fn write_tool_result_artifact(
        &self,
        _session_id: &SessionId,
        _artifact: ToolResultArtifactInput,
    ) -> Result<ToolResultArtifactRef, StorageError> {
        Err(StorageError::Unsupported(
            "tool result artifact storage is not supported".into(),
        ))
    }

    /// 将会话的 durable event log 强制 fsync 到磁盘。
    ///
    /// 默认空实现；文件系统实现延迟 `sync_all()` 到 turn 边界调用。
    async fn sync_durable_events(&self, _session_id: &SessionId) -> Result<(), StorageError> {
        Ok(())
    }
}

/// 工具结果 artifact 读取能力。
///
/// 该 trait 是工具上下文暴露给 `read` 的最小能力面，避免把完整
/// `EventStore` 暴露给普通工具。
#[async_trait::async_trait]
pub trait ToolResultArtifactReader: Send + Sync {
    /// 读取当前 session 中指定 artifact 路径的一段文本。
    async fn read_tool_result_artifact_by_path(
        &self,
        session_id: &SessionId,
        path: &str,
        char_offset: usize,
        max_chars: usize,
    ) -> Result<ToolResultArtifactSlice, StorageError>;
}
