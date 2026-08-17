//! Active execution 唯一 owner：输入投递、队列、registry、completion 收口与 stale repair。
//!
//! # 输入投递（[`InputDelivery`]）
//!
//! 所有用户输入进入运行中的 session，应经 [`Self::deliver_input`] 并显式选择策略：
//!
//! | 策略 | running | idle | 典型调用方 |
//! |------|---------|------|------------|
//! | [`InputDelivery::StartNew`] | busy | 开 turn | 测试、必须独占 turn 的路径 |
//! | [`InputDelivery::InjectOnly`] | durable `UserInputAccepted`（归属活跃 `turn_id`） | no active turn | HTTP `POST .../inject`、扩展 `defer_context` |
//! | [`InputDelivery::InjectIfRunningElseStart`] | durable `UserInputAccepted`（归属活跃 `turn_id`） | 开 turn | `SessionOperations::inject_message`、子 session 完成通知 |
//! | [`InputDelivery::QueueIfRunningElseStart`] | durable pending FIFO | 开 turn | HTTP/ACP `submit_input`（连发 prompt 不打断当前 turn）、扩展 `queue_or_start` |
//!
//! **Steer** 不是第三种策略：inject 与 queue 共用 accepted→absorbed 管线——接受时只落
//! `UserInputAccepted`（不进 transcript），由被归属的 turn 在 agent step 边界（上一轮工具
//! 结果已配对落盘之后）按 `accepted_seq` 吸收为 `UserMessage`（见 `astrcode_session::steer`）。
//!
//! # Cancel / Abort 分层
//!
//! - **Abort**（用户/API）：[`Self::abort`] 表达「停止当前 turn」；先协作式 shutdown， grace period
//!   后 force kill，必要时跑 stale repair。
//! - **Shutdown**（机制）：shutdown 先停止新 task 接纳，再协作停止所有活跃 turn。
//! - **Force kill**（机制）：abort / shutdown 在 grace 超时后硬杀 task 并写终态。
//! - **finish_reason**：`aborted` = 用户停止；`interrupted` = repair / 进程恢复。
//!
//! 对外只应使用 [`Self::deliver_input`] 与 [`Self::start_with_completion`]。启动路径先在
//! per-session gate 内完成决策和 turn reservation，再释放 gate 执行 session I/O；队列路径
//! 在 gate 内持久化 accepted event，以保证确认顺序就是 FIFO 顺序。delete / recycle 会等待
//! 已取得 reservation 的 start I/O 结束。

mod delivery;
mod lifecycle;
mod queue;

use std::{future::Future, panic::AssertUnwindSafe, sync::Arc};

use astrcode_core::{event::Phase, types::*, user_input::UserInput};
use astrcode_session::{SessionError, TurnFinalization, TurnHandle};
use futures_util::FutureExt;
use thiserror::Error;
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;

use crate::{
    child_session::ChildSessionCoordinator,
    delivery_gates::{SessionDeliveryGates, SessionOperationGuard, SessionStartLease},
    queue_drains::QueueDrainTracker,
    session_manager::SessionManager,
    task_utils::{OwnedTaskAdmission, OwnedTaskSet},
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
    #[error(
        "session {session_id} was recycled, but updating its parent relation failed \
         ({relation_error}) and restoring the session also failed ({restore_error})"
    )]
    RecycleRelationRollbackFailed {
        session_id: SessionId,
        relation_error: String,
        restore_error: String,
    },
    #[error(
        "session {session_id} was deleted, but updating its parent {parent_session_id} failed: \
         {relation_error}"
    )]
    DeleteRelationUpdateFailed {
        session_id: SessionId,
        parent_session_id: SessionId,
        relation_error: String,
    },
    #[error(
        "parent session {parent_session_id} has a conflicting relation for child \
         {child_session_id}: expected {expected}, found {actual}"
    )]
    ChildRelationConflict {
        parent_session_id: SessionId,
        child_session_id: SessionId,
        expected: String,
        actual: String,
    },
    #[error("completion ownership was lost for session {session_id}, turn {turn_id}")]
    CompletionOwnershipLost {
        session_id: SessionId,
        turn_id: TurnId,
    },
    #[error("server background tasks are shutting down")]
    BackgroundTasksClosed,
}

/// 输入投递策略（见模块文档「输入投递」表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDelivery {
    /// 必须 idle；否则 busy。
    StartNew,
    /// 必须 running；否则返回 [`TurnScheduleError::NoActiveTurn`]。
    InjectOnly,
    /// running 时落归属当前 turn 的 durable `UserInputAccepted`（mid-turn steer，由该 turn
    /// 在 step 边界吸收）；idle 时 start。
    InjectIfRunningElseStart,
    /// running 时入队，当前 turn 结束后 FIFO 开新 turn；idle 时 start。
    QueueIfRunningElseStart,
    /// 先中断当前 turn，再以新输入启动 turn。
    InterruptAndStart,
}

/// 输入投递结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Started {
        turn_id: TurnId,
    },
    /// 输入已 durable 接受并归属该 turn(`UserInputAccepted`);进入 transcript 的
    /// `UserMessage` 由该 turn 在下一 step 边界吸收时提交。
    Injected {
        turn_id: TurnId,
    },
    Queued {
        queue_len: usize,
    },
}

pub struct StartedExecution {
    pub turn_id: TurnId,
    pub handle: TurnHandle,
}

pub struct OwnedExecution {
    turn_id: TurnId,
    handle: TurnHandle,
    admission: OwnedTaskAdmission,
}

pub enum FinishOutcome {
    Settled(Option<OwnedExecution>),
    Stale,
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

struct CompletionWatch {
    source: &'static str,
    completion_tx: Option<oneshot::Sender<TurnCompletion>>,
    output_tx: Option<oneshot::Sender<Result<String, TurnScheduleError>>>,
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
const FINALIZATION_RETRY_INITIAL_MS: u64 = 100;
const FINALIZATION_RETRY_MAX_MS: u64 = 5_000;
#[cfg(any(test, feature = "testing"))]
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
    owned_tasks: Arc<OwnedTaskSet>,
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
            owned_tasks: Arc::clone(session_manager.owned_tasks()),
            session_manager,
            registry,
            child_sessions,
            queue_drains: Arc::new(QueueDrainTracker::default()),
            delivery_gates: Arc::new(SessionDeliveryGates::default()),
            background_shutdown: CancellationToken::new(),
            completion_events,
        }
    }

    pub(crate) fn admit_owned(&self) -> Result<OwnedTaskAdmission, TurnScheduleError> {
        self.owned_tasks
            .admit()
            .map_err(|_| TurnScheduleError::BackgroundTasksClosed)
    }

    pub(crate) fn spawn_owned<F>(
        &self,
        task: F,
    ) -> Result<tokio::task::JoinHandle<F::Output>, TurnScheduleError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.owned_tasks
            .spawn(task)
            .map_err(|_| TurnScheduleError::BackgroundTasksClosed)
    }

    pub(crate) fn spawn_owned_named(
        &self,
        name: &'static str,
        task: impl Future<Output = ()> + Send + 'static,
    ) -> Result<tokio::task::JoinHandle<()>, TurnScheduleError> {
        self.spawn_owned(async move {
            if AssertUnwindSafe(task).catch_unwind().await.is_err() {
                tracing::error!(task = name, "owned background task panicked");
            }
        })
    }

    pub(crate) fn stop_task_admission(&self) {
        self.owned_tasks.stop_accepting();
    }

    pub(crate) async fn wait_for_task_admissions(&self) {
        self.owned_tasks.wait_for_admissions().await;
    }

    pub(crate) async fn close_and_wait(&self) {
        self.owned_tasks.close_and_wait().await;
    }

    pub async fn shutdown_background_tasks(&self) {
        self.stop_task_admission();
        self.background_shutdown.cancel();
        self.request_all_turn_shutdowns();

        self.wait_for_task_admissions().await;
        self.request_all_turn_shutdowns();
        tokio::time::sleep(std::time::Duration::from_millis(FORCE_KILL_GRACE_MS)).await;
        self.settle_all_active_turns_for_shutdown().await;

        self.child_sessions
            .drain_completion_guards_for_shutdown(self)
            .await;
        self.child_sessions.shutdown_completion_watcher().await;
        self.close_and_wait().await;
        if !self.registry.active_session_ids().is_empty()
            || self.child_sessions.has_completion_owners()
        {
            tracing::error!("shutdown completed with unfinished session owners");
        }
    }

    fn request_all_turn_shutdowns(&self) {
        for session_id in self.registry.active_session_ids() {
            self.registry.request_shutdown(&session_id);
        }
    }

    async fn settle_all_active_turns_for_shutdown(&self) {
        while !self.registry.active_session_ids().is_empty() {
            for session_id in self.registry.active_session_ids() {
                let operation = self.delivery_gates.lock(&session_id).await;
                operation.wait_for_starts().await;
                if let Err(error) = self.cleanup_execution_locked(&session_id).await {
                    tracing::warn!(
                        %session_id,
                        %error,
                        "failed to settle turn during shutdown; retrying"
                    );
                }
            }
            if !self.registry.active_session_ids().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }

    #[cfg(feature = "testing")]
    pub fn owned_task_count(&self) -> usize {
        self.owned_tasks.task_count()
    }

    #[cfg(feature = "testing")]
    pub fn accepts_owned_tasks(&self) -> bool {
        self.owned_tasks.is_accepting()
    }

    pub fn registry(&self) -> &Arc<TurnRegistry> {
        &self.registry
    }

    pub(crate) fn subscribe_completions(&self) -> broadcast::Receiver<TurnCompletionEvent> {
        self.completion_events.subscribe()
    }

    pub(crate) async fn sync_durable_events_required(
        &self,
        session_id: &SessionId,
    ) -> Result<(), TurnScheduleError> {
        self.session_manager
            .sync_durable_events_required(session_id)
            .await
            .map_err(TurnScheduleError::SessionManager)
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
            message_count: state.model_context.messages.len(),
        })
    }

    /// 启动新 turn 并返回 handle（需要等待结果时用 [`Self::start_with_completion`]）。
    #[cfg(any(test, feature = "testing"))]
    pub(crate) async fn start_with_completion(
        &self,
        session_id: SessionId,
        input: UserInput,
    ) -> Result<StartedExecution, TurnScheduleError> {
        let _admission = self.admit_owned()?;
        self.start_with_completion_admitted(session_id, input).await
    }

    async fn start_with_completion_admitted(
        &self,
        session_id: SessionId,
        input: UserInput,
    ) -> Result<StartedExecution, TurnScheduleError> {
        let operation = self.begin_session_operation(&session_id).await?;
        let reserved = self.reserve_new_execution(&operation, input).await?;
        drop(operation);
        self.start_reserved_execution(reserved)
            .await
            .map_err(|failure| failure.error)
    }

    /// 供需要在 turn 启动后继续登记外部 owner 的调用方使用；调用方决定何时释放 gate。
    #[cfg(any(test, feature = "testing"))]
    pub(crate) async fn start_with_completion_in_operation(
        &self,
        operation: &SessionOperationGuard,
        input: UserInput,
    ) -> Result<StartedExecution, TurnScheduleError> {
        let _admission = self.admit_owned()?;
        self.start_with_completion_in_admitted_operation(operation, input)
            .await
    }

    pub(crate) async fn start_with_completion_in_admitted_operation(
        &self,
        operation: &SessionOperationGuard,
        input: UserInput,
    ) -> Result<StartedExecution, TurnScheduleError> {
        let reserved = self.reserve_new_execution(operation, input).await?;
        self.start_reserved_execution(reserved)
            .await
            .map_err(|failure| failure.error)
    }

    async fn reserve_new_execution(
        &self,
        operation: &SessionOperationGuard,
        input: UserInput,
    ) -> Result<ReservedExecution, TurnScheduleError> {
        validate_user_input(&input)?;
        let session_id = operation.session_id();
        self.ensure_no_pending_inputs(session_id).await?;
        self.reserve_execution(operation, input, None)
    }

    /// 启动由 scheduler 持有 completion watcher 的 turn，并返回首轮完成通知。
    pub(crate) async fn start_tracked_with_completion(
        &self,
        session_id: SessionId,
        input: UserInput,
    ) -> Result<(TurnId, oneshot::Receiver<TurnCompletion>), TurnScheduleError> {
        let admission = self.admit_owned()?;
        let StartedExecution { turn_id, handle } = self
            .start_with_completion_admitted(session_id.clone(), input)
            .await?;
        let (completion_tx, completion_rx) = oneshot::channel();
        self.watch_owned_turn(
            admission,
            session_id,
            turn_id.clone(),
            handle,
            CompletionWatch {
                source: "tracked",
                completion_tx: Some(completion_tx),
                output_tx: None,
            },
        );
        Ok((turn_id, completion_rx))
    }

    /// 启动由 scheduler 持有 completion owner 的 turn，并在 durable 收口后返回文本输出。
    pub(crate) async fn start_tracked_with_output(
        &self,
        session_id: SessionId,
        input: UserInput,
    ) -> Result<(TurnId, oneshot::Receiver<Result<String, TurnScheduleError>>), TurnScheduleError>
    {
        let admission = self.admit_owned()?;
        let StartedExecution { turn_id, handle } = self
            .start_with_completion_admitted(session_id.clone(), input)
            .await?;
        let (output_tx, output_rx) = oneshot::channel();
        self.watch_owned_turn(
            admission,
            session_id,
            turn_id.clone(),
            handle,
            CompletionWatch {
                source: "tracked-output",
                completion_tx: None,
                output_tx: Some(output_tx),
            },
        );
        Ok((turn_id, output_rx))
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
        finalization: Option<&TurnFinalization>,
    ) -> Result<FinishOutcome, TurnScheduleError> {
        self.sync_durable_events_required(session_id).await?;
        self.child_sessions.drain_completed(self, session_id).await;

        let operation = self.delivery_gates.lock(session_id).await;
        let settled = self
            .settle_finished_execution_locked(session_id, turn_id, finalization)
            .await?;
        if !settled {
            return Ok(FinishOutcome::Stale);
        }
        Ok(FinishOutcome::Settled(
            self.start_next_after_settle_in_operation(operation).await?,
        ))
    }

    pub(crate) async fn start_next_after_settle_in_operation(
        &self,
        operation: SessionOperationGuard,
    ) -> Result<Option<OwnedExecution>, TurnScheduleError> {
        let session_id = operation.session_id().clone();
        if operation.is_closing() || self.background_shutdown.is_cancelled() {
            return Ok(None);
        }
        let admission = match self.admit_owned() {
            Ok(admission) => admission,
            Err(TurnScheduleError::BackgroundTasksClosed) => return Ok(None),
            Err(error) => return Err(error),
        };
        if self.registry.has_active(&session_id) {
            return Ok(None);
        }
        let (reserved, entry) = match self.reserve_next_pending(&operation).await {
            Ok(Some(next)) => next,
            Ok(None) => return Ok(None),
            Err(error) => {
                tracing::warn!(session_id = %session_id, %error, "failed to reserve queued input");
                return Err(error);
            },
        };
        drop(operation);
        Ok(self
            .start_reserved_pending(reserved, entry)
            .await
            .ok()
            .map(|started| OwnedExecution {
                turn_id: started.turn_id,
                handle: started.handle,
                admission,
            }))
    }

    /// 若 [`Self::finish_and_maybe_start_next`] 已启动队列中的下一条 execution，挂上 detached
    /// watcher。
    pub(crate) fn watch_queued_if_any(&self, session_id: SessionId, next: Option<OwnedExecution>) {
        let Some(OwnedExecution {
            turn_id,
            handle,
            admission,
        }) = next
        else {
            return;
        };
        self.watch_owned_turn(
            admission,
            session_id,
            turn_id,
            handle,
            CompletionWatch {
                source: "queued",
                completion_tx: None,
                output_tx: None,
            },
        );
    }

    fn watch_owned_turn(
        &self,
        admission: OwnedTaskAdmission,
        session_id: SessionId,
        turn_id: TurnId,
        handle: TurnHandle,
        watch: CompletionWatch,
    ) {
        let scheduler = self.clone();
        admission.spawn_named(watch.source, async move {
            scheduler
                .run_detached_completion_watcher(session_id, turn_id, handle, watch)
                .await;
        });
    }

    async fn run_detached_completion_watcher(
        &self,
        session_id: SessionId,
        mut turn_id: TurnId,
        mut handle: TurnHandle,
        mut watch: CompletionWatch,
    ) {
        loop {
            let (completion, finalization, tracked_output) = match handle.wait().await {
                Some(result) => {
                    let finalization = result.finalization.clone();
                    let (completion, tracked_output) = match result.output {
                        Ok(output) => {
                            let finish_reason = output.finish_reason;
                            let tracked_output =
                                watch.output_tx.is_some().then_some(Ok(output.text));
                            (TurnCompletion::Completed { finish_reason }, tracked_output)
                        },
                        Err(error) => {
                            let message = error.to_string();
                            let tracked_output = watch
                                .output_tx
                                .is_some()
                                .then_some(Err(TurnScheduleError::Turn(error)));
                            (TurnCompletion::Failed { error: message }, tracked_output)
                        },
                    };
                    (completion, Some(finalization), tracked_output)
                },
                None => {
                    tracing::warn!(
                        session_id = %session_id,
                        turn_id = %turn_id,
                        source = watch.source,
                        "detached turn task ended without completion"
                    );
                    let tracked_output = watch.output_tx.is_some().then_some(Err(
                        TurnScheduleError::CompletionOwnershipLost {
                            session_id: session_id.clone(),
                            turn_id: turn_id.clone(),
                        },
                    ));
                    (TurnCompletion::Dropped, None, tracked_output)
                },
            };

            let mut retry_attempt = 0u32;
            let mut retry_delay_ms = FINALIZATION_RETRY_INITIAL_MS;
            let next = loop {
                match self
                    .finish_and_maybe_start_next(&session_id, &turn_id, finalization.as_ref())
                    .await
                {
                    Ok(FinishOutcome::Settled(next)) => break next,
                    Ok(FinishOutcome::Stale) => return,
                    Err(error) => {
                        retry_attempt += 1;
                        if retry_attempt == 1 || retry_attempt.is_power_of_two() {
                            tracing::warn!(
                                %session_id,
                                %turn_id,
                                %error,
                                retry_attempt,
                                retry_delay_ms,
                                "turn finalization is not durable yet; retrying before publishing completion"
                            );
                        } else {
                            tracing::debug!(
                                %session_id,
                                %turn_id,
                                %error,
                                retry_attempt,
                                retry_delay_ms,
                                "turn finalization retry failed"
                            );
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(retry_delay_ms)).await;
                        retry_delay_ms =
                            (retry_delay_ms.saturating_mul(2)).min(FINALIZATION_RETRY_MAX_MS);
                    },
                }
            };

            if let (Some(tx), Some(output)) = (watch.output_tx.take(), tracked_output) {
                let _ = tx.send(output);
            }
            if let Some(tx) = watch.completion_tx.take() {
                let _ = tx.send(completion.clone());
            }
            let _ = self.completion_events.send(TurnCompletionEvent {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
                completion,
            });

            let Some(OwnedExecution {
                turn_id: next_turn_id,
                handle: next_handle,
                admission,
            }) = next
            else {
                break;
            };
            turn_id = next_turn_id;
            handle = next_handle;
            drop(admission);
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
