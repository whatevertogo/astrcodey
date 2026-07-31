//! 子 agent session 的 server 侧 owner：spawn、turn 提交、completion guard、终态与回收。

mod completion;

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use astrcode_core::{
    event::DurableEventPayload,
    tool::{CreateSessionRequest, SessionApiError},
    types::{SessionId, TurnId},
};
use astrcode_session::{TurnError, TurnFinalization, TurnHandle};
use astrcode_session_projection::{AgentSessionLinkView, AgentSessionStatus, SessionReadModel};
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use self::completion::{ChildSessionCompletionGuard, build_background_agent_notification};
use crate::{
    delivery_gates::SessionOperationGuard,
    session_manager::{SessionManager, SessionManagerError},
    turn_scheduler::{InputDelivery, TurnScheduleError, TurnScheduler},
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChildOutcome {
    Completed { output: String },
    Failed { error: String },
    Aborted,
    TimedOut,
}

#[derive(Debug, Clone)]
struct ChildCompletion {
    outcome: ChildOutcome,
    finalization: Option<TurnFinalization>,
}

enum ChildRelationUpdate {
    Completed {
        child_session_id: SessionId,
        summary: String,
    },
    Failed {
        child_session_id: SessionId,
        error: String,
    },
    Recycled {
        child_session_id: SessionId,
    },
}

#[derive(Clone, Copy)]
enum SessionAccessScope {
    Active,
    ActiveOrRecycled,
}

impl ChildRelationUpdate {
    fn child_session_id(&self) -> &SessionId {
        match self {
            Self::Completed {
                child_session_id, ..
            }
            | Self::Failed {
                child_session_id, ..
            }
            | Self::Recycled { child_session_id } => child_session_id,
        }
    }

    fn expected(&self) -> &'static str {
        match self {
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
            Self::Recycled { .. } => "recycled",
        }
    }

    fn is_applied(&self, link: Option<&AgentSessionLinkView>) -> bool {
        match (self, link) {
            (
                Self::Completed {
                    child_session_id,
                    summary,
                },
                Some(link),
            ) => {
                link.status == AgentSessionStatus::Completed
                    && link.final_session_id.as_ref() == Some(child_session_id)
                    && link.summary.as_ref() == Some(summary)
            },
            (
                Self::Failed {
                    child_session_id,
                    error,
                },
                Some(link),
            ) => {
                link.status == AgentSessionStatus::Failed
                    && link.final_session_id.as_ref() == Some(child_session_id)
                    && link.error.as_ref() == Some(error)
            },
            (Self::Recycled { .. }, None) => true,
            _ => false,
        }
    }

    fn can_apply(&self, link: Option<&AgentSessionLinkView>) -> bool {
        match self {
            Self::Completed { .. } | Self::Failed { .. } => {
                link.is_some_and(|link| link.status == AgentSessionStatus::Running)
            },
            Self::Recycled { .. } => link.is_some(),
        }
    }

    fn into_payload(self) -> DurableEventPayload {
        match self {
            Self::Completed {
                child_session_id,
                summary,
            } => astrcode_session::payload::agent_session_completed_payload(
                child_session_id,
                summary,
            ),
            Self::Failed {
                child_session_id,
                error,
            } => astrcode_session::payload::agent_session_failed_payload(child_session_id, error),
            Self::Recycled { child_session_id } => {
                DurableEventPayload::AgentSessionRecycled { child_session_id }
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildCleanup {
    Recycle,
    Keep,
}

struct ChildSessionCompletionConfig {
    child_session_id: SessionId,
    parent_session_id: SessionId,
    turn_id: TurnId,
    cleanup: ChildCleanup,
    /// 非 None 时在完成后向父 session 注入通知；字符串作 summary 提示（可为空）。
    notify_on_complete: Option<String>,
    tool_call_id: Option<String>,
}

type CompletionGuards = Vec<Arc<ChildSessionCompletionGuard>>;

#[cfg(any(test, feature = "testing"))]
struct CompletionGuardPause {
    reached: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

pub(crate) struct ChildTreeShutdown {
    guards: Vec<TreeCompletionClaim>,
}

struct TreeCompletionClaim {
    guard: Arc<ChildSessionCompletionGuard>,
    claim: CompletionClaimLease,
}

struct ClaimedCompletionGuard {
    guard: Arc<ChildSessionCompletionGuard>,
    operation: SessionOperationGuard,
    claim: CompletionClaimLease,
}

struct ClaimedCompletionGuards {
    guards: Vec<ClaimedCompletionGuard>,
    error: Option<TurnScheduleError>,
}

#[derive(Default)]
struct CompletionClaims {
    active: AtomicUsize,
}

impl CompletionClaims {
    fn acquire(self: &Arc<Self>, count: usize) -> CompletionClaimLease {
        self.active.fetch_add(count, Ordering::AcqRel);
        CompletionClaimLease {
            claims: Arc::clone(self),
            count,
            restore: None,
        }
    }

    fn is_idle(&self) -> bool {
        self.active.load(Ordering::Acquire) == 0
    }
}

struct CompletionClaimLease {
    claims: Arc<CompletionClaims>,
    count: usize,
    restore: Option<CompletionClaimRestore>,
}

struct CompletionClaimRestore {
    by_parent: Arc<Mutex<HashMap<SessionId, CompletionGuards>>>,
    completed_tx: mpsc::UnboundedSender<SessionId>,
    guard: Arc<ChildSessionCompletionGuard>,
}

impl CompletionClaimLease {
    fn commit(&mut self) {
        self.restore = None;
    }
}

impl Drop for CompletionClaimLease {
    fn drop(&mut self) {
        if let Some(restore) = self.restore.take() {
            let parent_session_id = restore.guard.parent_session_id().clone();
            restore
                .by_parent
                .lock()
                .entry(parent_session_id.clone())
                .or_default()
                .push(restore.guard);
            let _ = restore.completed_tx.send(parent_session_id);
        }
        if self.count > 0 {
            self.claims.active.fetch_sub(self.count, Ordering::AcqRel);
        }
    }
}

/// 子 agent session 完成、turn 提交与回收的 server 侧协调者。
pub struct ChildSessionCoordinator {
    session_manager: Arc<SessionManager>,
    by_parent: Arc<Mutex<HashMap<SessionId, CompletionGuards>>>,
    completion_claims: Arc<CompletionClaims>,
    completed_tx: mpsc::UnboundedSender<SessionId>,
    completed_rx: Mutex<Option<mpsc::UnboundedReceiver<SessionId>>>,
    watcher_shutdown: CancellationToken,
    watcher: Mutex<Option<tokio::task::JoinHandle<()>>>,
    #[cfg(any(test, feature = "testing"))]
    registration_pause: Mutex<Option<CompletionGuardPause>>,
    #[cfg(any(test, feature = "testing"))]
    claim_pause: Mutex<Option<CompletionGuardPause>>,
    #[cfg(any(test, feature = "testing"))]
    sync_settled_pause: Mutex<Option<CompletionGuardPause>>,
}

impl ChildSessionCoordinator {
    pub fn new(session_manager: Arc<SessionManager>) -> Self {
        let (completed_tx, completed_rx) = mpsc::unbounded_channel();
        Self {
            session_manager,
            by_parent: Arc::new(Mutex::new(HashMap::new())),
            completion_claims: Arc::new(CompletionClaims::default()),
            completed_tx,
            completed_rx: Mutex::new(Some(completed_rx)),
            watcher_shutdown: CancellationToken::new(),
            watcher: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            registration_pause: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            claim_pause: Mutex::new(None),
            #[cfg(any(test, feature = "testing"))]
            sync_settled_pause: Mutex::new(None),
        }
    }

    /// 启动后台任务：child guard 完成后自动 drain 终态、回收与 notify。
    ///
    /// 每个实例只应调用一次（bootstrap 与测试 harness）。
    pub fn spawn_completion_watcher(self: &Arc<Self>, scheduler: Arc<TurnScheduler>) {
        let Some(mut rx) = self.completed_rx.lock().take() else {
            tracing::debug!("child completion watcher already running");
            return;
        };
        let coordinator = Arc::clone(self);
        let shutdown = self.watcher_shutdown.clone();
        let watcher_scheduler = Arc::clone(&scheduler);
        let watcher = match scheduler.spawn_owned_named("child_completion_watcher", async move {
            loop {
                tokio::select! {
                    parent_sid = rx.recv() => {
                        let Some(parent_sid) = parent_sid else {
                            break;
                        };
                        coordinator
                            .drain_completed(watcher_scheduler.as_ref(), &parent_sid)
                            .await;
                    },
                    _ = shutdown.cancelled() => {
                        while let Ok(parent_sid) = rx.try_recv() {
                            coordinator
                                .drain_completed(watcher_scheduler.as_ref(), &parent_sid)
                                .await;
                        }
                        break;
                    },
                }
            }
        }) {
            Ok(watcher) => watcher,
            Err(error) => {
                tracing::warn!(%error, "child completion watcher rejected");
                return;
            },
        };
        *self.watcher.lock() = Some(watcher);
    }

    pub async fn shutdown_completion_watcher(&self) {
        self.watcher_shutdown.cancel();
        let watcher = self.watcher.lock().take();
        if let Some(watcher) = watcher {
            if let Err(error) = watcher.await {
                tracing::warn!(%error, "child completion watcher failed to stop");
            }
        }
    }

    pub(crate) async fn drain_completion_guards_for_shutdown(&self, scheduler: &TurnScheduler) {
        loop {
            let parent_ids: Vec<_> = {
                let by_parent = self.by_parent.lock();
                if by_parent.is_empty() && self.completion_claims.is_idle() {
                    return;
                }
                by_parent.keys().cloned().collect()
            };
            for parent_id in parent_ids {
                self.drain_completed(scheduler, &parent_id).await;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub(crate) fn has_completion_owners(&self) -> bool {
        !self.by_parent.lock().is_empty() || !self.completion_claims.is_idle()
    }

    pub async fn verify_access(
        &self,
        caller: &SessionId,
        target: &SessionId,
    ) -> Result<(), SessionApiError> {
        self.verify_access_in_scope(caller, target, SessionAccessScope::Active)
            .await
    }

    pub(crate) async fn verify_restore_access(
        &self,
        caller: &SessionId,
        target: &SessionId,
    ) -> Result<(), SessionApiError> {
        self.verify_access_in_scope(caller, target, SessionAccessScope::ActiveOrRecycled)
            .await
    }

    async fn verify_access_in_scope(
        &self,
        caller: &SessionId,
        target: &SessionId,
        scope: SessionAccessScope,
    ) -> Result<(), SessionApiError> {
        if caller == target {
            return Ok(());
        }
        let mut current = target.clone();
        let mut visited = HashSet::new();
        let mut caller_is_ancestor = false;
        loop {
            if !visited.insert(current.clone()) {
                return Err(SessionApiError::internal_msg(format!(
                    "session parent chain contains a cycle at {current}"
                )));
            }
            let model = self.read_access_model(&current, scope).await?;
            match &model.identity.parent {
                Some(parent) => {
                    if &parent.session_id == caller {
                        caller_is_ancestor = true;
                    }
                    current = parent.session_id.clone();
                },
                None => {
                    return if caller_is_ancestor {
                        Ok(())
                    } else {
                        Err(SessionApiError::PermissionDenied(format!(
                            "session {target} is not a descendant of {caller}"
                        )))
                    };
                },
            }
        }
    }

    async fn read_access_model(
        &self,
        session_id: &SessionId,
        scope: SessionAccessScope,
    ) -> Result<Arc<SessionReadModel>, SessionApiError> {
        match self.session_manager.read_model(session_id).await {
            Ok(model) => Ok(model),
            Err(error) if matches!(scope, SessionAccessScope::Active) => {
                Err(SessionApiError::NotFound(error.to_string()))
            },
            Err(SessionManagerError::Storage(astrcode_storage::StorageError::NotFound(_))) => self
                .session_manager
                .read_recycled_model(session_id)
                .await
                .map_err(map_recycled_access_error),
            Err(error) => Err(SessionApiError::internal(error)),
        }
    }

    pub async fn session_depth(&self, session_id: &SessionId) -> Result<usize, SessionApiError> {
        let mut depth = 0;
        let mut current = session_id.clone();
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current.clone()) {
                return Err(SessionApiError::internal_msg(format!(
                    "session parent chain contains a cycle at {current}"
                )));
            }
            let model = self
                .session_manager
                .read_model(&current)
                .await
                .map_err(SessionApiError::internal)?;
            match &model.identity.parent {
                Some(parent) => {
                    depth += 1;
                    current = parent.session_id.clone();
                },
                None => break,
            }
        }
        Ok(depth)
    }

    pub(crate) async fn spawn_child(
        &self,
        parent_operation: SessionOperationGuard,
        request: CreateSessionRequest,
    ) -> Result<astrcode_session::Session, SessionApiError> {
        let parent_session_id = parent_operation.session_id().clone();
        let parent_session = self
            .session_manager
            .open(parent_session_id.clone())
            .await
            .map_err(|e| SessionApiError::NotFound(format!("parent: {e}")))?;

        let depth = self.session_depth(&parent_session_id).await?;
        let max_depth = self.session_manager.max_agent_depth();
        if depth >= max_depth {
            return Err(SessionApiError::MaxDepthExceeded {
                current: depth,
                max: max_depth,
            });
        }

        let parent_model = parent_session
            .read_model()
            .await
            .map_err(SessionApiError::internal)?;

        let working_dir = request
            .working_dir
            .unwrap_or_else(|| parent_model.identity.working_dir.clone());
        let model_id = request
            .model_preference
            .filter(|m| m != "inherit" && !m.is_empty())
            .unwrap_or_else(|| parent_model.identity.model_id.clone());
        let task = self
            .session_manager
            .spawn_creation_task(async move {
                let result = parent_session
                    .spawn_child(
                        &working_dir,
                        &model_id,
                        request.name,
                        String::new(),
                        request.system_prompt,
                        request.tool_selection,
                        request.source_extension.as_deref(),
                        request.tool_call_id.into(),
                    )
                    .await;
                drop(parent_operation);
                result
            })
            .map_err(SessionApiError::internal)?;
        task.await
            .map_err(|error| {
                SessionApiError::internal_msg(format!(
                    "child session creation transaction stopped: {error}"
                ))
            })?
            .map_err(SessionApiError::internal)
    }

    /// 启动独立 completion owner；请求只等待结果，取消请求不会丢失 turn 收口所有权。
    pub async fn submit_turn_sync(
        self: &Arc<Self>,
        scheduler: Arc<TurnScheduler>,
        caller_sid: &SessionId,
        target_sid: &SessionId,
        user_prompt: String,
    ) -> Result<String, SessionApiError> {
        let guard_admission = scheduler.admit_owned().map_err(SessionApiError::internal)?;
        let owner_admission = scheduler.admit_owned().map_err(SessionApiError::internal)?;
        self.prepare_turn_target(target_sid).await?;
        let operation = scheduler
            .begin_session_operation(target_sid)
            .await
            .map_err(SessionApiError::internal)?;
        let started = scheduler
            .start_with_completion_in_admitted_operation(
                &operation,
                astrcode_core::user_input::UserInput::text_only(user_prompt),
            )
            .await
            .map_err(SessionApiError::internal)?;
        let config = ChildSessionCompletionConfig {
            child_session_id: target_sid.clone(),
            parent_session_id: caller_sid.clone(),
            turn_id: started.turn_id.clone(),
            cleanup: ChildCleanup::Keep,
            notify_on_complete: None,
            tool_call_id: None,
        };
        let (guard, terminal_rx) =
            self.register_sync_completion_guard(guard_admission, started.handle, config);
        let coordinator = Arc::clone(self);
        let owner_scheduler = Arc::clone(&scheduler);
        let owner_guard = Arc::clone(&guard);
        let parent_sid = caller_sid.clone();
        owner_admission.spawn_named("child_sync_completion_owner", async move {
            let _ = owner_guard.completion().await;
            coordinator
                .drain_completed(owner_scheduler.as_ref(), &parent_sid)
                .await;
        });
        drop(operation);
        self.drain_completed(scheduler.as_ref(), caller_sid).await;
        let completion = guard.completion().await;
        terminal_rx
            .await
            .map_err(SessionApiError::internal)?
            .map_err(SessionApiError::internal_msg)?;
        match completion.outcome {
            ChildOutcome::Completed { output } => Ok(output),
            ChildOutcome::Failed { error } => Err(SessionApiError::internal_msg(error)),
            ChildOutcome::Aborted => Err(SessionApiError::internal(TurnError::Aborted)),
            ChildOutcome::TimedOut => Err(SessionApiError::internal_msg("turn timed out")),
        }
    }

    /// 后台 turn：注册 completion guard，并 drain 父 session 上已完成的 child。
    #[allow(clippy::too_many_arguments)]
    pub async fn submit_turn_background(
        &self,
        scheduler: &TurnScheduler,
        caller_sid: &SessionId,
        target_sid: &SessionId,
        user_prompt: String,
        cleanup: ChildCleanup,
        notify_on_complete: Option<String>,
        tool_call_id: Option<String>,
    ) -> Result<(TurnId, SessionId), SessionApiError> {
        let guard_admission = scheduler.admit_owned().map_err(SessionApiError::internal)?;
        self.prepare_turn_target(target_sid).await?;
        let operation = scheduler
            .begin_session_operation(target_sid)
            .await
            .map_err(SessionApiError::internal)?;
        let started = scheduler
            .start_with_completion_in_admitted_operation(
                &operation,
                astrcode_core::user_input::UserInput::text_only(user_prompt),
            )
            .await
            .map_err(SessionApiError::internal)?;

        let turn_id = started.turn_id.clone();
        let config = ChildSessionCompletionConfig {
            child_session_id: target_sid.clone(),
            parent_session_id: caller_sid.clone(),
            turn_id: turn_id.clone(),
            cleanup,
            notify_on_complete,
            tool_call_id,
        };
        self.register_completion_guard(guard_admission, started.handle, config)
            .await;
        drop(operation);
        self.drain_completed(scheduler, caller_sid).await;
        Ok((turn_id, target_sid.clone()))
    }

    async fn record_completed(
        &self,
        parent_sid: &SessionId,
        child_sid: &SessionId,
        summary: &str,
    ) -> Result<(), TurnScheduleError> {
        self.record_child_relation(
            parent_sid,
            ChildRelationUpdate::Completed {
                child_session_id: child_sid.clone(),
                summary: crate::presentation::inline_preview(summary, 159),
            },
        )
        .await
    }

    async fn record_failed(
        &self,
        parent_sid: &SessionId,
        child_sid: &SessionId,
        error: &str,
    ) -> Result<(), TurnScheduleError> {
        self.record_child_relation(
            parent_sid,
            ChildRelationUpdate::Failed {
                child_session_id: child_sid.clone(),
                error: error.to_string(),
            },
        )
        .await
    }

    pub(crate) async fn record_child_deleted(
        &self,
        parent_sid: &SessionId,
        child_sid: &SessionId,
    ) -> Result<(), TurnScheduleError> {
        self.record_failed(parent_sid, child_sid, "deleted").await
    }

    pub(crate) async fn record_child_recycled(
        &self,
        parent_sid: &SessionId,
        child_sid: &SessionId,
    ) -> Result<(), TurnScheduleError> {
        self.record_child_relation(
            parent_sid,
            ChildRelationUpdate::Recycled {
                child_session_id: child_sid.clone(),
            },
        )
        .await
    }

    async fn record_child_relation(
        &self,
        parent_sid: &SessionId,
        update: ChildRelationUpdate,
    ) -> Result<(), TurnScheduleError> {
        let parent_session = self
            .session_manager
            .open(parent_sid.clone())
            .await
            .map_err(TurnScheduleError::from)?;
        let parent_model = parent_session
            .read_model()
            .await
            .map_err(TurnScheduleError::Session)?;
        let child_session_id = update.child_session_id();
        let link = parent_model
            .agent_sessions
            .iter()
            .find(|link| &link.child_session_id == child_session_id);
        if update.is_applied(link) {
            self.session_manager
                .sync_durable_events_required(parent_sid)
                .await
                .map_err(TurnScheduleError::SessionManager)?;
            return Ok(());
        }
        if !update.can_apply(link) {
            return Err(TurnScheduleError::ChildRelationConflict {
                parent_session_id: parent_sid.clone(),
                child_session_id: child_session_id.clone(),
                expected: update.expected().into(),
                actual: link
                    .map(|link| format!("{:?}", link.status).to_lowercase())
                    .unwrap_or_else(|| "missing".into()),
            });
        }
        parent_session
            .emit_durable(None, update.into_payload())
            .await
            .map_err(TurnScheduleError::EventEmit)?;
        self.session_manager
            .sync_durable_events_required(parent_sid)
            .await
            .map_err(TurnScheduleError::SessionManager)?;
        Ok(())
    }

    pub async fn drain_completed(&self, scheduler: &TurnScheduler, parent_sid: &SessionId) {
        let completed = self.completed_guard_candidates(parent_sid);
        for candidate in completed {
            let operation = match scheduler
                .begin_session_operation(candidate.child_session_id())
                .await
            {
                Ok(operation) => operation,
                Err(error) => {
                    self.schedule_completion_retry(&candidate);
                    tracing::debug!(
                        child_session_id = %candidate.child_session_id(),
                        %error,
                        "child completion operation unavailable; retry scheduled"
                    );
                    continue;
                },
            };
            let Some((guard, mut claim)) = self.claim_completion_guard(parent_sid, &candidate)
            else {
                drop(operation);
                continue;
            };
            #[cfg(any(test, feature = "testing"))]
            self.pause_after_claim_for_test().await;

            let completion = guard.completion().await;
            if !guard.child_is_settled() {
                let settled = scheduler
                    .release_completed_execution_in_operation(
                        &operation,
                        guard.turn_id(),
                        completion.finalization.as_ref(),
                    )
                    .await;
                match settled {
                    Ok(true) => {
                        guard.mark_child_settled();
                        #[cfg(any(test, feature = "testing"))]
                        if guard.has_terminal_waiter() {
                            self.pause_sync_settled_for_test().await;
                        }
                    },
                    Ok(false) => {
                        if self.completion_turn_is_durably_settled(&guard).await {
                            guard.mark_child_settled();
                            #[cfg(any(test, feature = "testing"))]
                            if guard.has_terminal_waiter() {
                                self.pause_sync_settled_for_test().await;
                            }
                        } else {
                            let superseded = self.completion_was_superseded_by_close(&guard).await;
                            if superseded {
                                claim.commit();
                            } else {
                                self.restore_completion_guard(Arc::clone(&guard));
                                claim.commit();
                            }
                            guard.finish_terminal(Err(format!(
                                "completion ownership was lost for session {}, turn {}",
                                guard.child_session_id(),
                                guard.turn_id()
                            )));
                            drop(operation);
                            tracing::warn!(
                                child_session_id = %guard.child_session_id(),
                                turn_id = %guard.turn_id(),
                                superseded,
                                "child completion lost registry ownership"
                            );
                            continue;
                        }
                    },
                    Err(error) => {
                        let superseded = self.completion_was_superseded_by_close(&guard).await;
                        if superseded {
                            claim.commit();
                        } else {
                            self.restore_completion_guard(Arc::clone(&guard));
                            claim.commit();
                        }
                        guard.finish_terminal(Err(error.to_string()));
                        drop(operation);
                        tracing::error!(
                            child_session_id = %guard.child_session_id(),
                            turn_id = %guard.turn_id(),
                            %error,
                            superseded,
                            "failed to settle child completion"
                        );
                        continue;
                    },
                }
            }

            if let Err(error) = self.write_terminal_for_guard(&guard, &completion).await {
                let conflict = matches!(&error, TurnScheduleError::ChildRelationConflict { .. });
                if conflict {
                    claim.commit();
                } else {
                    self.restore_completion_guard(Arc::clone(&guard));
                    claim.commit();
                }
                guard.finish_terminal(Err(error.to_string()));
                drop(operation);
                tracing::warn!(
                    parent_session_id = %guard.parent_session_id(),
                    child_session_id = %guard.child_session_id(),
                    %error,
                    conflict,
                    "failed to persist child terminal"
                );
                continue;
            }
            if guard.cleanup_policy() == ChildCleanup::Recycle {
                if let Err(error) = scheduler
                    .recycle_settled_session_in_operation(operation)
                    .await
                {
                    guard.force_recycle_on_completion();
                    self.restore_completion_guard(Arc::clone(&guard));
                    claim.commit();
                    guard.finish_terminal(Err(error.to_string()));
                    tracing::warn!(
                        parent_session_id = %guard.parent_session_id(),
                        session_id = %guard.child_session_id(),
                        %error,
                        "failed to recycle settled child session; guard ownership retained"
                    );
                    continue;
                }
            } else {
                match scheduler
                    .start_next_after_settle_in_operation(operation)
                    .await
                {
                    Ok(next) => {
                        scheduler.watch_queued_if_any(guard.child_session_id().clone(), next);
                    },
                    Err(error) => {
                        self.restore_completion_guard(Arc::clone(&guard));
                        claim.commit();
                        guard.finish_terminal(Err(error.to_string()));
                        tracing::warn!(
                            parent_session_id = %guard.parent_session_id(),
                            child_session_id = %guard.child_session_id(),
                            %error,
                            "failed to start queued child input; guard ownership retained"
                        );
                        continue;
                    },
                }
            }
            guard.finish_terminal(Ok(()));
            claim.commit();

            if guard.notify_text().is_some() {
                let message = build_background_agent_notification(&guard).await;
                if let Err(e) = scheduler
                    .deliver_input(
                        guard.parent_session_id().clone(),
                        astrcode_core::user_input::UserInput::text_only(message),
                        InputDelivery::InjectIfRunningElseStart,
                    )
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

    async fn register_completion_guard(
        &self,
        admission: crate::task_utils::OwnedTaskAdmission,
        handle: TurnHandle,
        config: ChildSessionCompletionConfig,
    ) {
        let parent_sid = config.parent_session_id.clone();
        let guard = Arc::new(ChildSessionCompletionGuard::new(&handle, config));
        self.by_parent
            .lock()
            .entry(parent_sid)
            .or_default()
            .push(Arc::clone(&guard));
        #[cfg(any(test, feature = "testing"))]
        self.pause_registration_for_test().await;
        guard.start(admission, handle, self.completed_tx.clone());
    }

    fn register_sync_completion_guard(
        &self,
        admission: crate::task_utils::OwnedTaskAdmission,
        handle: TurnHandle,
        config: ChildSessionCompletionConfig,
    ) -> (
        Arc<ChildSessionCompletionGuard>,
        tokio::sync::oneshot::Receiver<Result<(), String>>,
    ) {
        let parent_sid = config.parent_session_id.clone();
        let (guard, terminal_rx) = ChildSessionCompletionGuard::new_sync(&handle, config);
        let guard = Arc::new(guard);
        self.by_parent
            .lock()
            .entry(parent_sid)
            .or_default()
            .push(Arc::clone(&guard));
        guard.start(admission, handle, self.completed_tx.clone());
        (guard, terminal_rx)
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn pause_next_registration(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        Self::install_pause(&self.registration_pause)
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn pause_next_claim(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        Self::install_pause(&self.claim_pause)
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn pause_next_sync_settled(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        Self::install_pause(&self.sync_settled_pause)
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn registered_guard_count(&self, parent_sid: &SessionId) -> usize {
        self.by_parent.lock().get(parent_sid).map_or(0, Vec::len)
    }

    #[cfg(any(test, feature = "testing"))]
    fn install_pause(
        target: &Mutex<Option<CompletionGuardPause>>,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        *target.lock() = Some(CompletionGuardPause {
            reached: reached_tx,
            release: release_rx,
        });
        (reached_rx, release_tx)
    }

    #[cfg(any(test, feature = "testing"))]
    async fn pause_registration_for_test(&self) {
        Self::wait_for_pause(&self.registration_pause).await;
    }

    #[cfg(any(test, feature = "testing"))]
    async fn pause_after_claim_for_test(&self) {
        Self::wait_for_pause(&self.claim_pause).await;
    }

    #[cfg(any(test, feature = "testing"))]
    async fn pause_sync_settled_for_test(&self) {
        Self::wait_for_pause(&self.sync_settled_pause).await;
    }

    #[cfg(any(test, feature = "testing"))]
    async fn wait_for_pause(target: &Mutex<Option<CompletionGuardPause>>) {
        let Some(pause) = target.lock().take() else {
            return;
        };
        let _ = pause.reached.send(());
        let _ = pause.release.await;
    }

    fn completed_guard_candidates(&self, parent_sid: &SessionId) -> CompletionGuards {
        self.by_parent
            .lock()
            .get(parent_sid)
            .map(|guards| {
                guards
                    .iter()
                    .filter(|guard| guard.is_complete())
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn guard_candidates(&self, parent_sid: &SessionId) -> CompletionGuards {
        self.by_parent
            .lock()
            .get(parent_sid)
            .cloned()
            .unwrap_or_default()
    }

    fn registered_child_ids(&self) -> HashSet<SessionId> {
        self.by_parent
            .lock()
            .values()
            .flatten()
            .map(|guard| guard.child_session_id().clone())
            .collect()
    }

    fn claim_completion_guard(
        &self,
        parent_sid: &SessionId,
        expected: &Arc<ChildSessionCompletionGuard>,
    ) -> Option<(Arc<ChildSessionCompletionGuard>, CompletionClaimLease)> {
        let mut by_parent = self.by_parent.lock();
        let (guard, remove_parent) = {
            let guards = by_parent.get_mut(parent_sid)?;
            let index = guards
                .iter()
                .position(|guard| Arc::ptr_eq(guard, expected))?;
            let guard = guards.remove(index);
            (guard, guards.is_empty())
        };
        if remove_parent {
            by_parent.remove(parent_sid);
        }
        let claim = self.completion_claim_for_guard(&guard);
        Some((guard, claim))
    }

    fn completion_claim_for_guard(
        &self,
        guard: &Arc<ChildSessionCompletionGuard>,
    ) -> CompletionClaimLease {
        let mut claim = self.completion_claims.acquire(1);
        claim.restore = Some(CompletionClaimRestore {
            by_parent: Arc::clone(&self.by_parent),
            completed_tx: self.completed_tx.clone(),
            guard: Arc::clone(guard),
        });
        claim
    }

    fn restore_completion_guard(&self, guard: Arc<ChildSessionCompletionGuard>) {
        let parent_session_id = guard.parent_session_id().clone();
        self.by_parent
            .lock()
            .entry(parent_session_id)
            .or_default()
            .push(Arc::clone(&guard));
        self.schedule_completion_retry(&guard);
    }

    fn schedule_completion_retry(&self, guard: &ChildSessionCompletionGuard) {
        let delay = Duration::from_millis(guard.retry_delay_ms());
        let parent_session_id = guard.parent_session_id().clone();
        let completed_tx = self.completed_tx.clone();
        let shutdown = self.watcher_shutdown.clone();
        if self
            .session_manager
            .owned_tasks()
            .spawn_named("child_completion_retry", async move {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {
                        let _ = completed_tx.send(parent_session_id);
                    },
                    _ = shutdown.cancelled() => {},
                }
            })
            .is_err()
        {
            tracing::debug!("child completion retry rejected during shutdown");
        }
    }

    fn restore_claimed_guards(&self, guards: Vec<ClaimedCompletionGuard>) {
        for mut claimed in guards {
            self.restore_completion_guard(Arc::clone(&claimed.guard));
            claimed.claim.commit();
        }
    }

    pub async fn cascade_abort_children(
        &self,
        scheduler: &TurnScheduler,
        parent_sid: &SessionId,
    ) -> Result<(), TurnScheduleError> {
        let claimed = self
            .claim_guards_deep(scheduler, parent_sid, Duration::from_secs(10))
            .await;
        let mut guarded_children: HashSet<SessionId> = claimed
            .guards
            .iter()
            .map(|claimed| claimed.guard.child_session_id().clone())
            .collect();
        if let Some(error) = claimed.error {
            self.restore_claimed_guards(claimed.guards);
            return Err(error);
        }
        if !claimed.guards.is_empty() {
            self.finalize_aborted_children(scheduler, claimed.guards)
                .await?;
        }
        guarded_children.extend(self.registered_child_ids());
        self.abort_unguarded_running_children(scheduler, parent_sid, &guarded_children)
            .await
    }

    pub(crate) fn begin_tree_shutdown(&self, session_ids: &[SessionId]) -> ChildTreeShutdown {
        let closing_sessions: HashSet<_> = session_ids.iter().cloned().collect();
        let mut selected = Vec::new();
        let mut by_parent = self.by_parent.lock();
        let parent_ids: Vec<_> = by_parent.keys().cloned().collect();

        for parent_id in parent_ids {
            let guards = by_parent.remove(&parent_id).unwrap_or_default();
            let (mut closing, remaining): (CompletionGuards, CompletionGuards) =
                guards.into_iter().partition(|guard| {
                    closing_sessions.contains(&parent_id)
                        || closing_sessions.contains(guard.child_session_id())
                });
            selected.append(&mut closing);
            if !remaining.is_empty() {
                by_parent.insert(parent_id, remaining);
            }
        }
        let selected: Vec<_> = selected
            .into_iter()
            .map(|guard| TreeCompletionClaim {
                claim: self.completion_claim_for_guard(&guard),
                guard,
            })
            .collect();
        drop(by_parent);

        for claimed in &selected {
            claimed.guard.request_shutdown();
        }
        ChildTreeShutdown { guards: selected }
    }

    pub(crate) async fn finish_tree_shutdown(
        &self,
        mut shutdown: ChildTreeShutdown,
        settled_sessions: &HashSet<SessionId>,
    ) -> Result<HashSet<SessionId>, TurnScheduleError> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        for claimed in shutdown.guards.iter().rev() {
            let guard = &claimed.guard;
            if tokio::time::timeout_at(deadline, guard.outcome())
                .await
                .is_err()
            {
                tracing::warn!(
                    child_session_id = %guard.child_session_id(),
                    "session tree shutdown: child session timed out"
                );
                guard.force_timeout();
            }
        }

        let guarded_children: HashSet<_> = shutdown
            .guards
            .iter()
            .map(|claimed| claimed.guard.child_session_id().clone())
            .collect();
        let mut first_error = None;
        for claimed in shutdown.guards.iter_mut().rev() {
            let guard = &claimed.guard;
            if settled_sessions.contains(guard.child_session_id()) {
                guard.mark_child_settled();
            }
            if !guard.child_is_settled() && self.completion_turn_is_durably_settled(guard).await {
                guard.mark_child_settled();
            }
            if !guard.child_is_settled() {
                let error = TurnScheduleError::CompletionOwnershipLost {
                    session_id: guard.child_session_id().clone(),
                    turn_id: guard.turn_id().clone(),
                };
                guard.finish_terminal(Err(error.to_string()));
                self.restore_completion_guard(Arc::clone(guard));
                claimed.claim.commit();
                if first_error.is_none() {
                    first_error = Some(error);
                } else {
                    tracing::warn!(
                        child_session_id = %guard.child_session_id(),
                        turn_id = %guard.turn_id(),
                        "additional session tree completion ownership failure"
                    );
                }
                continue;
            }
            let completion = guard.completion().await;
            if let Err(error) = self.write_terminal_for_guard(guard, &completion).await {
                guard.finish_terminal(Err(error.to_string()));
                self.restore_completion_guard(Arc::clone(guard));
                claimed.claim.commit();
                if first_error.is_none() {
                    first_error = Some(error);
                } else {
                    tracing::warn!(
                        parent_session_id = %guard.parent_session_id(),
                        child_session_id = %guard.child_session_id(),
                        %error,
                        "additional child terminal persistence failure"
                    );
                }
            } else {
                guard.finish_terminal(Ok(()));
                claimed.claim.commit();
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(guarded_children)
    }

    async fn prepare_turn_target(&self, target_sid: &SessionId) -> Result<(), SessionApiError> {
        self.session_manager
            .open(target_sid.clone())
            .await
            .map_err(|e| SessionApiError::NotFound(e.to_string()))?;
        Ok(())
    }

    async fn write_terminal_for_guard(
        &self,
        guard: &ChildSessionCompletionGuard,
        completion: &ChildCompletion,
    ) -> Result<(), TurnScheduleError> {
        let parent_sid = guard.parent_session_id();
        let child_sid = guard.child_session_id();
        match &completion.outcome {
            ChildOutcome::Completed { output } => {
                self.record_completed(parent_sid, child_sid, output).await
            },
            ChildOutcome::Failed { error } => {
                self.record_failed(parent_sid, child_sid, error).await
            },
            ChildOutcome::Aborted => self.record_failed(parent_sid, child_sid, "aborted").await,
            ChildOutcome::TimedOut => self.record_failed(parent_sid, child_sid, "timed out").await,
        }
    }

    async fn completion_was_superseded_by_close(
        &self,
        guard: &ChildSessionCompletionGuard,
    ) -> bool {
        match self
            .session_manager
            .read_model(guard.parent_session_id())
            .await
        {
            Ok(model) => !model.agent_sessions.iter().any(|link| {
                link.child_session_id == *guard.child_session_id()
                    && link.status == AgentSessionStatus::Running
            }),
            Err(SessionManagerError::Storage(astrcode_storage::StorageError::NotFound(_))) => true,
            Err(_) => false,
        }
    }

    async fn completion_turn_is_durably_settled(
        &self,
        guard: &ChildSessionCompletionGuard,
    ) -> bool {
        self.session_manager
            .has_durable_turn_completion(guard.child_session_id(), guard.turn_id())
            .await
            .unwrap_or(false)
    }

    async fn claim_guards_deep(
        &self,
        scheduler: &TurnScheduler,
        root_sid: &SessionId,
        timeout: Duration,
    ) -> ClaimedCompletionGuards {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut claimed = Vec::new();
        let mut first_error = None;
        let mut stack: Vec<SessionId> = vec![root_sid.clone()];
        let mut visited = HashSet::from([root_sid.clone()]);

        while let Some(sid) = stack.pop() {
            for candidate in self.guard_candidates(&sid) {
                let child_session_id = candidate.child_session_id().clone();
                candidate.request_shutdown();
                if visited.insert(child_session_id.clone()) {
                    stack.push(child_session_id.clone());
                }
                let operation = match tokio::time::timeout_at(
                    deadline,
                    scheduler.begin_session_operation(&child_session_id),
                )
                .await
                {
                    Ok(Ok(operation)) => operation,
                    Ok(Err(error)) => {
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                        continue;
                    },
                    Err(_) => {
                        if first_error.is_none() {
                            first_error = Some(TurnScheduleError::CompletionOwnershipLost {
                                session_id: child_session_id,
                                turn_id: candidate.turn_id().clone(),
                            });
                        }
                        continue;
                    },
                };
                let Some((guard, claim)) = self.claim_completion_guard(&sid, &candidate) else {
                    drop(operation);
                    continue;
                };
                guard.force_recycle_on_completion();
                claimed.push(ClaimedCompletionGuard {
                    guard,
                    operation,
                    claim,
                });
            }
        }

        // 反向迭代：子 session 先于父 session 被等待，避免父节点提前结束而子节点仍悬挂。
        for claimed_guard in claimed.iter().rev() {
            if tokio::time::timeout_at(deadline, claimed_guard.guard.outcome())
                .await
                .is_err()
            {
                tracing::warn!(
                    child_session_id = %claimed_guard.guard.child_session_id(),
                    timeout_ms = timeout.as_millis(),
                    "cascade abort: child session timed out"
                );
                claimed_guard.guard.force_timeout();
            }
        }

        ClaimedCompletionGuards {
            guards: claimed,
            error: first_error,
        }
    }

    async fn finalize_aborted_children(
        &self,
        scheduler: &TurnScheduler,
        mut guards: Vec<ClaimedCompletionGuard>,
    ) -> Result<(), TurnScheduleError> {
        while let Some(claimed) = guards.pop() {
            let ClaimedCompletionGuard {
                guard,
                operation,
                mut claim,
            } = claimed;
            let child_sid = guard.child_session_id();
            let parent_sid = guard.parent_session_id();
            let completion = guard.completion().await;
            if !guard.child_is_settled() {
                match scheduler
                    .release_completed_execution_in_operation(
                        &operation,
                        guard.turn_id(),
                        completion.finalization.as_ref(),
                    )
                    .await
                {
                    Ok(true) => guard.mark_child_settled(),
                    Ok(false) => {
                        let error = TurnScheduleError::CompletionOwnershipLost {
                            session_id: child_sid.clone(),
                            turn_id: guard.turn_id().clone(),
                        };
                        guard.finish_terminal(Err(error.to_string()));
                        self.restore_completion_guard(Arc::clone(&guard));
                        claim.commit();
                        self.restore_claimed_guards(guards);
                        return Err(error);
                    },
                    Err(error) => {
                        guard.finish_terminal(Err(error.to_string()));
                        self.restore_completion_guard(Arc::clone(&guard));
                        claim.commit();
                        self.restore_claimed_guards(guards);
                        return Err(error);
                    },
                }
            }
            let terminal_error = if completion.outcome == ChildOutcome::TimedOut {
                "abort timed out"
            } else {
                "aborted"
            };
            if let Err(error) = self
                .record_failed(parent_sid, child_sid, terminal_error)
                .await
            {
                guard.finish_terminal(Err(error.to_string()));
                self.restore_completion_guard(Arc::clone(&guard));
                claim.commit();
                self.restore_claimed_guards(guards);
                return Err(error);
            }
            if let Err(error) = scheduler
                .recycle_settled_session_in_operation(operation)
                .await
            {
                guard.force_recycle_on_completion();
                guard.finish_terminal(Err(error.to_string()));
                self.restore_completion_guard(Arc::clone(&guard));
                claim.commit();
                self.restore_claimed_guards(guards);
                return Err(error);
            }
            guard.finish_terminal(Ok(()));
            claim.commit();
        }
        Ok(())
    }

    /// 处理没有 completion guard 的 running child（例如恢复的旧状态）。
    async fn abort_unguarded_running_children(
        &self,
        scheduler: &TurnScheduler,
        root_sid: &SessionId,
        guarded_children: &HashSet<SessionId>,
    ) -> Result<(), TurnScheduleError> {
        let mut pending: Vec<(SessionId, SessionId)> = Vec::new();
        let mut stack = vec![root_sid.clone()];

        while let Some(current) = stack.pop() {
            let Ok(model) = self.session_manager.read_model(&current).await else {
                continue;
            };
            let current_parent = current.clone();
            for link in model
                .agent_sessions
                .iter()
                .filter(|link| link.status == AgentSessionStatus::Running)
            {
                let child_sid = link.child_session_id.clone();
                if current_parent == *root_sid && guarded_children.contains(&child_sid) {
                    continue;
                }
                stack.push(child_sid.clone());
                pending.push((current_parent.clone(), child_sid));
            }
        }

        for (parent_sid, child_sid) in pending.into_iter().rev() {
            let operation = match scheduler.begin_session_operation(&child_sid).await {
                Ok(operation) => operation,
                Err(_) => continue,
            };
            if guarded_children.contains(&child_sid)
                || self.registered_child_ids().contains(&child_sid)
            {
                drop(operation);
                continue;
            }
            let parent_model = self.session_manager.read_model(&parent_sid).await?;
            if !parent_model.agent_sessions.iter().any(|link| {
                link.child_session_id == child_sid && link.status == AgentSessionStatus::Running
            }) {
                drop(operation);
                continue;
            }
            scheduler.abort_current_in_operation(&child_sid).await?;
            self.record_failed(&parent_sid, &child_sid, "aborted")
                .await?;
            scheduler
                .recycle_settled_session_in_operation(operation)
                .await?;
        }
        Ok(())
    }
}

fn map_recycled_access_error(error: SessionManagerError) -> SessionApiError {
    match error {
        SessionManagerError::Storage(astrcode_storage::StorageError::NotFound(_)) => {
            SessionApiError::NotFound(error.to_string())
        },
        SessionManagerError::Storage(astrcode_storage::StorageError::Unsupported(reason)) => {
            SessionApiError::Unsupported(reason)
        },
        error => SessionApiError::internal(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_payload_uses_matching_child_and_final_session_ids() {
        let child = SessionId::from("child-session");
        match astrcode_session::payload::agent_session_completed_payload(
            child.clone(),
            "done".into(),
        ) {
            DurableEventPayload::AgentSessionCompleted {
                child_session_id,
                final_session_id,
                ..
            } => {
                assert_eq!(child_session_id, child);
                assert_eq!(final_session_id, child);
            },
            _ => panic!("expected AgentSessionCompleted"),
        }
    }
}
