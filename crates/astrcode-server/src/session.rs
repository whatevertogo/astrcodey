//! 会话模块 — 基于事件溯源的持久化会话管理。
//!
//! 核心组件：
//! - [`Session`][]: 会话实体，持有内存中的状态和事件广播通道
//! - [`SessionState`][]: 会话的内存投影状态（消息列表、阶段等）
//! - [`EventReducer`][]: 纯函数式事件归约器，将事件应用到状态上
//! - [`SessionManager`][]: 会话管理器，协调内存缓存与持久化存储

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use astrcode_context::compaction::CompactResult;
use astrcode_core::{
    event::{Event, EventPayload, Phase},
    llm::{LlmContent, LlmMessage, LlmRole},
    storage::{CompactSnapshotInput, EventStore, StorageError},
    types::*,
};
use astrcode_protocol::events::ClientNotification;
use tokio::sync::{RwLock, broadcast};

pub(crate) const COMPACTION_APPLIED_EVENT: &str = "context.compaction_applied";

// ─── Session ─────────────────────────────────────────────────────────────

/// 会话实体，代表一个与用户的对话上下文。
///
/// 持有读写锁保护的状态和广播通道，支持多个消费者订阅会话事件。
pub struct Session {
    /// 会话唯一标识
    pub id: SessionId,
    /// 会话的内存投影状态，通过读写锁支持并发访问
    pub state: RwLock<SessionState>,
    /// 事件广播发送端，用于向订阅者推送通知
    event_tx: broadcast::Sender<ClientNotification>,
}

/// 会话的内存投影状态，由事件归约器从事件流中构建。
///
/// 每次追加事件后，归约器会更新此状态以反映最新的会话快照。
#[derive(Debug, Clone)]
pub struct SessionState {
    /// 对话消息历史（包含用户、助手和工具消息）
    pub messages: Vec<LlmMessage>,
    /// 只发送给 provider 的合成上下文，不在普通会话快照中展示。
    pub context_messages: Vec<LlmMessage>,
    /// 会话的工作目录
    pub working_dir: String,
    /// 使用的模型标识
    pub model_id: String,
    /// 当前会话阶段（空闲、思考中、流式输出、调用工具等）
    pub phase: Phase,
    /// 会话初始化时固定下来的完整 system prompt。
    ///
    /// 它来自 durable `SystemPromptConfigured` 事件，不进入 `messages`，
    /// 避免恢复会话时把大段系统提示展示给用户。
    pub system_prompt: Option<String>,
    /// 正在等待完成的工具调用 ID 集合
    pub pending_tool_calls: HashSet<ToolCallId>,
    /// ISO 8601 格式的创建时间，由 SessionStarted 事件填充
    pub created_at: String,
}

impl SessionState {
    /// 创建初始的空会话状态。
    fn new(working_dir: String, model_id: String) -> Self {
        Self {
            messages: Vec::new(),
            context_messages: Vec::new(),
            working_dir,
            model_id,
            phase: Phase::Idle,
            system_prompt: None,
            pending_tool_calls: HashSet::new(),
            created_at: String::new(),
        }
    }

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
}

pub(crate) fn compaction_applied_payload(compaction: &CompactResult) -> EventPayload {
    EventPayload::Custom {
        name: COMPACTION_APPLIED_EVENT.into(),
        data: serde_json::json!({
            "messagesRemoved": compaction.messages_removed,
            "contextMessages": compaction.context_messages,
            "summary": compaction.summary,
            "preTokens": compaction.pre_tokens,
            "postTokens": compaction.post_tokens,
            "transcriptPath": compaction.transcript_path.clone(),
        }),
    }
}

impl Session {
    /// 创建新的会话实例。
    ///
    /// # 参数
    /// - `id`: 会话唯一标识
    /// - `working_dir`: 工作目录
    /// - `model_id`: 模型标识
    /// - `capacity`: 广播通道容量
    pub fn new(id: SessionId, working_dir: String, model_id: String, capacity: usize) -> Self {
        let (event_tx, _) = broadcast::channel(capacity);
        Self {
            id,
            state: RwLock::new(SessionState::new(working_dir, model_id)),
            event_tx,
        }
    }

    /// 订阅此会话的事件广播，返回一个新的接收端。
    pub fn subscribe(&self) -> broadcast::Receiver<ClientNotification> {
        self.event_tx.subscribe()
    }
}

// ─── EventReducer ────────────────────────────────────────────────────────

/// 纯函数式事件归约器，将事件应用到会话状态上。
///
/// 不持有任何状态，所有方法都是纯函数。
/// 根据事件类型更新会话的阶段、消息列表和工具调用状态。
pub struct EventReducer;

impl EventReducer {
    /// 将单个事件归约到会话状态上。
    ///
    /// 根据事件负载类型执行不同的状态更新：
    /// - 会话启动/删除：更新基础信息和阶段
    /// - 回合启动/完成：更新阶段和消息
    /// - 助手消息：追加到消息列表
    /// - 工具调用：管理待完成调用集合并追加消息
    /// - 错误：设置错误阶段
    pub fn reduce(event: &Event, state: &mut SessionState) {
        match &event.payload {
            EventPayload::SessionStarted {
                working_dir,
                model_id,
                ..
            } => {
                state.working_dir = working_dir.clone();
                state.model_id = model_id.clone();
                state.phase = Phase::Idle;
                // 使用事件的 seq 来判断是否是首次 SessionStarted，
                // 首次（seq=0）时记录创建时间
                if state.created_at.is_empty() {
                    state.created_at = chrono::Utc::now().to_rfc3339();
                }
            },
            EventPayload::SessionDeleted => {
                state.phase = Phase::Idle;
                // 清理完整状态，避免已删除 session 的残留数据被误用
                state.messages.clear();
                state.context_messages.clear();
                state.system_prompt = None;
                state.pending_tool_calls.clear();
            },
            EventPayload::SystemPromptConfigured { text, .. } => {
                state.system_prompt = Some(text.clone());
            },
            EventPayload::TurnStarted | EventPayload::UserMessage { .. } => {
                state.phase = Phase::Thinking;
                if let EventPayload::UserMessage { text, .. } = &event.payload {
                    state.messages.push(LlmMessage::user(text));
                }
            },
            EventPayload::TurnCompleted { .. } => {
                state.phase = Phase::Idle;
                state.pending_tool_calls.clear();
            },
            EventPayload::AssistantMessageStarted { .. }
            | EventPayload::AssistantTextDelta { .. }
            | EventPayload::ThinkingDelta { .. } => {
                state.phase = Phase::Streaming;
            },
            EventPayload::AssistantMessageCompleted { text, .. } => {
                state.messages.push(LlmMessage::assistant(text));
                state.phase = Phase::Idle;
            },
            EventPayload::ToolCallStarted { call_id, .. } => {
                state.pending_tool_calls.insert(call_id.clone());
                state.phase = Phase::CallingTool;
            },
            EventPayload::ToolCallArgumentsDelta { .. } | EventPayload::ToolOutputDelta { .. } => {
                state.phase = Phase::CallingTool;
            },
            EventPayload::ToolCallRequested {
                call_id,
                tool_name,
                arguments,
            } => {
                state.pending_tool_calls.insert(call_id.clone());
                state.messages.push(LlmMessage {
                    role: LlmRole::Assistant,
                    content: vec![LlmContent::ToolCall {
                        call_id: call_id.clone(),
                        name: tool_name.clone(),
                        arguments: arguments.clone(),
                    }],
                    name: None,
                });
                state.phase = Phase::CallingTool;
            },
            EventPayload::ToolCallCompleted {
                call_id,
                tool_name,
                result,
            } => {
                state.pending_tool_calls.remove(call_id);
                state.messages.push(LlmMessage {
                    role: LlmRole::Tool,
                    content: vec![LlmContent::ToolResult {
                        tool_call_id: call_id.clone(),
                        content: result.content.clone(),
                        is_error: result.is_error,
                    }],
                    name: Some(tool_name.clone()),
                });
                state.phase = if state.pending_tool_calls.is_empty() {
                    Phase::Thinking
                } else {
                    Phase::CallingTool
                };
            },
            EventPayload::CompactionStarted => {
                state.phase = Phase::Compacting;
            },
            EventPayload::CompactionCompleted { .. } => {
                state.phase = Phase::Idle;
            },
            EventPayload::AgentRunStarted => {
                state.phase = Phase::Thinking;
            },
            EventPayload::AgentRunCompleted { .. } => {
                state.phase = Phase::Idle;
            },
            EventPayload::ErrorOccurred { .. } => {
                state.phase = Phase::Error;
            },
            EventPayload::Custom { name, data } if name == COMPACTION_APPLIED_EVENT => {
                apply_compaction_projection(data, state);
            },
            EventPayload::Custom { .. } => {},
        }
    }

    /// Replay a list of events to build initial SessionState.
    pub fn replay(events: &[Event]) -> SessionState {
        let mut state = SessionState::new(String::new(), String::new());
        for event in events {
            Self::reduce(event, &mut state);
        }
        state
    }
}

fn apply_compaction_projection(data: &serde_json::Value, state: &mut SessionState) {
    let Some(messages_removed) = data
        .get("messagesRemoved")
        .and_then(serde_json::Value::as_u64)
        .map(|value| value as usize)
    else {
        return;
    };
    let context_messages = data
        .get("contextMessages")
        .cloned()
        .and_then(|value| serde_json::from_value::<Vec<LlmMessage>>(value).ok())
        .unwrap_or_default();

    let drain_end = messages_removed.min(state.messages.len());
    state.messages.drain(..drain_end);
    state.context_messages = context_messages;
}

// ─── SessionManager ──────────────────────────────────────────────────────

/// 会话管理器，协调内存中的活跃会话与持久化事件存储。
///
/// 提供会话的完整生命周期管理：创建、恢复、事件追加、列表查询和删除。
/// 内存中维护一个活跃会话的缓存映射，避免频繁从磁盘重放事件。
pub struct SessionManager {
    /// 活跃会话的内存缓存，按会话 ID 索引
    active: RwLock<HashMap<SessionId, Arc<Session>>>,
    /// 持久化事件存储后端
    store: Arc<dyn EventStore>,
}

impl SessionManager {
    /// 创建新的会话管理器。
    ///
    /// # 参数
    /// - `store`: 事件存储后端实现
    pub fn new(store: Arc<dyn EventStore>) -> Self {
        Self {
            active: RwLock::new(HashMap::new()),
            store,
        }
    }

    /// Create a new session and persist SessionStarted.
    pub async fn create(
        &self,
        working_dir: &str,
        model_id: &str,
        capacity: usize,
        parent_session_id: Option<&str>,
    ) -> Result<Event, SessionError> {
        let sid = new_session_id();
        let event = self
            .store
            .create_session(&sid, working_dir, model_id, parent_session_id)
            .await?;

        let session = Arc::new(Session::new(
            sid.clone(),
            working_dir.into(),
            model_id.into(),
            capacity,
        ));
        self.active.write().await.insert(sid, session);
        Ok(event)
    }

    /// Resume a session from disk, replay events, add to active set.
    pub async fn resume(&self, session_id: &SessionId) -> Result<Arc<Session>, SessionError> {
        if let Some(s) = self.active.read().await.get(session_id) {
            return Ok(s.clone());
        }

        self.store.open_session(session_id).await?;
        let events = self.store.replay_events(session_id).await?;
        let state = EventReducer::replay(&events);

        let session = Arc::new(Session {
            id: session_id.clone(),
            state: RwLock::new(state),
            event_tx: broadcast::channel(2048).0,
        });
        self.active
            .write()
            .await
            .insert(session_id.clone(), session.clone());
        Ok(session)
    }

    /// Append a durable event to disk and update in-memory state.
    pub async fn append_event(&self, event: Event) -> Result<Event, SessionError> {
        let stored = self.store.append_event(event).await?;
        if let Some(session) = self.active.read().await.get(&stored.session_id).cloned() {
            EventReducer::reduce(&stored, &mut *session.state.write().await);
        }
        Ok(stored)
    }

    pub async fn write_compact_snapshot(
        &self,
        session_id: &SessionId,
        snapshot: CompactSnapshotInput,
    ) -> Result<Option<String>, SessionError> {
        Ok(self
            .store
            .write_compact_snapshot(session_id, snapshot)
            .await?)
    }

    /// Get active session by ID.
    pub async fn get(&self, session_id: &SessionId) -> Option<Arc<Session>> {
        self.active.read().await.get(session_id).cloned()
    }

    /// List all sessions (from disk).
    pub async fn list(&self) -> Result<Vec<SessionId>, SessionError> {
        Ok(self.store.list_sessions().await?)
    }

    /// 获取当前活跃 session 映射的读引用，用于 ListSessions 等需要批量查询的场景
    pub async fn active(
        &self,
    ) -> tokio::sync::RwLockReadGuard<'_, HashMap<SessionId, Arc<Session>>> {
        self.active.read().await
    }

    /// Delete session from memory and disk.
    pub async fn delete(&self, session_id: &SessionId) -> Result<(), SessionError> {
        self.active.write().await.remove(session_id);
        self.store.delete_session(session_id).await?;
        Ok(())
    }
}

/// 会话操作中可能出现的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Session not found: {0}")]
    NotFound(SessionId),
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),
}
