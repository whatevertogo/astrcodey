//! TurnScheduler — 统一的 turn 生命周期服务。
//!
//! 主会话和子会话共用同一条 submit/abort 路径。取代了之前分散在
//! `CommandHandler.active_turns` 和 `SessionManager.ActiveExecutionIndex` 的两套编排。
//!
//! ## 下一 turn 输入队列（唯一）
//!
//! `pending_queues` 是进程内唯一的「等当前 turn 结束再处理」队列（HTTP / stdio / Actor
//! 均通过 [`notify_turn`](TurnScheduler::notify_turn) 入队）。`on_turn_completed` 按 FIFO
//! 每次只弹出一条并启动新 turn，保证连发 prompt 仍对应多个独立 `UserMessage` 事件。

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use astrcode_core::{
    event::{EventPayload, Phase},
    llm::{LlmContent, LlmRole},
    storage::SessionReadModel,
    tool::ToolResult,
    types::*,
};
use astrcode_session::{
    Session, SessionError,
    child_turn::{ChildCleanup, ChildOutcome},
    turn_handle::TurnHandle,
};
use parking_lot::Mutex;
use thiserror::Error;

use crate::{
    session_manager::SessionManager, session_operations::ServerSessionOperations,
    turn_registry::TurnRegistry,
};

#[path = "turn_scheduler_queue.rs"]
mod turn_queue;

/// Turn 调度层错误（会话是否存在、是否已有 turn 在跑等）。
///
/// 与 [`astrcode_session::turn_context::TurnError`]（单 turn 执行期错误）区分命名，避免跨 crate
/// 歧义。
#[derive(Debug, Error)]
pub enum TurnScheduleError {
    #[error("A turn is already running")]
    TurnAlreadyRunning,
    #[error("No active turn")]
    NoActiveTurn,
    #[error("Session not found: {0}")]
    SessionNotFound(String),
    #[error(transparent)]
    SessionManager(#[from] crate::session_manager::SessionManagerError),
    #[error(transparent)]
    Session(SessionError),
    #[error(transparent)]
    Turn(#[from] astrcode_session::turn_context::TurnError),
    #[error("event emit failed")]
    EventEmit(#[source] SessionError),
}

pub enum SubmitOutcome {
    Started {
        turn_id: TurnId,
        handle: TurnHandle,
    },
    Injected,
    /// 消息已入队，等待当前 turn 结束后处理
    Queued,
}

/// 待处理的消息，用于 "下一 turn" 路径
pub(crate) struct PendingMessage {
    text: String,
}

/// per-session 的待处理消息队列
type PendingQueue = VecDeque<PendingMessage>;

pub struct TurnScheduler {
    session_manager: Arc<SessionManager>,
    registry: Arc<TurnRegistry>,
    /// 等待当前 turn 结束后处理的消息队列
    pub(super) pending_queues: Mutex<HashMap<SessionId, PendingQueue>>,
}

impl TurnScheduler {
    pub fn new(session_manager: Arc<SessionManager>, registry: Arc<TurnRegistry>) -> Self {
        Self {
            session_manager,
            registry,
            pending_queues: Mutex::new(HashMap::new()),
        }
    }

    pub fn registry(&self) -> &Arc<TurnRegistry> {
        &self.registry
    }

    pub async fn sync_durable_events(&self, session_id: &SessionId) {
        self.session_manager.sync_durable_events(session_id).await;
    }

    /// 提交新 turn。
    ///
    /// attach session 到 event_bus、修复遗留状态、调用 `Session::submit`、注册到 registry。
    /// 排队中的输入由 [`on_turn_completed`](Self::on_turn_completed) 在 turn 结束后按 FIFO 处理。
    pub async fn submit(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<(TurnId, TurnHandle), TurnScheduleError> {
        if self.registry.has_active(&session_id) {
            return Err(TurnScheduleError::TurnAlreadyRunning);
        }

        tracing::info!(session_id = %session_id, text_len = text.len(), "scheduler: submit turn");

        let session = self
            .session_manager
            .open(session_id.clone())
            .await
            .map_err(|e| TurnScheduleError::SessionNotFound(format!("{session_id}: {e}")))?;

        let turn_id = new_turn_id();
        let handle = session.submit(text, turn_id.clone()).await.map_err(|e| {
            tracing::error!(session_id = %session_id, error = %e, "session.submit failed");
            TurnScheduleError::Turn(e)
        })?;

        let session_arc = Arc::new(session);
        if !self.registry.register(
            session_id,
            turn_id.clone(),
            handle.abort_handle(),
            session_arc,
        ) {
            handle.abort();
            return Err(TurnScheduleError::TurnAlreadyRunning);
        }

        Ok((turn_id, handle))
    }

    /// 智能路由：有活跃 turn 则 inject，否则 submit。
    pub async fn submit_or_inject(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<SubmitOutcome, TurnScheduleError> {
        if self.registry.has_active(&session_id) {
            self.inject(&session_id, text).await?;
            Ok(SubmitOutcome::Injected)
        } else {
            let (turn_id, handle) = self.submit(session_id, text).await?;
            Ok(SubmitOutcome::Started { turn_id, handle })
        }
    }

    /// 通知后台任务已完成，在当前 turn 的**下一步**触发 agent 继续处理。
    ///
    /// ## 行为
    /// - 如果当前有活跃 turn → 立即 inject 消息，LLM 在下一步就能看到
    /// - 如果当前无活跃 turn → 启动新 turn 处理
    ///
    /// ## 使用场景
    /// 后台任务完成、compact 完成等需要立即让 LLM 感知结果的场景。
    pub async fn notify_step(
        &self,
        session_id: SessionId,
        source: &str,
    ) -> Result<SubmitOutcome, TurnScheduleError> {
        // 先处理已完成的子 agent——LLM 在下一步就能看到子 agent 完成结果
        self.process_child_completions(&session_id).await;

        let marker = format!(
            r#"<system type="background_completed" source="{}">"#,
            source
        );
        self.submit_or_inject(session_id, marker).await
    }

    /// 中止活跃 turn。
    ///
    /// 1. 级联停止并回收所有运行中的子（Agent）会话（深度优先）
    /// 2. 从 registry abort + remove
    /// 3. 清理 background tasks
    /// 4. 写终态事件
    ///
    /// 幂等性：多次调用同一 session 的 abort 是安全的，后续调用会静默成功。
    pub async fn abort(&self, session_id: &SessionId) -> Result<(), TurnScheduleError> {
        // 先停止并回收所有子会话，确保子会话的进程内资源和持久化状态被正确清理
        self.cascade_abort_children(session_id).await;

        // 快路径：registry 中有活跃 turn
        if let Some((turn_id, session)) = self.registry.abort_and_remove(session_id) {
            self.emit_turn_aborted(&turn_id, &session, session_id).await;
            return Ok(());
        }

        // 慢路径：无 registry entry，检查是否需要修复过期 phase
        // 先读取当前状态，避免与正在进行的 abort 冲突
        let session = match self.session_manager.open(session_id.clone()).await {
            Ok(s) => s,
            Err(_) => return Err(TurnScheduleError::SessionNotFound(session_id.to_string())),
        };

        let state = match session.read_model().await {
            Ok(s) => s,
            Err(e) => return Err(TurnScheduleError::Session(e)),
        };

        // 如果已经是终态，直接返回成功（幂等性）
        if matches!(
            state.phase,
            astrcode_core::event::Phase::Idle | astrcode_core::event::Phase::Error
        ) {
            return Ok(());
        }

        // 只有在确实有 stale 状态时才修复
        self.repair_stale(session_id).await
    }

    /// 清理 session 相关资源（delete/recycle 时由调用方在 session_manager 操作前调用）。
    ///
    /// Abort 活跃 turn + 清理 background tasks + 清理待处理消息队列。
    /// event_bus 的 detach 由 SessionManager::delete/recycle 自动处理。
    pub async fn cleanup(&self, session_id: &SessionId) {
        if let Some((turn_id, session)) = self.registry.abort_and_remove(session_id) {
            self.emit_turn_aborted(&turn_id, &session, session_id).await;
        }
        // 清理待处理消息队列，避免内存泄漏
        let removed = self.pending_queues.lock().remove(session_id);
        if removed.is_some() {
            tracing::info!(session_id = %session_id, "cleaned up pending message queue");
        }
    }

    /// 统一发送 turn aborted 终态事件 + 清理 bg tasks + sync durable。
    async fn emit_turn_aborted(&self, turn_id: &TurnId, session: &Session, session_id: &SessionId) {
        session
            .runtime()
            .background_tasks()
            .lock()
            .cleanup_session(session_id);

        if let Err(e) = session
            .emit_durable(
                Some(turn_id),
                EventPayload::TurnCompleted {
                    finish_reason: "aborted".into(),
                },
            )
            .await
        {
            tracing::error!(
                session_id = %session_id,
                turn_id = %turn_id,
                error = %e,
                "failed to write TurnCompleted during abort"
            );
        }
        session
            .emit_live(
                Some(turn_id),
                EventPayload::AgentRunCompleted {
                    reason: "aborted".into(),
                },
            )
            .await;
        self.session_manager.sync_durable_events(session_id).await;
    }

    /// 级联停止并回收所有运行中的子（Agent）会话。
    ///
    /// 深度优先：先 abort 所有孙子 turn，再 abort 子 turn，再统一等待。
    /// 事件写入由 `finalize_aborted_children` 统一处理——唯一一处写终态事件。
    async fn cascade_abort_children(&self, parent_sid: &SessionId) {
        let guards = self
            .collect_guards_deep(parent_sid, Duration::from_secs(10))
            .await;
        if guards.is_empty() {
            return;
        }
        self.finalize_aborted_children(&guards).await;
    }

    /// 显式栈遍历所有子孙 session，abort 每个 session 的直接子 turn。
    ///
    /// 不做递归——用栈模拟 DFS，深度无限制。
    /// 返回的 guards 按深度优先排列：grandchildren → children。
    async fn collect_guards_deep(
        &self,
        root_sid: &SessionId,
        timeout: Duration,
    ) -> Vec<Arc<astrcode_session::child_turn::ChildTurnGuard>> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut all_guards: Vec<Arc<astrcode_session::child_turn::ChildTurnGuard>> = Vec::new();
        let mut stack: Vec<SessionId> = vec![root_sid.clone()];

        // Phase 1: DFS 遍历，abort 所有层级的子 turn
        while let Some(sid) = stack.pop() {
            let session = match self.session_manager.open(sid).await {
                Ok(s) => s,
                Err(_) => continue,
            };
            let guards = session.runtime().abort_all_direct();
            if guards.is_empty() {
                continue;
            }
            for guard in &guards {
                stack.push(guard.child_session_id().clone());
            }
            all_guards.extend(guards);
        }

        // Phase 2: 统一等待所有 guard 完成（含超时）。先叶子后根。
        for guard in all_guards.iter().rev() {
            let result = tokio::time::timeout_at(deadline, guard.outcome()).await;
            if result.is_err() {
                tracing::warn!(
                    child_session_id = %guard.child_session_id(),
                    timeout_ms = timeout.as_millis(),
                    "cascade abort: child turn timed out"
                );
                // 写入 TimedOut 确保后续 outcome() 调用立即返回（如 finalize_aborted_children）
                guard.force_timeout();
            }
        }

        all_guards
    }

    /// 统一写所有被 abort 的子 session 的终态事件。
    async fn finalize_aborted_children(
        &self,
        guards: &[Arc<astrcode_session::child_turn::ChildTurnGuard>],
    ) {
        let session_manager = &self.session_manager;
        let scheduler = self;

        // 反转：先处理深层（grandchildren），再浅层（children）
        for guard in guards.iter().rev() {
            let child_sid = guard.child_session_id();
            let parent_sid = guard.parent_session_id();

            let error = match guard.outcome().await {
                ChildOutcome::TimedOut => "abort timed out",
                _ => "aborted",
            };
            ServerSessionOperations::write_agent_failed(
                session_manager,
                parent_sid,
                child_sid,
                error,
            )
            .await;
            ServerSessionOperations::recycle_child(
                session_manager,
                scheduler,
                parent_sid,
                child_sid,
            )
            .await;
        }
    }

    /// 处理父 session 中已完成的子 turn：回收、通知。
    ///
    /// 终态事件已由 guard 后台任务写入。本方法只处理 cleanup + notify。
    /// 幂等。无已完成子 turn 时为空操作。
    pub async fn process_child_completions(&self, parent_sid: &SessionId) {
        let parent_session = match self.session_manager.open(parent_sid.clone()).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(%parent_sid, error = %e, "process_child_completions: failed to open parent");
                return;
            },
        };
        let completed = parent_session.drain_completed_guards();
        for guard in completed {
            if guard.cleanup() == ChildCleanup::Recycle {
                ServerSessionOperations::recycle_child(
                    &self.session_manager,
                    self,
                    guard.parent_session_id(),
                    guard.child_session_id(),
                )
                .await;
            } else {
                // 非回收策略：仅清理 registry entry（已完成 turn 无需 abort）
                self.registry().remove(guard.child_session_id());
            }
            if let Some(notify_text) = guard.notify_text() {
                if let Err(e) = self
                    .submit_or_inject(guard.parent_session_id().clone(), notify_text.to_string())
                    .await
                {
                    tracing::warn!(
                        parent_session_id = %guard.parent_session_id(),
                        child_session_id = %guard.child_session_id(),
                        error = %e,
                        "child completion notification dropped"
                    );
                }
            }
        }
    }

    /// 向活跃 turn 注入中途消息。
    pub async fn inject(
        &self,
        session_id: &SessionId,
        text: String,
    ) -> Result<(), TurnScheduleError> {
        let turn_id = self
            .registry
            .active_turn_id(session_id)
            .ok_or(TurnScheduleError::NoActiveTurn)?;
        let session = self
            .registry
            .get_session(session_id)
            .ok_or(TurnScheduleError::NoActiveTurn)?;
        let message_id = new_message_id();
        session
            .emit_durable(
                Some(&turn_id),
                EventPayload::UserMessage { message_id, text },
            )
            .await
            .map_err(TurnScheduleError::EventEmit)?;
        Ok(())
    }


    /// 聚合修复：stale phase + stale background tasks + stale runs。
    pub async fn repair_stale(&self, session_id: &SessionId) -> Result<(), TurnScheduleError> {
        if self.registry.has_active(session_id) {
            return Ok(());
        }

        let session = self
            .session_manager
            .open(session_id.clone())
            .await
            .map_err(|e| TurnScheduleError::SessionNotFound(format!("{session_id}: {e}")))?;

        let state = session
            .read_model()
            .await
            .map_err(TurnScheduleError::Session)?;

        // Phase repair
        match repair_stale_phase_for_state(session_id, &session, &state).await {
            Ok(()) | Err(TurnScheduleError::NoActiveTurn) => {},
            Err(e) => return Err(e),
        }

        // Background tasks repair
        repair_stale_background_tasks_for_state(session_id, &session, &state).await?;

        // Stale runs repair
        repair_stale_runs_for_state(&self.registry, &session, &state).await?;

        self.session_manager.sync_durable_events(session_id).await;
        Ok(())
    }
}

// ─── Stale repair 内部函数 ─────────────────────────────────────────

async fn repair_stale_phase_for_state(
    session_id: &SessionId,
    session: &Session,
    state: &SessionReadModel,
) -> Result<(), TurnScheduleError> {
    if matches!(state.phase, Phase::Idle | Phase::Error) {
        return Err(TurnScheduleError::NoActiveTurn);
    }

    tracing::info!(
        session_id = %session_id,
        phase = ?state.phase,
        "repairing stale turn phase"
    );

    for pending in pending_requested_tool_calls(state) {
        let result = interrupted_tool_result(&pending.call_id);
        session
            .emit_durable(
                None,
                EventPayload::ToolCallCompleted {
                    call_id: pending.call_id.into(),
                    tool_name: pending.tool_name,
                    result,
                    arguments: String::new(),
                    arguments_json: None,
                },
            )
            .await
            .map_err(TurnScheduleError::EventEmit)?;
    }

    session
        .emit_durable(
            None,
            EventPayload::TurnCompleted {
                finish_reason: "interrupted".into(),
            },
        )
        .await
        .map_err(TurnScheduleError::EventEmit)?;
    session
        .emit_live(
            None,
            EventPayload::AgentRunCompleted {
                reason: "interrupted".into(),
            },
        )
        .await;

    Ok(())
}

async fn repair_stale_background_tasks_for_state(
    session_id: &SessionId,
    session: &Session,
    state: &SessionReadModel,
) -> Result<(), TurnScheduleError> {
    let active_tasks: std::collections::HashSet<_> = session
        .runtime()
        .background_tasks()
        .lock()
        .list_active(session_id)
        .into_iter()
        .collect();

    for (call_id, background) in &state.background_tool_calls {
        if background.completed || active_tasks.contains(&background.task_id) {
            continue;
        }
        let Some((tool_name, arguments_json)) = find_tool_call_history(state, call_id) else {
            tracing::warn!(
                session_id = %session_id,
                call_id = %call_id,
                task_id = %background.task_id,
                "stale background task has no matching tool call history"
            );
            continue;
        };
        let result = interrupted_background_tool_result(call_id.as_str(), &background.task_id);
        session
            .emit_durable(
                None,
                EventPayload::ToolCallCompleted {
                    call_id: call_id.clone(),
                    tool_name,
                    result,
                    arguments: arguments_json.to_string(),
                    arguments_json: Some(arguments_json),
                },
            )
            .await
            .map_err(TurnScheduleError::EventEmit)?;
    }
    Ok(())
}

async fn repair_stale_runs_for_state(
    registry: &TurnRegistry,
    session: &Session,
    state: &SessionReadModel,
) -> Result<(), TurnScheduleError> {
    for link in state
        .agent_sessions
        .iter()
        .filter(|link| link.status == astrcode_core::storage::AgentSessionStatus::Running)
    {
        if registry.has_active(&link.child_session_id) {
            continue;
        }
        session
            .emit_durable(
                None,
                astrcode_session::payload::agent_session_failed_payload(
                    link.child_session_id.clone(),
                    "interrupted".into(),
                ),
            )
            .await
            .map_err(TurnScheduleError::EventEmit)?;
    }
    Ok(())
}

// ─── 辅助函数 ─────────────────────────────────────────────────────

struct PendingRequestedToolCall {
    call_id: String,
    tool_name: String,
}

fn pending_requested_tool_calls(state: &SessionReadModel) -> Vec<PendingRequestedToolCall> {
    let mut remaining = state.pending_tool_calls.clone();
    let mut pending = Vec::new();

    for message in &state.messages {
        if message.role != LlmRole::Assistant {
            continue;
        }
        for content in &message.content {
            let LlmContent::ToolCall { call_id, name, .. } = content else {
                continue;
            };
            if remaining.remove(&ToolCallId::from(call_id.clone())) {
                pending.push(PendingRequestedToolCall {
                    call_id: call_id.clone(),
                    tool_name: name.clone(),
                });
            }
        }
    }

    pending
}

fn find_tool_call_history(
    state: &SessionReadModel,
    target_call_id: &ToolCallId,
) -> Option<(String, serde_json::Value)> {
    state.messages.iter().find_map(|message| {
        if message.role != LlmRole::Assistant {
            return None;
        }
        message.content.iter().find_map(|content| {
            let LlmContent::ToolCall {
                call_id,
                name,
                arguments,
            } = content
            else {
                return None;
            };
            (call_id == target_call_id.as_str()).then(|| (name.clone(), arguments.clone()))
        })
    })
}

fn interrupted_tool_result(call_id: &str) -> ToolResult {
    let content = "Tool execution interrupted before completion".to_string();
    ToolResult {
        call_id: call_id.to_string(),
        content: content.clone(),
        is_error: true,
        error: Some(content),
        metadata: Default::default(),
        duration_ms: None,
    }
}

fn interrupted_background_tool_result(call_id: &str, task_id: &BackgroundTaskId) -> ToolResult {
    let content = "Background task interrupted before completion".to_string();
    let mut metadata = BTreeMap::new();
    metadata.insert("task_id".into(), serde_json::json!(task_id.to_string()));
    ToolResult {
        call_id: call_id.to_string(),
        content: content.clone(),
        is_error: true,
        error: Some(content),
        metadata,
        duration_ms: None,
    }
}
