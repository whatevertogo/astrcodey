//! 会话存储 trait 定义。
//!
//! 本模块定义了会话事件持久化的核心抽象：
//! - [`EventStore`] trait：事件存储的统一接口
//! - [`SessionInfo`]：会话元数据
//! - [`StorageError`]：存储操作错误类型

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    event::{Event, Phase},
    llm::LlmMessage,
    types::*,
};

/// 会话事件存储 trait。
///
/// 实现类负责持久化统一事件，并在事件进入 JSONL 日志时
/// 分配递增的会话内序号。
#[async_trait::async_trait]
pub trait EventStore: Send + Sync {
    /// 创建新的会话事件日志，并写入初始的 SessionStarted 事件。
    ///
    /// - `session_id`：会话唯一标识
    /// - `working_dir`：工作目录路径
    /// - `model_id`：使用的模型标识
    /// - `parent_session_id`：父会话 ID（子会话场景），可为 `None`
    async fn create_session(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        model_id: &str,
        parent_session_id: Option<&str>,
    ) -> Result<Event, StorageError>;

    /// 向会话的事件日志追加一个事件。
    ///
    /// 存储层会为事件分配递增序号。
    async fn append_event(&self, event: Event) -> Result<Event, StorageError>;

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

    /// 返回所有会话摘要，供列表类接口使用。
    async fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError>;

    /// 返回当前 conversation 的全量快照读模型。
    ///
    /// v1 语义是 full snapshot；cursor 仅表示该快照对应的最新 durable seq，
    /// 不表示“从此 cursor 开始增量查询”。
    async fn conversation_snapshot(
        &self,
        session_id: &SessionId,
    ) -> Result<ConversationReadModel, StorageError>;

    /// 返回当前会话最新 durable cursor。
    async fn latest_cursor(&self, session_id: &SessionId) -> Result<Option<Cursor>, StorageError>;

    /// 从指定的游标位置开始重放事件。
    async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<Event>, StorageError>;

    /// 在当前位置创建检查点快照。
    async fn checkpoint(&self, session_id: &SessionId, cursor: &Cursor)
    -> Result<(), StorageError>;

    /// 列出所有会话 ID。
    async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError>;

    /// 从磁盘打开已有的会话，准备追加操作。
    ///
    /// 在恢复的会话上调用 `append_event` 之前必须先调用此方法。
    async fn open_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        self.replay_events(session_id).await.map(|_| ())
    }

    /// 删除会话及其所有数据。
    async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError>;

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

    /// 读取当前 session 关联工具结果 artifact 路径的一段文本。
    async fn read_tool_result_artifact_by_path(
        &self,
        _session_id: &SessionId,
        _path: &str,
        _char_offset: usize,
        _max_chars: usize,
    ) -> Result<ToolResultArtifactSlice, StorageError> {
        Err(StorageError::Unsupported(
            "tool result artifact storage is not supported".into(),
        ))
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

/// compact 前 transcript snapshot 的存储输入。
///
/// 这是持久化边界的数据包；调用方决定收集哪些 provider messages，存储层只负责
/// 把它写成可读的 JSONL 文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactSnapshotInput {
    /// compact 触发来源，例如 `auto_threshold`。
    pub trigger: String,
    /// 当前模型标识。
    pub model_id: String,
    /// 当前工作目录。
    pub working_dir: String,
    /// 当前 session system prompt。
    pub system_prompt: Option<String>,
    /// compact 前的 provider 可见消息，不包含单独记录的 system prompt。
    pub provider_messages: Vec<LlmMessage>,
}

/// 工具结果 artifact 写入输入。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultArtifactInput {
    /// 工具调用 ID。
    pub call_id: String,
    /// 工具名称。
    pub tool_name: String,
    /// 原始工具输出正文。
    pub content: String,
}

/// 已持久化工具结果的引用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolResultArtifactRef {
    /// 原始正文 UTF-8 字节数。
    pub bytes: usize,
    /// 可展示给 `read` 使用的存储路径；内存存储可用虚拟路径。
    pub path: Option<String>,
}

/// 工具结果 artifact 的分页读取结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolResultArtifactSlice {
    /// artifact 路径。
    pub path: String,
    /// artifact 原始 UTF-8 字节数。
    pub bytes: usize,
    /// 本次读取的字符偏移。
    pub char_offset: usize,
    /// 本次返回的字符数。
    pub returned_chars: usize,
    /// 下一次读取的字符偏移；没有更多内容时为空。
    pub next_char_offset: Option<usize>,
    /// 是否还有更多内容。
    pub has_more: bool,
    /// 本次读取的正文片段。
    pub content: String,
}

/// 会话事件流的内部读模型。
///
/// 这是 storage/domain 边界类型，不是 wire DTO。它只能由事件日志重建，并由
/// server 映射到具体传输协议。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionReadModel {
    /// 会话唯一标识。
    pub session_id: SessionId,
    /// 普通对话消息历史。
    pub messages: Vec<LlmMessage>,
    /// provider 可见但不展示给普通 transcript 的上下文消息。
    pub context_messages: Vec<LlmMessage>,
    /// 会话工作目录。
    pub working_dir: String,
    /// 模型标识。
    pub model_id: String,
    /// 当前执行阶段。
    pub phase: Phase,
    /// 会话级 system prompt。
    pub system_prompt: Option<String>,
    /// 尚未完成的工具调用。
    pub pending_tool_calls: HashSet<ToolCallId>,
    /// 创建时间（ISO 8601）。
    pub created_at: String,
    /// 更新时间（ISO 8601）。
    pub updated_at: String,
    /// 父会话 ID。
    pub parent_session_id: Option<SessionId>,
    /// 最新 durable 事件 seq。
    pub latest_seq: Option<u64>,
}

impl SessionReadModel {
    /// 创建空读模型。
    pub fn empty(session_id: SessionId) -> Self {
        Self {
            session_id,
            messages: Vec::new(),
            context_messages: Vec::new(),
            working_dir: String::new(),
            model_id: String::new(),
            phase: Phase::Idle,
            system_prompt: None,
            pending_tool_calls: HashSet::new(),
            created_at: String::new(),
            updated_at: String::new(),
            parent_session_id: None,
            latest_seq: None,
        }
    }

    /// 返回 provider 可见消息。
    pub fn provider_messages(&self) -> Vec<LlmMessage> {
        let mut messages = Vec::with_capacity(
            self.context_messages
                .len()
                .saturating_add(self.messages.len()),
        );
        messages.extend(self.context_messages.clone());
        messages.extend(self.messages.clone());
        messages
    }

    /// 当前快照 cursor。
    pub fn cursor(&self) -> Cursor {
        self.latest_seq
            .map(|seq| seq.to_string())
            .unwrap_or_else(|| "0".into())
    }
}

/// 会话列表摘要读模型。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    /// 会话唯一标识。
    pub session_id: SessionId,
    /// 创建时间（ISO 8601）。
    pub created_at: String,
    /// 更新时间（ISO 8601）。
    pub updated_at: String,
    /// 工作目录。
    pub working_dir: String,
    /// 模型标识。
    pub model_id: String,
    /// 父会话 ID。
    pub parent_session_id: Option<SessionId>,
    /// 当前执行阶段。
    pub phase: Phase,
    /// 最新 durable cursor。
    pub latest_cursor: Cursor,
}

impl From<SessionReadModel> for SessionSummary {
    fn from(model: SessionReadModel) -> Self {
        let latest_cursor = model.cursor();
        Self {
            session_id: model.session_id,
            created_at: model.created_at,
            updated_at: model.updated_at,
            working_dir: model.working_dir,
            model_id: model.model_id,
            parent_session_id: model.parent_session_id,
            phase: model.phase,
            latest_cursor,
        }
    }
}

/// conversation hydration 的内部全量快照。
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationReadModel {
    /// 当前会话读模型。
    pub session: SessionReadModel,
}

/// 会话元数据，用于列表展示。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// 会话唯一标识。
    pub session_id: SessionId,
    /// 会话创建时间。
    pub created_at: DateTime<Utc>,
    /// 最后活跃时间。
    pub last_active_at: DateTime<Utc>,
    /// 工作目录路径。
    pub working_dir: String,
    /// 使用的模型标识。
    pub model_id: String,
    /// 父会话 ID（子会话场景）。
    pub parent_session_id: Option<SessionId>,
}

/// 存储操作产生的错误。
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// 找不到指定的会话。
    #[error("Session not found: {0}")]
    NotFound(SessionId),
    /// IO 错误。
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// 序列化/反序列化错误。
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    /// 无效的会话 ID。
    #[error("Invalid session ID: {0}")]
    InvalidId(String),
    /// 锁操作错误。
    #[error("Lock error: {0}")]
    LockError(String),
    /// 当前存储实现不支持该能力。
    #[error("Unsupported storage operation: {0}")]
    Unsupported(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmMessage;

    #[test]
    fn session_read_model_serializes_round_trip() {
        let mut model = SessionReadModel::empty("session-test".into());
        model.working_dir = "D:/work/project".into();
        model.model_id = "mock-model".into();
        model.messages.push(LlmMessage::user("hello"));
        model.context_messages.push(LlmMessage::system("system"));
        model.latest_seq = Some(7);

        let encoded = serde_json::to_string(&model).unwrap();
        let decoded: SessionReadModel = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, model);
    }

    #[test]
    fn session_read_model_cursor_defaults_to_zero() {
        let model = SessionReadModel::empty("session-test".into());

        assert_eq!(model.cursor(), "0");
    }
}
