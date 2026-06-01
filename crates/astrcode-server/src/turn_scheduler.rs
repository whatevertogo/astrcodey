//! Active execution 唯一 owner：输入投递、队列、registry、completion 收口与 stale repair。
//!
//! # 输入投递（[`InputDelivery`]）
//!
//! 所有用户文本进入运行中的 session，应经 [`Self::deliver_input`] 并显式选择策略：
//!
//! | 策略 | running | idle | 典型调用方 |
//! |------|---------|------|------------|
//! | [`InputDelivery::StartNew`] | busy | 开 turn | 测试、必须独占 turn 的路径 |
//! | [`InputDelivery::InjectIfRunningElseStart`] | durable `UserMessage`（同 `turn_id`） | 开 turn | TUI `InjectMessage`、HTTP `POST .../inject`、`SessionOperations::inject_message`、子 session 完成通知 |
//! | [`InputDelivery::QueueIfRunningElseStart`] | `pending_queues` FIFO | 开 turn | HTTP/ACP `submit_input`（连发 prompt 不打断当前 turn） |
//!
//! **Steer** 不是第三种策略：它是 `Inject` 写 EventLog 后，由 `TurnRunner` 在下一 agent step
//! 将消息并入 LLM 上下文（见 `astrcode_session::steer`）。
//!
//! # Cancel / Abort 分层
//!
//! - **Abort**（用户/API）：[`Self::abort`] 表达「停止当前 turn」；先协作式 shutdown， grace period
//!   后 force kill，必要时跑 stale repair。
//! - **Shutdown**（机制）：[`Self::request_turn_shutdown`] 仅对本 session 发协作式停止信号。
//! - **Force kill**（机制）：[`Self::schedule_force_kill`] 在 grace 超时后硬杀 task 并写终态。
//! - **finish_reason**：`aborted` = 用户停止；`interrupted` = repair / 进程恢复。
//!
//! 对外只应使用 [`Self::deliver_input`] 与 [`Self::start_with_completion`]；低层
//! [`Self::start_execution`] 仅供本 crate 内部使用。

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use astrcode_core::{
    event::{EventPayload, Phase},
    storage::SessionReadModel,
    types::*,
};
use astrcode_session::{
    Session, SessionError, interrupted_tool_result,
    payload::{
        TURN_FINISH_ABORTED, TURN_FINISH_INTERRUPTED, agent_run_completed_payload,
        turn_completed_payload,
    },
    turn_handle::TurnHandle,
};
use parking_lot::Mutex;
use thiserror::Error;
use tokio::{sync::Mutex as AsyncMutex, task::JoinHandle};

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

/// 输入投递策略（见模块文档「输入投递」表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDelivery {
    /// 必须 idle；否则 busy。
    StartNew,
    /// running 时写入当前 turn 的 durable `UserMessage`（mid-turn steer）；idle 时 start。
    InjectIfRunningElseStart,
    /// running 时入队，当前 turn 结束后 FIFO 开新 turn；idle 时 start。
    QueueIfRunningElseStart,
}

/// 输入投递结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Started { turn_id: TurnId },
    Injected { turn_id: TurnId },
    Queued { queue_len: usize },
}

/// 告诉 scheduler 要收尾哪条 execution（输入参数，不是完成结果）。
pub struct CompletionParams {
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
type SessionDeliveryGate = Arc<AsyncMutex<()>>;
const FORCE_KILL_GRACE_MS: u64 = 1500;
const ABORT_WAIT_POLL_MS: u64 = 50;
const ABORT_WAIT_EXTRA_MS: u64 = 500;

#[derive(Clone)]
pub struct TurnScheduler {
    session_manager: Arc<SessionManager>,
    registry: Arc<TurnRegistry>,
    child_sessions: Arc<ChildSessionCoordinator>,
    pending_queues: Arc<Mutex<HashMap<SessionId, PendingQueue>>>,
    delivery_gates: Arc<AsyncMutex<HashMap<SessionId, SessionDeliveryGate>>>,
    detached_tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
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
            delivery_gates: Arc::new(AsyncMutex::new(HashMap::new())),
            detached_tasks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn session_delivery_gate(&self, session_id: &SessionId) -> SessionDeliveryGate {
        let mut gates = self.delivery_gates.lock().await;
        gates
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    fn track_detached_task(&self, handle: JoinHandle<()>) {
        self.detached_tasks.lock().push(handle);
    }

    /// 等待所有 detached completion / force-kill 任务结束（进程退出前调用）。
    pub async fn drain_detached_tasks(&self) {
        let handles: Vec<JoinHandle<()>> = self.detached_tasks.lock().drain(..).collect();
        for handle in handles {
            let _ = handle.await;
        }
    }

    fn release_delivery_gate(&self, session_id: &SessionId) {
        if let Ok(mut gates) = self.delivery_gates.try_lock() {
            gates.remove(session_id);
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
        let gate = self.session_delivery_gate(&session_id).await;
        let _guard = gate.lock().await;
        self.deliver_input_locked(session_id, text, delivery).await
    }

    async fn deliver_input_locked(
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
                if let Some(turn_id) = self.registry.active_turn_id(&session_id) {
                    match self.inject_internal(&session_id, text.clone()).await {
                        Ok(()) => Ok(DeliveryOutcome::Injected { turn_id }),
                        Err(TurnScheduleError::NoActiveTurn) => {
                            let started = self.start_execution(session_id.clone(), text).await?;
                            self.watch_detached_turn(
                                session_id,
                                started.turn_id.clone(),
                                started.handle,
                                "deliver_input:inject-fallback",
                            );
                            Ok(DeliveryOutcome::Started {
                                turn_id: started.turn_id,
                            })
                        },
                        Err(error) => Err(error),
                    }
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
        let gate = self.session_delivery_gate(&session_id).await;
        let _guard = gate.lock().await;
        self.start_execution(session_id, text).await
    }

    /// 低层启动：注册 registry 并返回 handle。调用方须走 [`Self::finish_and_maybe_start_next`]
    /// 收尾。
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
            handle.shutdown_handle(),
            session_arc,
        ) {
            handle.force_kill();
            return Err(TurnScheduleError::TurnAlreadyRunning);
        }

        Ok(StartedExecution { turn_id, handle })
    }

    /// Turn 收尾：registry 清理、sync、子 session drain；若队列非空且 session 空闲则启动下一条
    /// turn。
    pub async fn finish_and_maybe_start_next(
        &self,
        completion: CompletionParams,
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
        let gate = self.session_delivery_gate(&completion.session_id).await;
        let _guard = gate.lock().await;
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

    /// 若 [`Self::finish_and_maybe_start_next`] 已启动队列中的下一条 execution，挂上 detached
    /// watcher。
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
        let handle = tokio::spawn(async move {
            scheduler
                .run_detached_completion_watcher(session_id, turn_id, handle, source)
                .await;
        });
        self.track_detached_task(handle);
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
                .finish_and_maybe_start_next(CompletionParams {
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

    /// 中止活跃 turn（含级联子 session）。
    pub async fn abort(&self, session_id: &SessionId) -> Result<(), TurnScheduleError> {
        self.child_sessions
            .cascade_abort_children(self, session_id)
            .await;
        self.request_turn_shutdown(session_id).await?;

        if self.registry.has_active(session_id) {
            let deadline = tokio::time::Instant::now()
                + Duration::from_millis(FORCE_KILL_GRACE_MS + ABORT_WAIT_EXTRA_MS);
            while self.registry.has_active(session_id) && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(ABORT_WAIT_POLL_MS)).await;
            }
            if self.registry.has_active(session_id) {
                return Ok(());
            }
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

    /// 仅对本 session 发起协作式 shutdown（不级联子 session、不跑 stale repair）。
    pub(crate) async fn request_turn_shutdown(
        &self,
        session_id: &SessionId,
    ) -> Result<(), TurnScheduleError> {
        if let Some((turn_id, _session)) = self.registry.request_shutdown(session_id) {
            self.schedule_force_kill(session_id.clone(), turn_id);
        }
        Ok(())
    }

    /// 已完成 turn 的 registry / 队列清理（不 abort、不写 `TurnCompleted`）。
    ///
    /// 用于 child session 正常结束后的 recycle；turn 终态已由 runner 写入事件日志。
    pub async fn release_completed_execution(&self, session_id: &SessionId) {
        self.registry.remove(session_id);
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
        if self.registry.active_is_finished(session_id) {
            self.registry.remove(session_id);
        } else if let Some((turn_id, session)) = self.registry.force_kill_current(session_id) {
            self.emit_turn_aborted(&turn_id, &session, session_id).await;
        }
        let removed = self.pending_queues.lock().remove(session_id);
        if removed.is_some() {
            tracing::info!(session_id = %session_id, "cleaned up pending message queue");
        }
        self.release_delivery_gate(session_id);
    }

    fn schedule_force_kill(&self, session_id: SessionId, turn_id: TurnId) {
        let scheduler = self.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(FORCE_KILL_GRACE_MS)).await;
            let Some((removed_turn_id, session)) = scheduler
                .registry
                .force_kill_and_remove_if_running(&session_id, &turn_id)
            else {
                return;
            };
            tracing::warn!(
                session_id = %session_id,
                turn_id = %removed_turn_id,
                "turn did not stop after cooperative shutdown; forced kill"
            );
            scheduler
                .emit_turn_aborted(&removed_turn_id, &session, &session_id)
                .await;
        });
        self.track_detached_task(handle);
    }

    async fn emit_turn_aborted(&self, turn_id: &TurnId, session: &Session, session_id: &SessionId) {
        let tool_protocol_settled = match session.read_model().await {
            Ok(state) => {
                match emit_interrupted_tool_results(session, &state, Some(turn_id)).await {
                    Ok(_) => true,
                    Err(e) => {
                        tracing::warn!(
                            session_id = %session_id,
                            turn_id = %turn_id,
                            error = %e,
                            "failed to settle pending tool calls during abort"
                        );
                        false
                    },
                }
            },
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    turn_id = %turn_id,
                    error = %e,
                    "failed to read session state during abort"
                );
                false
            },
        };

        if tool_protocol_settled {
            if let Err(e) = emit_turn_aborted_context(session, Some(turn_id)).await {
                tracing::warn!(
                    session_id = %session_id,
                    turn_id = %turn_id,
                    error = %e,
                    "failed to write turn-aborted provider context"
                );
            }
        }

        if let Err(e) = session
            .emit_durable(Some(turn_id), turn_completed_payload(TURN_FINISH_ABORTED))
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
                agent_run_completed_payload(TURN_FINISH_ABORTED),
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

        if matches!(state.phase, Phase::Idle | Phase::Error) {
            repair_incomplete_tool_protocol_for_state(&session, &state).await?;
        } else {
            repair_stale_phase_for_state(session_id, &session, &state).await?;
        }

        repair_stale_runs_for_state(self, &session, &state).await?;

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

    emit_interrupted_tool_results(session, state, None).await?;
    emit_turn_aborted_context(session, None).await?;

    session
        .emit_durable(None, turn_completed_payload(TURN_FINISH_INTERRUPTED))
        .await
        .map_err(TurnScheduleError::EventEmit)?;
    session
        .emit_live(None, agent_run_completed_payload(TURN_FINISH_INTERRUPTED))
        .await;

    Ok(())
}

async fn repair_incomplete_tool_protocol_for_state(
    session: &Session,
    state: &SessionReadModel,
) -> Result<(), TurnScheduleError> {
    let interrupted = emit_interrupted_tool_results(session, state, None).await?;
    if interrupted > 0 {
        emit_turn_aborted_context(session, None).await?;
    }
    Ok(())
}

async fn emit_interrupted_tool_results(
    session: &Session,
    state: &SessionReadModel,
    turn_id: Option<&TurnId>,
) -> Result<usize, TurnScheduleError> {
    let mut emitted = 0;
    for pending in state.tool_calls_needing_interruption() {
        let result = interrupted_tool_result(
            pending.call_id.clone(),
            &pending.tool_name,
            std::time::Duration::ZERO,
        );
        session
            .emit_durable(
                turn_id,
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
        emitted += 1;
    }
    Ok(emitted)
}

async fn emit_turn_aborted_context(
    session: &Session,
    turn_id: Option<&TurnId>,
) -> Result<(), TurnScheduleError> {
    session
        .emit_durable(turn_id, EventPayload::TurnAbortedContext)
        .await
        .map_err(TurnScheduleError::EventEmit)?;
    Ok(())
}

async fn repair_stale_runs_for_state(
    scheduler: &TurnScheduler,
    session: &Session,
    state: &SessionReadModel,
) -> Result<(), TurnScheduleError> {
    for link in state
        .agent_sessions
        .iter()
        .filter(|link| link.status == astrcode_core::storage::AgentSessionStatus::Running)
    {
        let child_sid = &link.child_session_id;
        if scheduler.registry().has_active(child_sid) {
            if let Err(e) = scheduler.request_turn_shutdown(child_sid).await {
                tracing::debug!(
                    parent_session_id = %session.id(),
                    child_session_id = %child_sid,
                    error = %e,
                    "repair_stale: child abort returned error"
                );
            }
            scheduler.abort_and_cleanup(child_sid).await;
            continue;
        }
        session
            .emit_durable(
                None,
                astrcode_session::payload::agent_session_failed_payload(
                    child_sid.clone(),
                    "interrupted".into(),
                ),
            )
            .await
            .map_err(TurnScheduleError::EventEmit)?;
    }
    Ok(())
}
