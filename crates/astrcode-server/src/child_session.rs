//! 子 agent session 的 server 侧 owner：spawn、turn 提交、completion guard、终态与回收。

mod completion;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use astrcode_core::{
    event::DurableEventPayload,
    tool::{CreateSessionRequest, SessionApiError},
    types::{SessionId, TurnId},
};
use astrcode_session::TurnHandle;
use astrcode_session_projection::AgentSessionStatus;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use self::completion::{
    ChildSessionCompletionGuard, build_background_agent_notification, write_agent_completed,
    write_agent_failed,
};
use crate::{
    session_manager::SessionManager,
    turn_scheduler::{CompletedRecycleOutcome, InputDelivery, TurnScheduler},
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChildOutcome {
    Completed { output: String },
    Failed { error: String },
    Aborted,
    TimedOut,
}

const CHILD_SESSION_COMPLETE_CAPACITY: usize = 256;

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

/// 子 agent session 完成、turn 提交与回收的 server 侧协调者。
pub struct ChildSessionCoordinator {
    session_manager: Arc<SessionManager>,
    by_parent: Mutex<HashMap<SessionId, CompletionGuards>>,
    completed_tx: mpsc::Sender<SessionId>,
    completed_rx: Mutex<Option<mpsc::Receiver<SessionId>>>,
    watcher_shutdown: CancellationToken,
    watcher: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl ChildSessionCoordinator {
    pub fn new(session_manager: Arc<SessionManager>) -> Self {
        let (completed_tx, completed_rx) = mpsc::channel(CHILD_SESSION_COMPLETE_CAPACITY);
        Self {
            session_manager,
            by_parent: Mutex::new(HashMap::new()),
            completed_tx,
            completed_rx: Mutex::new(Some(completed_rx)),
            watcher_shutdown: CancellationToken::new(),
            watcher: Mutex::new(None),
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
        let watcher = tokio::spawn(async move {
            loop {
                tokio::select! {
                    parent_sid = rx.recv() => {
                        let Some(parent_sid) = parent_sid else {
                            break;
                        };
                        coordinator
                            .drain_completed(scheduler.as_ref(), &parent_sid)
                            .await;
                    },
                    _ = shutdown.cancelled() => {
                        while let Ok(parent_sid) = rx.try_recv() {
                            coordinator
                                .drain_completed(scheduler.as_ref(), &parent_sid)
                                .await;
                        }
                        break;
                    },
                }
            }
        });
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

    pub async fn verify_access(
        &self,
        caller: &SessionId,
        target: &SessionId,
    ) -> Result<(), SessionApiError> {
        if caller == target {
            return Ok(());
        }
        let mut current = target.clone();
        loop {
            let model = self
                .session_manager
                .read_model(&current)
                .await
                .map_err(|e| SessionApiError::NotFound(e.to_string()))?;
            match &model.identity.parent {
                Some(parent) => {
                    if &parent.session_id == caller {
                        return Ok(());
                    }
                    current = parent.session_id.clone();
                },
                None => {
                    return Err(SessionApiError::PermissionDenied(format!(
                        "session {target} is not a descendant of {caller}"
                    )));
                },
            }
        }
    }

    pub async fn session_depth(&self, session_id: &SessionId) -> Result<usize, SessionApiError> {
        let mut depth = 0;
        let mut current = session_id.clone();
        loop {
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

    pub async fn spawn_child(
        &self,
        parent_session_id: &SessionId,
        request: CreateSessionRequest,
    ) -> Result<astrcode_session::Session, SessionApiError> {
        let parent_session = self
            .session_manager
            .open(parent_session_id.clone())
            .await
            .map_err(|e| SessionApiError::NotFound(format!("parent: {e}")))?;

        let depth = self.session_depth(parent_session_id).await?;
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

        let child = parent_session
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
            .await
            .map_err(SessionApiError::internal)?;

        Ok(child)
    }

    /// 同步等待 turn 结束，写终态并 drain 父 session 上已完成的 child guard。
    pub async fn submit_turn_sync(
        &self,
        scheduler: &TurnScheduler,
        caller_sid: &SessionId,
        target_sid: &SessionId,
        user_prompt: String,
    ) -> Result<String, SessionApiError> {
        self.prepare_turn_target(target_sid).await?;
        let started = scheduler
            .start_with_completion(
                target_sid.clone(),
                astrcode_core::user_input::UserInput::text_only(user_prompt),
            )
            .await
            .map_err(SessionApiError::internal)?;

        let turn_id = started.turn_id;
        let result = started.handle.wait().await;
        let next = scheduler
            .finish_and_maybe_start_next(target_sid, &turn_id)
            .await;
        scheduler.watch_queued_if_any(target_sid.clone(), next);

        let content = match result {
            Some(r) => match r.output {
                Ok(out) => {
                    self.record_completed(caller_sid, target_sid, &out.text)
                        .await;
                    out.text
                },
                Err(e) => {
                    self.record_failed(caller_sid, target_sid, &e.to_string())
                        .await;
                    return Err(SessionApiError::internal(e));
                },
            },
            None => {
                self.record_failed(caller_sid, target_sid, "turn task panicked")
                    .await;
                return Err(SessionApiError::internal_msg("turn task panicked"));
            },
        };

        self.drain_completed(scheduler, caller_sid).await;
        Ok(content)
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
        self.prepare_turn_target(target_sid).await?;
        let started = scheduler
            .start_with_completion(
                target_sid.clone(),
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
        self.register_completion_guard(started.handle, config);
        self.drain_completed(scheduler, caller_sid).await;
        Ok((turn_id, target_sid.clone()))
    }

    async fn record_completed(&self, parent_sid: &SessionId, child_sid: &SessionId, summary: &str) {
        write_agent_completed(&self.session_manager, parent_sid, child_sid, summary).await;
    }

    async fn record_failed(&self, parent_sid: &SessionId, child_sid: &SessionId, error: &str) {
        write_agent_failed(&self.session_manager, parent_sid, child_sid, error).await;
    }

    pub async fn recycle_child(
        &self,
        scheduler: &TurnScheduler,
        parent_sid: &SessionId,
        child_sid: &SessionId,
    ) {
        let result = scheduler.recycle_session(child_sid).await;
        self.record_child_recycled(parent_sid, child_sid, result)
            .await;
    }

    async fn recycle_completed_child(
        &self,
        scheduler: &TurnScheduler,
        parent_sid: &SessionId,
        child_sid: &SessionId,
        turn_id: &TurnId,
    ) {
        match scheduler
            .recycle_completed_session(child_sid, turn_id)
            .await
        {
            Ok(CompletedRecycleOutcome::Recycled) => {
                self.record_child_recycled(parent_sid, child_sid, Ok(()))
                    .await;
            },
            Ok(CompletedRecycleOutcome::StaleCompletion) => {
                tracing::debug!(
                    session_id = %child_sid,
                    turn_id = %turn_id,
                    "ignored stale child completion during recycle"
                );
            },
            Err(error) => {
                self.record_child_recycled(parent_sid, child_sid, Err(error))
                    .await;
            },
        }
    }

    async fn record_child_recycled(
        &self,
        parent_sid: &SessionId,
        child_sid: &SessionId,
        result: Result<(), crate::turn_scheduler::TurnScheduleError>,
    ) {
        if let Err(e) = result {
            tracing::warn!(
                session_id = %child_sid,
                error = %e,
                "failed to recycle session"
            );
            return;
        }
        if let Ok(parent_session) = self.session_manager.open(parent_sid.clone()).await {
            if let Err(e) = parent_session
                .emit_durable(
                    None,
                    DurableEventPayload::AgentSessionRecycled {
                        child_session_id: child_sid.clone(),
                    },
                )
                .await
            {
                tracing::warn!(
                    parent_session_id = %parent_sid,
                    child_session_id = %child_sid,
                    error = %e,
                    "failed to append AgentSessionRecycled event"
                );
            }
            self.session_manager.sync_durable_events(parent_sid).await;
        }
    }

    pub async fn drain_completed(&self, scheduler: &TurnScheduler, parent_sid: &SessionId) {
        let completed = self.drain_completed_guards(parent_sid);
        for guard in completed {
            self.write_terminal_for_guard(&guard).await;
            if guard.cleanup_policy() == ChildCleanup::Recycle {
                self.recycle_completed_child(
                    scheduler,
                    guard.parent_session_id(),
                    guard.child_session_id(),
                    guard.turn_id(),
                )
                .await;
            } else {
                scheduler
                    .release_completed_execution(guard.child_session_id(), guard.turn_id())
                    .await;
            }
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

    fn register_completion_guard(&self, handle: TurnHandle, config: ChildSessionCompletionConfig) {
        let parent_sid = config.parent_session_id.clone();
        let guard = ChildSessionCompletionGuard::spawn(handle, config, self.completed_tx.clone());
        self.by_parent
            .lock()
            .entry(parent_sid)
            .or_default()
            .push(Arc::new(guard));
    }

    pub async fn cascade_abort_children(&self, scheduler: &TurnScheduler, parent_sid: &SessionId) {
        let guards = self
            .collect_guards_deep(parent_sid, Duration::from_secs(10))
            .await;
        if !guards.is_empty() {
            self.finalize_aborted_children(scheduler, &guards).await;
        }
        let guarded_children: HashSet<SessionId> = guards
            .iter()
            .map(|guard| guard.child_session_id().clone())
            .collect();
        self.abort_unguarded_running_children(scheduler, parent_sid, &guarded_children)
            .await;
    }

    async fn prepare_turn_target(&self, target_sid: &SessionId) -> Result<(), SessionApiError> {
        self.session_manager
            .open(target_sid.clone())
            .await
            .map_err(|e| SessionApiError::NotFound(e.to_string()))?;
        Ok(())
    }

    fn drain_completed_guards(&self, parent_sid: &SessionId) -> CompletionGuards {
        let mut by_parent = self.by_parent.lock();
        let Some((parent_key, guards)) = by_parent.remove_entry(parent_sid) else {
            return Vec::new();
        };
        let (completed, pending): (CompletionGuards, CompletionGuards) =
            guards.into_iter().partition(|guard| guard.is_complete());
        if !pending.is_empty() {
            by_parent.insert(parent_key, pending);
        }
        completed
    }

    fn abort_all_direct(&self, parent_sid: &SessionId) -> CompletionGuards {
        let guards = self.by_parent.lock().remove(parent_sid).unwrap_or_default();
        for guard in &guards {
            guard.request_shutdown();
        }
        guards
    }

    async fn write_terminal_for_guard(&self, guard: &ChildSessionCompletionGuard) {
        let parent_sid = guard.parent_session_id();
        let child_sid = guard.child_session_id();
        match guard.outcome().await {
            ChildOutcome::Completed { output } => {
                self.record_completed(parent_sid, child_sid, &output).await;
            },
            ChildOutcome::Failed { error } => {
                self.record_failed(parent_sid, child_sid, &error).await;
            },
            ChildOutcome::Aborted => {
                self.record_failed(parent_sid, child_sid, "aborted").await;
            },
            ChildOutcome::TimedOut => {
                self.record_failed(parent_sid, child_sid, "timed out").await;
            },
        }
    }

    async fn collect_guards_deep(
        &self,
        root_sid: &SessionId,
        timeout: Duration,
    ) -> Vec<Arc<ChildSessionCompletionGuard>> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut all_guards: Vec<Arc<ChildSessionCompletionGuard>> = Vec::new();
        let mut stack: Vec<SessionId> = vec![root_sid.clone()];

        while let Some(sid) = stack.pop() {
            let guards = self.abort_all_direct(&sid);
            if guards.is_empty() {
                continue;
            }
            for guard in &guards {
                stack.push(guard.child_session_id().clone());
            }
            all_guards.extend(guards);
        }

        // 反向迭代：子 session 先于父 session 被等待，避免父节点提前结束而子节点仍悬挂。
        for guard in all_guards.iter().rev() {
            if tokio::time::timeout_at(deadline, guard.outcome())
                .await
                .is_err()
            {
                tracing::warn!(
                    child_session_id = %guard.child_session_id(),
                    timeout_ms = timeout.as_millis(),
                    "cascade abort: child session timed out"
                );
                guard.force_timeout();
            }
        }

        all_guards
    }

    async fn finalize_aborted_children(
        &self,
        scheduler: &TurnScheduler,
        guards: &[Arc<ChildSessionCompletionGuard>],
    ) {
        for guard in guards.iter().rev() {
            let child_sid = guard.child_session_id();
            let parent_sid = guard.parent_session_id();
            let outcome = guard.outcome().await;
            let timed_out = outcome == ChildOutcome::TimedOut;
            let error = if timed_out {
                "abort timed out"
            } else {
                "aborted"
            };
            self.record_failed(parent_sid, child_sid, error).await;
            if timed_out {
                self.recycle_child(scheduler, parent_sid, child_sid).await;
            } else {
                self.recycle_completed_child(scheduler, parent_sid, child_sid, guard.turn_id())
                    .await;
            }
        }
    }

    /// 同步子 agent（`submit_turn_sync`）不注册 completion guard，需按 session 树投影中止。
    async fn abort_unguarded_running_children(
        &self,
        scheduler: &TurnScheduler,
        root_sid: &SessionId,
        guarded_children: &HashSet<SessionId>,
    ) {
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
            self.record_failed(&parent_sid, &child_sid, "aborted").await;
            self.recycle_child(scheduler, &parent_sid, &child_sid).await;
        }
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
