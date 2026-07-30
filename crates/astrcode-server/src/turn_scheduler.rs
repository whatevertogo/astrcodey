//! Active execution 唯一 owner：输入投递、队列、registry、completion 收口与 stale repair。
//!
//! # 输入投递（[`InputDelivery`]）
//!
//! 所有用户输入进入运行中的 session，应经 [`Self::deliver_input`] 并显式选择策略：
//!
//! | 策略 | running | idle | 典型调用方 |
//! |------|---------|------|------------|
//! | [`InputDelivery::StartNew`] | busy | 开 turn | 测试、必须独占 turn 的路径 |
//! | [`InputDelivery::InjectOnly`] | durable `UserMessage`（同 `turn_id`） | no active turn | HTTP `POST .../inject` |
//! | [`InputDelivery::InjectIfRunningElseStart`] | durable `UserMessage`（同 `turn_id`） | 开 turn | `SessionOperations::inject_message`、子 session 完成通知 |
//! | [`InputDelivery::QueueIfRunningElseStart`] | durable pending FIFO | 开 turn | HTTP/ACP `submit_input`（连发 prompt 不打断当前 turn） |
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
//! 对外只应使用 [`Self::deliver_input`] 与 [`Self::start_with_completion`]。启动路径先在
//! per-session gate 内完成决策和 turn reservation，再释放 gate 执行 session I/O；队列路径
//! 在 gate 内持久化 accepted event，以保证确认顺序就是 FIFO 顺序。delete / recycle 会等待
//! 已取得 reservation 的 start I/O 结束。

mod delivery;
mod lifecycle;
mod queue;

use std::sync::Arc;

use astrcode_core::{event::Phase, types::*, user_input::UserInput};
use astrcode_session::{SessionError, TurnHandle};
use futures_util::FutureExt;
use parking_lot::Mutex;
use thiserror::Error;
use tokio::{
    sync::{broadcast, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    child_session::ChildSessionCoordinator,
    delivery_gates::{SessionDeliveryGates, SessionOperationGuard, SessionStartLease},
    queue_drains::QueueDrainTracker,
    session_manager::SessionManager,
    turn_registry::{TurnRegistry, TurnReservation},
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
    #[error("prompt must contain text or an attachment")]
    EmptyInput,
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

struct ReservedExecution {
    session_id: SessionId,
    turn_id: TurnId,
    input: UserInput,
    accepted_seq: Option<u64>,
    reservation: TurnReservation,
    _start_lease: SessionStartLease,
}

struct StartExecutionFailure {
    error: TurnScheduleError,
    _reservation: Option<TurnReservation>,
    _start_lease: SessionStartLease,
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

pub const MAX_PENDING_INPUTS_PER_SESSION: usize = 32;
pub const MAX_PROMPT_TEXT_BYTES: usize = 1024 * 1024;
const FORCE_KILL_GRACE_MS: u64 = 1500;
const ABORT_WAIT_POLL_MS: u64 = 50;
const ABORT_WAIT_EXTRA_MS: u64 = 500;
pub(crate) enum CompletedRecycleOutcome {
    Recycled,
    StaleCompletion,
}

#[derive(Clone)]
pub struct TurnScheduler {
    session_manager: Arc<SessionManager>,
    registry: Arc<TurnRegistry>,
    child_sessions: Arc<ChildSessionCoordinator>,
    queue_drains: Arc<QueueDrainTracker>,
    delivery_gates: Arc<SessionDeliveryGates>,
    detached_tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
    background_shutdown: CancellationToken,
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
            queue_drains: Arc::new(QueueDrainTracker::default()),
            delivery_gates: Arc::new(SessionDeliveryGates::default()),
            detached_tasks: Arc::new(Mutex::new(Vec::new())),
            background_shutdown: CancellationToken::new(),
            completion_events,
        }
    }

    fn track_detached_task(&self, handle: JoinHandle<()>) {
        let mut tasks = self.detached_tasks.lock();
        let mut active = Vec::with_capacity(tasks.len() + 1);
        let mut finished = Vec::new();
        for task in tasks.drain(..) {
            if task.is_finished() {
                finished.push(task);
            } else {
                active.push(task);
            }
        }
        active.push(handle);
        *tasks = active;
        drop(tasks);

        for task in finished {
            if let Some(Err(error)) = task.now_or_never() {
                tracing::warn!(%error, "turn scheduler background task failed");
            }
        }
    }

    /// 等待所有 detached completion / force-kill 任务结束（进程退出前调用）。
    pub async fn drain_detached_tasks(&self) {
        self.background_shutdown.cancel();
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
        let session_ids = self.registry.active_session_ids();
        for session_id in &session_ids {
            let gate = self.delivery_gates.mark_closing(session_id).await;
            gate.wait_for_starts().await;
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
        let queued_inputs = state.execution.pending_inputs.len();
        Ok(SessionExecutionView {
            phase: state.execution.phase,
            active_turn_id,
            queued_inputs,
            message_count: state.transcript.messages.len(),
        })
    }

    /// 启动新 turn 并返回 handle（需要等待结果时用 [`Self::start_with_completion`]）。
    pub async fn start_with_completion(
        &self,
        session_id: SessionId,
        input: UserInput,
    ) -> Result<StartedExecution, TurnScheduleError> {
        validate_user_input(&input)?;
        let operation = self.begin_session_operation(&session_id).await?;
        self.ensure_no_pending_inputs(&session_id).await?;
        let reserved = self.reserve_execution(&operation, input, None)?;
        drop(operation);
        self.start_reserved_execution(reserved)
            .await
            .map_err(|failure| failure.error)
    }

    /// 启动由 scheduler 持有 completion watcher 的 turn，并返回首轮完成通知。
    pub(crate) async fn start_tracked_with_completion(
        &self,
        session_id: SessionId,
        input: UserInput,
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

    fn reserve_execution(
        &self,
        operation: &SessionOperationGuard,
        input: UserInput,
        accepted_seq: Option<u64>,
    ) -> Result<ReservedExecution, TurnScheduleError> {
        let session_id = operation.session_id().clone();
        if self.registry.has_active(&session_id) {
            return Err(TurnScheduleError::TurnAlreadyRunning);
        }
        let turn_id = new_turn_id();
        let reservation = self
            .registry
            .reserve(session_id.clone(), turn_id.clone())
            .ok_or(TurnScheduleError::TurnAlreadyRunning)?;
        Ok(ReservedExecution {
            session_id,
            turn_id,
            input,
            accepted_seq,
            reservation,
            _start_lease: operation.start_lease(),
        })
    }

    async fn start_reserved_execution(
        &self,
        reserved: ReservedExecution,
    ) -> Result<StartedExecution, Box<StartExecutionFailure>> {
        let ReservedExecution {
            session_id,
            turn_id,
            input,
            accepted_seq,
            reservation,
            _start_lease,
        } = reserved;

        tracing::info!(
            session_id = %session_id,
            text_len = input.text.len(),
            attachment_count = input.attachments.len(),
            "scheduler: submit turn"
        );

        let session = match self.session_manager.open(session_id.clone()).await {
            Ok(session) => session,
            Err(error) => {
                return Err(Box::new(StartExecutionFailure {
                    error: TurnScheduleError::SessionNotFound(format!("{session_id}: {error}")),
                    _reservation: Some(reservation),
                    _start_lease,
                }));
            },
        };

        let handle = match session.submit(input, turn_id.clone(), accepted_seq).await {
            Ok(handle) => handle,
            Err(error) => {
                tracing::error!(session_id = %session_id, %error, "session.submit failed");
                return Err(Box::new(StartExecutionFailure {
                    error: TurnScheduleError::Turn(error),
                    _reservation: Some(reservation),
                    _start_lease,
                }));
            },
        };

        let session_arc = Arc::new(session);
        if !reservation.activate(handle.shutdown_handle(), session_arc) {
            handle.force_kill();
            return Err(Box::new(StartExecutionFailure {
                error: TurnScheduleError::TurnAlreadyRunning,
                _reservation: None,
                _start_lease,
            }));
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
        self.sync_durable_events(session_id).await;
        self.child_sessions.drain_completed(self, session_id).await;

        let operation = self.delivery_gates.lock(session_id).await;
        self.registry.remove_if_matches(session_id, turn_id);
        if operation.is_closing() {
            return None;
        }
        if self.registry.has_active(session_id) {
            return None;
        }
        let (reserved, entry) = match self.reserve_next_pending(&operation).await {
            Ok(Some(next)) => next,
            Ok(None) => return None,
            Err(error) => {
                tracing::warn!(session_id = %session_id, %error, "failed to reserve queued input");
                return None;
            },
        };
        drop(operation);
        self.start_reserved_pending(reserved, entry).await.ok()
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
}

fn validate_user_input(input: &UserInput) -> Result<(), TurnScheduleError> {
    if !input.can_submit() {
        return Err(TurnScheduleError::EmptyInput);
    }
    let len = input.text.len();
    if len > MAX_PROMPT_TEXT_BYTES {
        return Err(TurnScheduleError::InputTooLarge {
            actual: len,
            max: MAX_PROMPT_TEXT_BYTES,
        });
    }
    Ok(())
}
