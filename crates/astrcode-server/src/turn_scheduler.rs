//! TurnScheduler — 统一的 turn 生命周期服务。
//!
//! 主会话和子会话共用同一条 submit/abort 路径。取代了之前分散在
//! `CommandHandler.active_turns` 和 `SessionManager.ActiveExecutionIndex` 的两套编排。

use std::{collections::BTreeMap, sync::Arc};

use astrcode_core::{
    event::{EventPayload, Phase},
    llm::{LlmContent, LlmRole},
    storage::SessionReadModel,
    tool::ToolResult,
    types::*,
};
use astrcode_session::{Session, turn_handle::TurnHandle};
use thiserror::Error;

use crate::{session_manager::SessionManager, turn_registry::TurnRegistry};

#[derive(Debug, Error)]
pub enum TurnError {
    #[error("A turn is already running")]
    TurnAlreadyRunning,
    #[error("No active turn")]
    NoActiveTurn,
    #[error("Session not found: {0}")]
    SessionNotFound(String),
    #[error("Session manager error: {0}")]
    SessionManager(#[from] crate::session_manager::SessionManagerError),
    #[error("Session error: {0}")]
    Session(String),
    #[error("Event emit failed: {0}")]
    EventEmit(String),
}

pub enum SubmitOutcome {
    Started { turn_id: TurnId, handle: TurnHandle },
    Injected,
}

pub struct TurnScheduler {
    session_manager: Arc<SessionManager>,
    registry: Arc<TurnRegistry>,
}

impl TurnScheduler {
    pub fn new(session_manager: Arc<SessionManager>, registry: Arc<TurnRegistry>) -> Self {
        Self {
            session_manager,
            registry,
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
    pub async fn submit(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<(TurnId, TurnHandle), TurnError> {
        if self.registry.has_active(&session_id) {
            return Err(TurnError::TurnAlreadyRunning);
        }

        tracing::info!(session_id = %session_id, text_len = text.len(), "scheduler: submit turn");

        let session = self
            .session_manager
            .open(session_id.clone())
            .await
            .map_err(|e| TurnError::SessionNotFound(format!("{session_id}: {e}")))?;

        let turn_id = new_turn_id();
        let handle = session.submit(text, turn_id.clone()).await.map_err(|e| {
            // submit 失败时 session 内部可能已经写了 TurnStarted，
            // 但 error + TurnCompleted 补写也在 session.submit 内处理。
            // 这里只报告错误。
            tracing::error!(session_id = %session_id, error = %e, "session.submit failed");
            TurnError::Session(format!("submit: {e}"))
        })?;

        let session_arc = Arc::new(session);
        if !self.registry.register(
            session_id,
            turn_id.clone(),
            handle.abort_handle(),
            session_arc,
        ) {
            // 极端情况：并发 register。abort 刚拿到的 handle。
            handle.abort();
            return Err(TurnError::TurnAlreadyRunning);
        }

        Ok((turn_id, handle))
    }

    /// 智能路由：有活跃 turn 则 inject，否则 submit。
    pub async fn submit_or_inject(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<SubmitOutcome, TurnError> {
        if self.registry.has_active(&session_id) {
            self.inject(&session_id, text).await?;
            Ok(SubmitOutcome::Injected)
        } else {
            let (turn_id, handle) = self.submit(session_id, text).await?;
            Ok(SubmitOutcome::Started { turn_id, handle })
        }
    }

    /// 中止活跃 turn。
    ///
    /// 1. 从 registry abort + remove
    /// 2. 清理 background tasks
    /// 3. 写终态事件
    ///
    /// 幂等性：多次调用同一 session 的 abort 是安全的，后续调用会静默成功。
    pub async fn abort(&self, session_id: &SessionId) -> Result<(), TurnError> {
        // 快路径：registry 中有活跃 turn
        if let Some((turn_id, session)) = self.registry.abort_and_remove(session_id) {
            self.emit_turn_aborted(&turn_id, &session, session_id).await;
            return Ok(());
        }

        // 慢路径：无 registry entry，检查是否需要修复过期 phase
        // 先读取当前状态，避免与正在进行的 abort 冲突
        let session = match self.session_manager.open(session_id.clone()).await {
            Ok(s) => s,
            Err(_) => return Err(TurnError::SessionNotFound(session_id.to_string())),
        };

        let state = match session.read_model().await {
            Ok(s) => s,
            Err(e) => return Err(TurnError::Session(format!("read session: {e}"))),
        };

        // 如果已经是终态，直接返回成功（幂等性）
        if matches!(state.phase, astrcode_core::event::Phase::Idle | astrcode_core::event::Phase::Error) {
            return Ok(());
        }

        // 只有在确实有 stale 状态时才修复
        self.repair_stale(session_id).await
    }

    /// 清理 session 相关资源（delete/recycle 时由调用方在 session_manager 操作前调用）。
    ///
    /// Abort 活跃 turn + 清理 background tasks。
    /// event_bus 的 detach 由 SessionManager::delete/recycle 自动处理。
    pub async fn cleanup(&self, session_id: &SessionId) {
        if let Some((turn_id, session)) = self.registry.abort_and_remove(session_id) {
            self.emit_turn_aborted(&turn_id, &session, session_id).await;
        }
    }

    /// 统一发送 turn aborted 终态事件 + 清理 bg tasks + sync durable。
    async fn emit_turn_aborted(&self, turn_id: &TurnId, session: &Session, session_id: &SessionId) {
        session
            .runtime()
            .background_tasks()
            .lock()
            .cleanup_session(session_id);

        let _ = session
            .emit_durable(
                Some(turn_id),
                EventPayload::TurnCompleted {
                    finish_reason: "aborted".into(),
                },
            )
            .await;
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

    /// 向活跃 turn 注入中途消息。
    pub async fn inject(&self, session_id: &SessionId, text: String) -> Result<(), TurnError> {
        let turn_id = self
            .registry
            .active_turn_id(session_id)
            .ok_or(TurnError::NoActiveTurn)?;
        let session = self
            .registry
            .get_session(session_id)
            .ok_or(TurnError::NoActiveTurn)?;
        let message_id = new_message_id();
        session
            .emit_durable(
                Some(&turn_id),
                EventPayload::UserMessage { message_id, text },
            )
            .await
            .map_err(|e| TurnError::EventEmit(format!("inject message: {e}")))?;
        Ok(())
    }


    /// 聚合修复：stale phase + stale background tasks + stale runs。
    pub async fn repair_stale(&self, session_id: &SessionId) -> Result<(), TurnError> {
        if self.registry.has_active(session_id) {
            return Ok(());
        }

        let session = self
            .session_manager
            .open(session_id.clone())
            .await
            .map_err(|e| TurnError::SessionNotFound(format!("{session_id}: {e}")))?;

        let state = session
            .read_model()
            .await
            .map_err(|e| TurnError::Session(format!("read session {session_id}: {e}")))?;

        // Phase repair
        match repair_stale_phase_for_state(session_id, &session, &state).await {
            Ok(()) | Err(TurnError::NoActiveTurn) => {},
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
) -> Result<(), TurnError> {
    if matches!(state.phase, Phase::Idle | Phase::Error) {
        return Err(TurnError::NoActiveTurn);
    }

    tracing::info!(
        session_id = %session_id,
        phase = ?state.phase,
        "repairing stale turn phase"
    );

    for pending in pending_requested_tool_calls(state) {
        session
            .emit_durable(
                None,
                EventPayload::ToolCallCompleted {
                    call_id: pending.call_id.clone().into(),
                    tool_name: pending.tool_name,
                    result: interrupted_tool_result(&pending.call_id),
                    arguments: String::new(),
                    arguments_json: None,
                },
            )
            .await
            .map_err(|e| {
                TurnError::EventEmit(format!("emit ToolCallCompleted during repair: {e}"))
            })?;
    }

    session
        .emit_durable(
            None,
            EventPayload::TurnCompleted {
                finish_reason: "interrupted".into(),
            },
        )
        .await
        .map_err(|e| TurnError::EventEmit(format!("emit TurnCompleted during repair: {e}")))?;
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
) -> Result<(), TurnError> {
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
            .map_err(|e| TurnError::EventEmit(format!("emit stale background completion: {e}")))?;
    }
    Ok(())
}

async fn repair_stale_runs_for_state(
    registry: &TurnRegistry,
    session: &Session,
    state: &SessionReadModel,
) -> Result<(), TurnError> {
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
                EventPayload::AgentSessionFailed {
                    child_session_id: link.child_session_id.clone(),
                    final_session_id: link.child_session_id.clone(),
                    error: "interrupted".into(),
                },
            )
            .await
            .map_err(|e| TurnError::EventEmit(format!("emit stale child failure: {e}")))?;
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
