//! 会话存储 trait 定义。
//!
//! 本模块定义了会话事件持久化的核心抽象：
//! - [`EventStore`] trait：事件存储的统一接口
//! - [`SessionInfo`]：会话元数据
//! - [`StorageError`]：存储操作错误类型

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{event::Event, llm::LlmMessage, types::*};

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
}
