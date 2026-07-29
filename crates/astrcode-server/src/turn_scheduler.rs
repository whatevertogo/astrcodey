//! Active execution 唯一 owner：输入投递、队列、registry、completion 收口与 stale repair。
//!
//! # 输入投递（[`InputDelivery`]）
//!
//! 所有用户文本进入运行中的 session，应经 [`Self::deliver_input`] 并显式选择策略：
//!
//! | 策略 | running | idle | 典型调用方 |
//! |------|---------|------|------------|
//! | [`InputDelivery::StartNew`] | busy | 开 turn | 测试、必须独占 turn 的路径 |
//! | [`InputDelivery::InjectOnly`] | durable `UserMessage`（同 `turn_id`） | no active turn | HTTP `POST .../inject` |
//! | [`InputDelivery::InjectIfRunningElseStart`] | durable `UserMessage`（同 `turn_id`） | 开 turn | `SessionOperations::inject_message`、子 session 完成通知 |
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
//! `start_execution_locked` 仅供本 crate 内部持有 per-session gate 后调用。

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Weak},
    time::Duration,
};

use astrcode_core::{
    event::{DurableEventPayload, Phase},
    message_attachment::MessageAttachment,
    types::*,
};
use astrcode_session::{
    InterruptedToolOutcome, Session, SessionError, TurnHandle, emit_interrupted_tool_results,
    emit_turn_aborted_context,
    payload::{
        TURN_FINISH_ABORTED, TURN_FINISH_INTERRUPTED, agent_run_completed_payload,
        turn_completed_payload,
    },
};
use astrcode_session_projection::{AgentSessionStatus, SessionReadModel};
use parking_lot::Mutex;
use thiserror::Error;
use tokio::{
    sync::{Mutex as AsyncMutex, OwnedMutexGuard, broadcast, oneshot},
    task::JoinHandle,
};

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
    #[error("pending input queue is full ({max} items)")]
    QueueFull { max: usize },
    #[error("prompt text is too large ({actual} bytes, max {max} bytes)")]
    InputTooLarge { actual: usize, max: usize },
    #[error("Session not found: {0}")]
    SessionNotFound(String),
    #[error(transparent)]
    SessionManager(#[from] crate::session_manager::SessionManagerError),
    #[error(transparent)]
    Session(SessionError),
    #[error(transparent)]
    Turn(#[from] astrcode_session::TurnError),
    #[error("event emit failed")]
    EventEmit(#[source] SessionError),
}

/// 输入投递策略（见模块文档「输入投递」表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDelivery {
    /// 必须 idle；否则 busy。
    StartNew,
    /// 必须 running；否则返回 [`TurnScheduleError::NoActiveTurn`]。
    InjectOnly,
    /// running 时写入当前 turn 的 durable `UserMessage`（mid-turn steer）；idle 时 start。
    InjectIfRunningElseStart,
    /// running 时入队，当前 turn 结束后 FIFO 开新 turn；idle 时 start。
    QueueIfRunningElseStart,
    /// 先中断当前 turn，再以新输入启动 turn。
    InterruptAndStart,
}

/// 输入投递结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Started { turn_id: TurnId },
    Injected { turn_id: TurnId },
    Queued { queue_len: usize },
}

pub struct StartedExecution {
    pub turn_id: TurnId,
    pub handle: TurnHandle,
}

/// Turn 完成结果，供等待请求完成的传输层与空闲 recap 监听器使用。
#[derive(Debug, Clone)]
pub(crate) enum TurnCompletion {
    Completed {
        finish_reason: String,
    },
    Failed {
        error: String,
    },
    /// completion 通道关闭或 task 异常，未拿到 turn 结果。
    Dropped,
}

#[derive(Debug, Clone)]
pub(crate) struct TurnCompletionEvent {
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub completion: TurnCompletion,
}

/// 对外 execution 查询视图（durable session snapshot + 热路径 registry + 队列深度）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionExecutionView {
    pub phase: Phase,
    pub active_turn_id: Option<TurnId>,
    pub queued_inputs: usize,
    pub message_count: usize,
}

/// 用户输入（文本 + 可选附件）。
#[derive(Debug, Clone)]
pub struct PromptInput {
    pub text: String,
    pub attachments: Vec<MessageAttachment>,
}

impl PromptInput {
    pub fn text_only(text: String) -> Self {
        Self {
            text,
            attachments: Vec::new(),
        }
    }

    pub fn can_submit(&self) -> bool {
        !self.text.trim().is_empty() || !self.attachments.is_empty()
    }
}

impl From<String> for PromptInput {
    fn from(text: String) -> Self {
        Self::text_only(text)
    }
}

impl From<&str> for PromptInput {
    fn from(text: &str) -> Self {
        Self::text_only(text.to_string())
    }
}

type PendingQueue = VecDeque<PromptInput>;
type SessionDeliveryGate = Arc<AsyncMutex<SessionDeliveryState>>;
pub const MAX_PENDING_INPUTS_PER_SESSION: usize = 32;
pub const MAX_PROMPT_TEXT_BYTES: usize = 1024 * 1024;
const FORCE_KILL_GRACE_MS: u64 = 1500;
const ABORT_WAIT_POLL_MS: u64 = 50;
const ABORT_WAIT_EXTRA_MS: u64 = 500;
const INITIAL_DELIVERY_GATE_PRUNE_AT: usize = 128;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SessionDeliveryState {
    Open,
    Closing,
}

/// 持有一个 session 的输入/命令操作权，防止检查状态后被并发请求改变。
pub(crate) struct SessionOperationGuard {
    session_id: SessionId,
    _state: OwnedMutexGuard<SessionDeliveryState>,
}

impl SessionOperationGuard {
    pub(crate) fn session_id(&self) -> &SessionId {
        &self.session_id
    }
}

pub(crate) enum CompletedRecycleOutcome {
    Recycled,
    StaleCompletion,
}

impl SessionDeliveryState {
    fn ensure_open(self, session_id: &SessionId) -> Result<(), TurnScheduleError> {
        if self == Self::Closing {
            return Err(TurnScheduleError::SessionNotFound(format!(
                "{session_id}: session is closing"
            )));
        }
        Ok(())
    }
}

struct DeliveryGateRegistry {
    gates: HashMap<SessionId, Weak<AsyncMutex<SessionDeliveryState>>>,
    next_prune_at: usize,
}

impl Default for DeliveryGateRegistry {
    fn default() -> Self {
        Self {
            gates: HashMap::new(),
            next_prune_at: INITIAL_DELIVERY_GATE_PRUNE_AT,
        }
    }
}

#[derive(Clone)]
pub struct TurnScheduler {
    session_manager: Arc<SessionManager>,
    registry: Arc<TurnRegistry>,
    child_sessions: Arc<ChildSessionCoordinator>,
    pending_queues: Arc<Mutex<HashMap<SessionId, PendingQueue>>>,
    delivery_gates: Arc<Mutex<DeliveryGateRegistry>>,
    detached_tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
    completion_events: broadcast::Sender<TurnCompletionEvent>,
}

impl TurnScheduler {
    pub fn new(
        session_manager: Arc<SessionManager>,
        registry: Arc<TurnRegistry>,
        child_sessions: Arc<ChildSessionCoordinator>,
    ) -> Self {
        let (completion_events, _) = broadcast::channel(256);
        Self {
            session_manager,
            registry,
            child_sessions,
            pending_queues: Arc::new(Mutex::new(HashMap::new())),
            delivery_gates: Arc::new(Mutex::new(DeliveryGateRegistry::default())),
            detached_tasks: Arc::new(Mutex::new(Vec::new())),
            completion_events,
        }
    }

    fn session_delivery_gate(&self, session_id: &SessionId) -> SessionDeliveryGate {
        let mut registry = self.delivery_gates.lock();
        if let Some(gate) = registry.gates.get(session_id).and_then(Weak::upgrade) {
            return gate;
        }
        if registry.gates.len() >= registry.next_prune_at {
            registry.gates.retain(|_, gate| gate.strong_count() > 0);
            registry.next_prune_at = registry
                .gates
                .len()
                .saturating_mul(2)
                .max(INITIAL_DELIVERY_GATE_PRUNE_AT);
        }
        let gate = Arc::new(AsyncMutex::new(SessionDeliveryState::Open));
        registry
            .gates
            .insert(session_id.clone(), Arc::downgrade(&gate));
        gate
    }

    fn track_detached_task(&self, handle: JoinHandle<()>) {
        let mut tasks = self.detached_tasks.lock();
        tasks.retain(|task| !task.is_finished());
        tasks.push(handle);
    }

    /// 等待所有 detached completion / force-kill 任务结束（进程退出前调用）。
    pub async fn drain_detached_tasks(&self) {
        loop {
            let handles: Vec<JoinHandle<()>> = self.detached_tasks.lock().drain(..).collect();
            if handles.is_empty() {
                break;
            }
            for handle in handles {
                if let Err(error) = handle.await {
                    tracing::warn!(%error, "turn scheduler background task failed");
                }
            }
        }
    }

    pub async fn shutdown_background_tasks(&self) {
        self.child_sessions.shutdown_completion_watcher().await;
        let mut session_ids = self.registry.active_session_ids();
        for session_id in self.pending_queues.lock().keys() {
            if !session_ids.contains(session_id) {
                session_ids.push(session_id.clone());
            }
        }
        for session_id in &session_ids {
            let gate = self.session_delivery_gate(session_id);
            *gate.lock().await = SessionDeliveryState::Closing;
            self.pending_queues.lock().remove(session_id);
            self.request_turn_shutdown(session_id);
        }
        self.drain_detached_tasks().await;
    }

    #[cfg(feature = "testing")]
    pub fn tracked_detached_task_count(&self) -> usize {
        self.detached_tasks
            .lock()
            .iter()
            .filter(|task| !task.is_finished())
            .count()
    }

    #[cfg(feature = "testing")]
    pub fn tracked_detached_task_slots(&self) -> usize {
        self.detached_tasks.lock().len()
    }

    pub fn registry(&self) -> &Arc<TurnRegistry> {
        &self.registry
    }

    pub(crate) fn subscribe_completions(&self) -> broadcast::Receiver<TurnCompletionEvent> {
        self.completion_events.subscribe()
    }

    pub(crate) async fn sync_durable_events(&self, session_id: &SessionId) {
        self.session_manager.sync_durable_events(session_id).await;
    }

    /// 统一的 execution 状态查询。
    pub async fn execution_view(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionExecutionView, TurnScheduleError> {
        let active_turn_id = self.registry.active_turn_id(session_id);
        let state = self
            .session_manager
            .read_model(session_id)
            .await
            .map_err(|e| TurnScheduleError::SessionNotFound(format!("{session_id}: {e}")))?;
        let queued_inputs = self
            .pending_queues
            .lock()
            .get(session_id)
            .map(|q| q.len())
            .unwrap_or(0);
        Ok(SessionExecutionView {
            phase: state.execution.phase,
            active_turn_id,
            queued_inputs,
            message_count: state.transcript.messages.len(),
        })
    }

    /// 输入投递的唯一 public gateway。
    pub async fn deliver_input(
        &self,
        session_id: SessionId,
        input: PromptInput,
        delivery: InputDelivery,
    ) -> Result<DeliveryOutcome, TurnScheduleError> {
        let operation = self.begin_session_operation(&session_id).await?;
        self.deliver_input_in_operation(&operation, input, delivery)
            .await
    }

    pub(crate) async fn begin_session_operation(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionOperationGuard, TurnScheduleError> {
        let state = self.session_delivery_gate(session_id).lock_owned().await;
        state.ensure_open(session_id)?;
        Ok(SessionOperationGuard {
            session_id: session_id.clone(),
            _state: state,
        })
    }

    pub(crate) async fn deliver_input_in_operation(
        &self,
        operation: &SessionOperationGuard,
        input: PromptInput,
        delivery: InputDelivery,
    ) -> Result<DeliveryOutcome, TurnScheduleError> {
        validate_prompt_input(&input)?;
        let session_id = operation.session_id().clone();
        match delivery {
            InputDelivery::StartNew => {
                self.start_detached_locked(session_id, input, "deliver_input:start")
                    .await
            },
            InputDelivery::InjectOnly => {
                if let Some(finished_turn_id) = self.registry.remove_if_finished(&session_id) {
                    tracing::debug!(
                        session_id = %session_id,
                        turn_id = %finished_turn_id,
                        "finished active turn was still registered; rejecting stale injection"
                    );
                    self.sync_durable_events(&session_id).await;
                    return Err(TurnScheduleError::NoActiveTurn);
                }
                let Some((turn_id, session)) = self.registry.active_execution(&session_id) else {
                    return Err(TurnScheduleError::NoActiveTurn);
                };
                self.inject_internal(&turn_id, &session, input).await?;
                Ok(DeliveryOutcome::Injected { turn_id })
            },
            InputDelivery::InjectIfRunningElseStart => {
                if let Some(finished_turn_id) = self.registry.remove_if_finished(&session_id) {
                    tracing::debug!(
                        session_id = %session_id,
                        turn_id = %finished_turn_id,
                        "finished active turn was still registered; starting injected input as a new turn"
                    );
                    self.sync_durable_events(&session_id).await;
                    return self
                        .start_detached_locked(
                            session_id,
                            input,
                            "deliver_input:inject-after-finished",
                        )
                        .await;
                }
                if let Some((turn_id, session)) = self.registry.active_execution(&session_id) {
                    self.inject_internal(&turn_id, &session, input).await?;
                    return Ok(DeliveryOutcome::Injected { turn_id });
                }
                self.start_detached_locked(session_id, input, "deliver_input:inject")
                    .await
            },
            InputDelivery::QueueIfRunningElseStart if self.registry.has_active(&session_id) => {
                let queue_len = {
                    let mut queues = self.pending_queues.lock();
                    let queue = queues.entry(session_id.clone()).or_default();
                    if queue.len() >= MAX_PENDING_INPUTS_PER_SESSION {
                        return Err(TurnScheduleError::QueueFull {
                            max: MAX_PENDING_INPUTS_PER_SESSION,
                        });
                    }
                    queue.push_back(input);
                    queue.len()
                };
                tracing::info!(
                    session_id = %session_id,
                    queue_len,
                    "message queued for next turn"
                );
                Ok(DeliveryOutcome::Queued { queue_len })
            },
            InputDelivery::QueueIfRunningElseStart => {
                self.start_detached_locked(session_id, input, "deliver_input:queue")
                    .await
            },
            InputDelivery::InterruptAndStart => {
                self.abort_in_operation(&session_id).await?;
                self.start_detached_locked(session_id, input, "deliver_input:interrupt")
                    .await
            },
        }
    }

    /// 调用方必须持有对应 session 的 delivery gate。
    async fn start_detached_locked(
        &self,
        session_id: SessionId,
        input: PromptInput,
        source: &'static str,
    ) -> Result<DeliveryOutcome, TurnScheduleError> {
        let StartedExecution { turn_id, handle } = self
            .start_execution_locked(session_id.clone(), input)
            .await?;
        self.watch_detached_turn(session_id, turn_id.clone(), handle, source, None);
        Ok(DeliveryOutcome::Started { turn_id })
    }

    /// 启动新 turn 并返回 handle（需要等待结果时用 [`Self::start_with_completion`]）。
    pub async fn start_with_completion(
        &self,
        session_id: SessionId,
        input: PromptInput,
    ) -> Result<StartedExecution, TurnScheduleError> {
        validate_prompt_input(&input)?;
        let _operation = self.begin_session_operation(&session_id).await?;
        self.start_execution_locked(session_id, input).await
    }

    /// 启动由 scheduler 持有 completion watcher 的 turn，并返回首轮完成通知。
    pub(crate) async fn start_tracked_with_completion(
        &self,
        session_id: SessionId,
        input: PromptInput,
    ) -> Result<(TurnId, oneshot::Receiver<TurnCompletion>), TurnScheduleError> {
        let StartedExecution { turn_id, handle } = self
            .start_with_completion(session_id.clone(), input)
            .await?;
        let (completion_tx, completion_rx) = oneshot::channel();
        self.watch_detached_turn(
            session_id,
            turn_id.clone(),
            handle,
            "tracked",
            Some(completion_tx),
        );
        Ok((turn_id, completion_rx))
    }

    /// 低层启动：调用方必须已持有对应 session 的 delivery gate。注册 registry 并返回 handle；
    /// 调用方须走 [`Self::finish_and_maybe_start_next`] 收尾。
    async fn start_execution_locked(
        &self,
        session_id: SessionId,
        input: PromptInput,
    ) -> Result<StartedExecution, TurnScheduleError> {
        if self.registry.has_active(&session_id) {
            return Err(TurnScheduleError::TurnAlreadyRunning);
        }

        tracing::info!(
            session_id = %session_id,
            text_len = input.text.len(),
            attachment_count = input.attachments.len(),
            "scheduler: submit turn"
        );

        let session = self
            .session_manager
            .open(session_id.clone())
            .await
            .map_err(|e| TurnScheduleError::SessionNotFound(format!("{session_id}: {e}")))?;

        let turn_id = new_turn_id();
        let handle = session
            .submit(input.text, input.attachments, turn_id.clone())
            .await
            .map_err(|e| {
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
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Option<StartedExecution> {
        self.registry.remove_if_matches(session_id, turn_id);
        self.sync_durable_events(session_id).await;
        self.child_sessions.drain_completed(self, session_id).await;

        let gate = self.session_delivery_gate(session_id);
        let state = gate.lock().await;
        if *state == SessionDeliveryState::Closing {
            return None;
        }
        if self.registry.has_active(session_id) {
            return None;
        }
        let input = self.dequeue_next_pending(session_id)?;
        tracing::info!(
            session_id = %session_id,
            "auto-submitting next queued message for new turn"
        );
        match self.start_execution_locked(session_id.clone(), input).await {
            Ok(started) => Some(started),
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
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
        self.watch_detached_turn(session_id, turn_id, handle, "queued", None);
    }

    fn watch_detached_turn(
        &self,
        session_id: SessionId,
        turn_id: TurnId,
        handle: TurnHandle,
        source: &'static str,
        completion_tx: Option<oneshot::Sender<TurnCompletion>>,
    ) {
        let scheduler = self.clone();
        let handle = tokio::spawn(async move {
            scheduler
                .run_detached_completion_watcher(session_id, turn_id, handle, source, completion_tx)
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
        mut completion_tx: Option<oneshot::Sender<TurnCompletion>>,
    ) {
        loop {
            let completion = match handle.wait().await {
                Some(result) => match result.output {
                    Ok(output) => TurnCompletion::Completed {
                        finish_reason: output.finish_reason,
                    },
                    Err(error) => TurnCompletion::Failed {
                        error: error.to_string(),
                    },
                },
                None => {
                    tracing::warn!(
                        session_id = %session_id,
                        turn_id = %turn_id,
                        source,
                        "detached turn task ended without completion"
                    );
                    TurnCompletion::Dropped
                },
            };

            let next = self
                .finish_and_maybe_start_next(&session_id, &turn_id)
                .await;

            if let Some(tx) = completion_tx.take() {
                let _ = tx.send(completion.clone());
            }
            let _ = self.completion_events.send(TurnCompletionEvent {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
                completion,
            });

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
        let _operation = self.begin_session_operation(session_id).await?;
        self.abort_in_operation(session_id).await
    }

    async fn abort_in_operation(&self, session_id: &SessionId) -> Result<(), TurnScheduleError> {
        self.child_sessions
            .cascade_abort_children(self, session_id)
            .await;
        self.request_turn_shutdown(session_id);

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

        if matches!(state.execution.phase, Phase::Idle | Phase::Error) {
            return Ok(());
        }

        self.repair_stale(session_id).await
    }

    /// 仅对本 session 发起协作式 shutdown（不级联子 session、不跑 stale repair）。
    fn request_turn_shutdown(&self, session_id: &SessionId) {
        if let Some(turn_id) = self.registry.request_shutdown(session_id) {
            self.schedule_force_kill(session_id.clone(), turn_id);
        }
    }

    /// completion 已产出但 task 可能尚未退出；只按 turn identity 非破坏性移除。
    pub async fn release_completed_execution(&self, session_id: &SessionId, turn_id: &TurnId) {
        let gate = self.session_delivery_gate(session_id);
        let _state = gate.lock().await;
        self.release_completed_execution_locked(session_id, turn_id);
    }

    fn release_completed_execution_locked(&self, session_id: &SessionId, turn_id: &TurnId) -> bool {
        if self
            .registry
            .remove_if_matches(session_id, turn_id)
            .is_some()
        {
            self.pending_queues.lock().remove(session_id);
            return true;
        }
        false
    }

    /// 清理 session 的 execution 与待处理输入；仍在运行的 turn 会被中止。
    pub async fn cleanup_execution(&self, session_id: &SessionId) {
        let gate = self.session_delivery_gate(session_id);
        let _state = gate.lock().await;
        self.cleanup_execution_locked(session_id).await;
    }

    async fn cleanup_execution_locked(&self, session_id: &SessionId) {
        if self.registry.remove_if_finished(session_id).is_none() {
            if let Some((turn_id, session)) = self.registry.force_kill_current(session_id) {
                self.emit_turn_aborted(&turn_id, &session, session_id).await;
            }
        }
        if self.pending_queues.lock().remove(session_id).is_some() {
            tracing::info!(session_id = %session_id, "cleaned up pending message queue");
        }
    }

    async fn close_delivery_gate(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionDeliveryGate, TurnScheduleError> {
        let gate = self.session_delivery_gate(session_id);
        let mut state = gate.lock().await;
        state.ensure_open(session_id)?;
        *state = SessionDeliveryState::Closing;
        drop(state);
        Ok(gate)
    }

    pub async fn delete_session(&self, session_id: &SessionId) -> Result<(), TurnScheduleError> {
        let _gate = self.close_delivery_gate(session_id).await?;
        self.cleanup_execution_locked(session_id).await;
        self.session_manager.delete(session_id).await?;
        Ok(())
    }

    pub async fn recycle_session(&self, session_id: &SessionId) -> Result<(), TurnScheduleError> {
        let _gate = self.close_delivery_gate(session_id).await?;
        self.cleanup_execution_locked(session_id).await;
        self.session_manager.recycle_session(session_id).await?;
        Ok(())
    }

    pub(crate) async fn recycle_completed_session(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<CompletedRecycleOutcome, TurnScheduleError> {
        let gate = self.session_delivery_gate(session_id);
        let mut state = gate.lock().await;
        state.ensure_open(session_id)?;
        if !self.release_completed_execution_locked(session_id, turn_id) {
            return Ok(CompletedRecycleOutcome::StaleCompletion);
        }
        *state = SessionDeliveryState::Closing;
        drop(state);
        let _gate = gate;
        self.session_manager.recycle_session(session_id).await?;
        Ok(CompletedRecycleOutcome::Recycled)
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
                match emit_interrupted_tool_results(
                    session,
                    &state,
                    Some(turn_id),
                    InterruptedToolOutcome::Cancelled,
                )
                .await
                {
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
        session.emit_live(
            Some(turn_id),
            agent_run_completed_payload(TURN_FINISH_ABORTED),
        );
        self.session_manager.sync_durable_events(session_id).await;
    }

    async fn inject_internal(
        &self,
        turn_id: &TurnId,
        session: &Session,
        input: PromptInput,
    ) -> Result<(), TurnScheduleError> {
        let message_id = new_message_id();
        session
            .emit_durable(
                Some(turn_id),
                DurableEventPayload::UserMessage {
                    message_id,
                    text: input.text,
                    attachments: input.attachments,
                },
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

        if matches!(state.execution.phase, Phase::Idle | Phase::Error) {
            repair_incomplete_tool_protocol_for_state(&session, &state).await?;
        } else {
            repair_stale_phase_for_state(session_id, &session, &state).await?;
        }

        repair_stale_runs_for_state(self, &session, &state).await?;

        self.session_manager.sync_durable_events(session_id).await;
        Ok(())
    }

    pub(crate) fn needs_stale_repair(state: &SessionReadModel) -> bool {
        !matches!(state.execution.phase, Phase::Idle | Phase::Error)
            || !state.tool_calls_needing_interruption().is_empty()
            || state
                .agent_sessions
                .iter()
                .any(|link| link.status == AgentSessionStatus::Running)
    }

    // ─── Pending Input Queue ──────────────────────────────────────

    fn dequeue_next_pending(&self, session_id: &SessionId) -> Option<PromptInput> {
        let mut queues = self.pending_queues.lock();
        let queue = queues.get_mut(session_id)?;
        let input = queue.pop_front()?;
        if queue.is_empty() {
            queues.remove(session_id);
        }
        if input.can_submit() {
            Some(input)
        } else {
            None
        }
    }
}

// ─── Stale repair 内部函数 ─────────────────────────────────────────

fn validate_prompt_input(input: &PromptInput) -> Result<(), TurnScheduleError> {
    let len = input.text.len();
    if len > MAX_PROMPT_TEXT_BYTES {
        return Err(TurnScheduleError::InputTooLarge {
            actual: len,
            max: MAX_PROMPT_TEXT_BYTES,
        });
    }
    Ok(())
}

async fn repair_stale_phase_for_state(
    session_id: &SessionId,
    session: &Session,
    state: &SessionReadModel,
) -> Result<(), TurnScheduleError> {
    if matches!(state.execution.phase, Phase::Idle | Phase::Error) {
        return Err(TurnScheduleError::NoActiveTurn);
    }

    tracing::info!(
        session_id = %session_id,
        phase = ?state.execution.phase,
        "repairing stale turn phase"
    );

    emit_interrupted_tool_results(session, state, None, InterruptedToolOutcome::Failed)
        .await
        .map_err(TurnScheduleError::EventEmit)?;
    emit_turn_aborted_context(session, None)
        .await
        .map_err(TurnScheduleError::EventEmit)?;

    session
        .emit_durable(None, turn_completed_payload(TURN_FINISH_INTERRUPTED))
        .await
        .map_err(TurnScheduleError::EventEmit)?;
    session.emit_live(None, agent_run_completed_payload(TURN_FINISH_INTERRUPTED));

    Ok(())
}

async fn repair_incomplete_tool_protocol_for_state(
    session: &Session,
    state: &SessionReadModel,
) -> Result<(), TurnScheduleError> {
    let interrupted =
        emit_interrupted_tool_results(session, state, None, InterruptedToolOutcome::Failed)
            .await
            .map_err(TurnScheduleError::EventEmit)?;
    if interrupted > 0 {
        emit_turn_aborted_context(session, None)
            .await
            .map_err(TurnScheduleError::EventEmit)?;
    }
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
        .filter(|link| link.status == AgentSessionStatus::Running)
    {
        let child_sid = &link.child_session_id;
        if scheduler.registry().has_active(child_sid) {
            scheduler.cleanup_execution(child_sid).await;
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
