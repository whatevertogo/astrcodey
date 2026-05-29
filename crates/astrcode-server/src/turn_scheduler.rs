//! Active execution 唯一 owner：输入投递、队列、registry、completion 收口与 stale repair。
//!
//! 对外只应使用 [`Self::deliver_input`] 与 [`Self::start_with_completion`]；低层
//! [`Self::start_execution`] 仅供本 crate 内部使用。

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use astrcode_core::{
    event::{EventPayload, Phase},
    llm::{LlmContent, LlmRole},
    storage::SessionReadModel,
    tool::ToolResult,
    types::*,
};
use astrcode_session::{Session, SessionError, turn_handle::TurnHandle};
use parking_lot::Mutex;
use thiserror::Error;

use crate::{
    child_session::ChildSessionCoordinator, session_manager::SessionManager,
    turn_registry::TurnRegistry,
};

/// Turn 调度层错误（会话是否存在、是否已有 turn 在跑等）。
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

/// 输入投递策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDelivery {
    /// 必须 idle；否则 busy。
    StartNew,
    /// running 时 inject；idle 时 start。
    InjectIfRunningElseStart,
    /// running 时入队；idle 时 start。
    QueueIfRunningElseStart,
}

/// 输入投递结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Started { turn_id: TurnId },
    Injected { turn_id: TurnId },
    Queued { queue_len: usize },
}

pub struct ExecutionCompletion {
    pub session_id: SessionId,
    pub turn_id: TurnId,
}

pub struct StartedExecution {
    pub turn_id: TurnId,
    pub handle: TurnHandle,
}

/// 对外 execution 查询视图（durable phase + 热路径 registry + 队列深度）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionExecutionView {
    pub phase: Phase,
    pub active_turn_id: Option<TurnId>,
    pub queued_inputs: usize,
}

pub(crate) struct PendingMessage {
    text: String,
}

type PendingQueue = VecDeque<PendingMessage>;

#[derive(Clone)]
pub struct TurnScheduler {
    session_manager: Arc<SessionManager>,
    registry: Arc<TurnRegistry>,
    child_sessions: Arc<ChildSessionCoordinator>,
    pending_queues: Arc<Mutex<HashMap<SessionId, PendingQueue>>>,
}

impl TurnScheduler {
    pub fn new(
        session_manager: Arc<SessionManager>,
        registry: Arc<TurnRegistry>,
        child_sessions: Arc<ChildSessionCoordinator>,
    ) -> Self {
        Self {
            session_manager,
            registry,
            child_sessions,
            pending_queues: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn registry(&self) -> &Arc<TurnRegistry> {
        &self.registry
    }

    pub async fn sync_durable_events(&self, session_id: &SessionId) {
        self.session_manager.sync_durable_events(session_id).await;
    }

    /// 统一的 execution 状态查询。
    pub async fn execution_view(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionExecutionView, TurnScheduleError> {
        let active_turn_id = self.registry.active_turn_id(session_id);
        let phase = self
            .session_manager
            .read_model(session_id)
            .await
            .map_err(|e| TurnScheduleError::SessionNotFound(format!("{session_id}: {e}")))?
            .phase;
        let queued_inputs = self
            .pending_queues
            .lock()
            .get(session_id)
            .map(|q| q.len())
            .unwrap_or(0);
        Ok(SessionExecutionView {
            phase,
            active_turn_id,
            queued_inputs,
        })
    }

    /// 输入投递的唯一 public gateway。
    pub async fn deliver_input(
        &self,
        session_id: SessionId,
        text: String,
        delivery: InputDelivery,
    ) -> Result<DeliveryOutcome, TurnScheduleError> {
        match delivery {
            InputDelivery::StartNew => {
                let started = self.start_execution(session_id.clone(), text).await?;
                self.watch_detached_turn(
                    session_id,
                    started.turn_id.clone(),
                    started.handle,
                    "deliver_input:start",
                );
                Ok(DeliveryOutcome::Started {
                    turn_id: started.turn_id,
                })
            },
            InputDelivery::InjectIfRunningElseStart => {
                if self.registry.has_active(&session_id) {
                    let turn_id = self
                        .registry
                        .active_turn_id(&session_id)
                        .expect("has_active implies active_turn_id");
                    self.inject_internal(&session_id, text).await?;
                    Ok(DeliveryOutcome::Injected { turn_id })
                } else {
                    let started = self.start_execution(session_id.clone(), text).await?;
                    self.watch_detached_turn(
                        session_id,
                        started.turn_id.clone(),
                        started.handle,
                        "deliver_input:inject",
                    );
                    Ok(DeliveryOutcome::Started {
                        turn_id: started.turn_id,
                    })
                }
            },
            InputDelivery::QueueIfRunningElseStart => {
                if !self.registry.has_active(&session_id) {
                    let started = self.start_execution(session_id.clone(), text).await?;
                    self.watch_detached_turn(
                        session_id,
                        started.turn_id.clone(),
                        started.handle,
                        "deliver_input:queue",
                    );
                    return Ok(DeliveryOutcome::Started {
                        turn_id: started.turn_id,
                    });
                }
                let mut queues = self.pending_queues.lock();
                let queue = queues.entry(session_id.clone()).or_default();
                queue.push_back(PendingMessage { text });
                let queue_len = queue.len();
                drop(queues);
                tracing::info!(
                    session_id = %session_id,
                    queue_len = queue_len,
                    "message queued for next turn"
                );
                Ok(DeliveryOutcome::Queued { queue_len })
            },
        }
    }

    /// 启动新 turn 并返回 handle（需要等待结果时用 [`Self::start_with_completion`]）。
    pub async fn start_with_completion(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<StartedExecution, TurnScheduleError> {
        self.start_execution(session_id, text).await
    }

    /// 低层启动：注册 registry 并返回 handle。调用方须走 [`Self::finish_execution`] 收尾。
    pub(crate) async fn start_execution(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<StartedExecution, TurnScheduleError> {
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

        Ok(StartedExecution { turn_id, handle })
    }

    /// 唯一的 turn completion 收口：registry 清理、sync、子 session drain、队列出队。
    pub async fn finish_execution(
        &self,
        completion: ExecutionCompletion,
    ) -> Option<StartedExecution> {
        self.registry
            .remove_if_matches(&completion.session_id, &completion.turn_id);
        self.sync_durable_events(&completion.session_id).await;
        self.child_sessions
            .drain_completed(self, &completion.session_id)
            .await;

        if self.registry.has_active(&completion.session_id) {
            return None;
        }
        let text = self.dequeue_next_pending(&completion.session_id)?;
        tracing::info!(
            session_id = %completion.session_id,
            "auto-submitting next queued message for new turn"
        );
        match self
            .start_execution(completion.session_id.clone(), text)
            .await
        {
            Ok(started) => Some(started),
            Err(e) => {
                tracing::warn!(
                    session_id = %completion.session_id,
                    error = %e,
                    "failed to auto-submit queued message"
                );
                None
            },
        }
    }

    /// 若 `finish_execution` 已启动队列中的下一条 execution，挂上 detached watcher。
    pub(crate) fn watch_queued_if_any(
        &self,
        session_id: SessionId,
        next: Option<StartedExecution>,
    ) {
        let Some(StartedExecution { turn_id, handle }) = next else {
            return;
        };
        self.watch_detached_turn(session_id, turn_id, handle, "queued");
    }

    fn watch_detached_turn(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        handle: TurnHandle,
        source: &'static str,
    ) {
        let scheduler = self.clone();
        tokio::spawn(async move {
            scheduler
                .run_detached_completion_watcher(session_id, turn_id, handle, source)
                .await;
        });
    }

    async fn run_detached_completion_watcher(
        &self,
        session_id: SessionId,
        mut turn_id: TurnId,
        mut handle: TurnHandle,
        source: &'static str,
    ) {
        loop {
            let wait_result = handle.wait().await;
            if wait_result.is_none() {
                tracing::warn!(
                    session_id = %session_id,
                    turn_id = %turn_id,
                    source,
                    "detached turn task ended without completion"
                );
            }

            let next = self
                .finish_execution(ExecutionCompletion {
                    session_id: session_id.clone(),
                    turn_id: turn_id.clone(),
                })
                .await;

            let Some(StartedExecution {
                turn_id: next_turn_id,
                handle: next_handle,
            }) = next
            else {
                break;
            };
            turn_id = next_turn_id;
            handle = next_handle;
        }
    }

    /// 中止活跃 turn。
    pub async fn abort(&self, session_id: &SessionId) -> Result<(), TurnScheduleError> {
        self.child_sessions
            .cascade_abort_children(self, session_id)
            .await;

        if let Some((turn_id, session)) = self.registry.abort_and_remove(session_id) {
            self.emit_turn_aborted(&turn_id, &session, session_id).await;
            return Ok(());
        }

        let session = match self.session_manager.open(session_id.clone()).await {
            Ok(s) => s,
            Err(_) => return Err(TurnScheduleError::SessionNotFound(session_id.to_string())),
        };

        let state = match session.read_model().await {
            Ok(s) => s,
            Err(e) => return Err(TurnScheduleError::Session(e)),
        };

        if matches!(state.phase, Phase::Idle | Phase::Error) {
            return Ok(());
        }

        self.repair_stale(session_id).await
    }

    /// 已完成 turn 的 registry / 队列 / 后台任务清理（不 abort、不写 `TurnCompleted`）。
    ///
    /// 用于 child session 正常结束后的 recycle；turn 终态已由 runner 写入事件日志。
    pub async fn release_completed_execution(&self, session_id: &SessionId) {
        self.registry.remove(session_id);
        if let Ok(session) = self.session_manager.open(session_id.clone()).await {
            session
                .runtime()
                .background_tasks()
                .lock()
                .cleanup_session(session_id);
        }
        let removed = self.pending_queues.lock().remove(session_id);
        if removed.is_some() {
            tracing::debug!(
                session_id = %session_id,
                "released pending message queue after completed turn"
            );
        }
    }

    /// 中止或删除 session 时的强制清理（可能 abort 活跃 turn 并写 `TurnCompleted(aborted)`）。
    pub async fn abort_and_cleanup(&self, session_id: &SessionId) {
        if let Some((turn_id, session)) = self.registry.abort_and_remove(session_id) {
            self.emit_turn_aborted(&turn_id, &session, session_id).await;
        } else if let Ok(session) = self.session_manager.open(session_id.clone()).await {
            session
                .runtime()
                .background_tasks()
                .lock()
                .cleanup_session(session_id);
        }
        let removed = self.pending_queues.lock().remove(session_id);
        if removed.is_some() {
            tracing::info!(session_id = %session_id, "cleaned up pending message queue");
        }
    }

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

    async fn inject_internal(
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

        match repair_stale_phase_for_state(session_id, &session, &state).await {
            Ok(()) | Err(TurnScheduleError::NoActiveTurn) => {},
            Err(e) => return Err(e),
        }

        repair_stale_background_tasks_for_state(session_id, &session, &state).await?;
        repair_stale_runs_for_state(&self.registry, &session, &state).await?;

        self.session_manager.sync_durable_events(session_id).await;
        Ok(())
    }

    // ─── Pending Input Queue ──────────────────────────────────────

    fn dequeue_next_pending(&self, session_id: &SessionId) -> Option<String> {
        let mut queues = self.pending_queues.lock();
        let queue = queues.get_mut(session_id)?;
        let text = queue.pop_front()?.text;
        if queue.is_empty() {
            queues.remove(session_id);
        }
        if text.is_empty() { None } else { Some(text) }
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
        let Some((tool_name, _arguments_json)) = find_tool_call_history(state, call_id) else {
            tracing::warn!(
                session_id = %session_id,
                call_id = %call_id,
                task_id = %background.task_id,
                "stale background task has no matching tool call history"
            );
            continue;
        };
        let summary = format!(
            "Background task interrupted before completion (task: {})",
            background.task_id
        );
        session
            .emit_durable(
                None,
                EventPayload::BackgroundTaskNotification {
                    task_id: background.task_id.clone(),
                    call_id: call_id.clone(),
                    tool_name,
                    summary,
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

struct PendingRequestedToolCall {
    call_id: String,
    tool_name: String,
}

fn pending_requested_tool_calls(state: &SessionReadModel) -> Vec<PendingRequestedToolCall> {
    let mut remaining = state.pending_tool_calls.clone();
    let mut pending = Vec::new();

    for message in &state.messages {
        if message.message.role != LlmRole::Assistant {
            continue;
        }
        for content in &message.message.content {
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
        if message.message.role != LlmRole::Assistant {
            return None;
        }
        message.message.content.iter().find_map(|content| {
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
