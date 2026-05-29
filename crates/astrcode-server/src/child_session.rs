//! 子 agent session 的 server 侧 owner：spawn、turn 提交、completion guard、终态与回收。

use std::{collections::HashMap, sync::Arc, time::Duration};

use astrcode_core::{
    event::EventPayload,
    tool::{CreateSessionRequest, SessionApiError},
    types::{SessionId, TurnId},
};
use astrcode_session::turn_handle::TurnHandle;
use parking_lot::Mutex;
use tokio::sync::{mpsc, watch};

use crate::{
    session_manager::SessionManager,
    turn_scheduler::{ExecutionCompletion, InputDelivery, TurnScheduler},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildOutcome {
    Completed { summary: String },
    Failed { error: String },
    Aborted,
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildCleanup {
    Recycle,
    Keep,
}

#[derive(Debug, Clone)]
pub struct ChildSessionCompletionConfig {
    pub child_session_id: SessionId,
    pub parent_session_id: SessionId,
    pub cleanup: ChildCleanup,
    pub notify_on_complete: Option<String>,
}

struct ChildSessionTracker {
    guards: Mutex<Vec<Arc<ChildSessionCompletionGuard>>>,
}

impl ChildSessionTracker {
    fn new() -> Self {
        Self {
            guards: Mutex::new(Vec::new()),
        }
    }

    fn register(&self, guard: Arc<ChildSessionCompletionGuard>) {
        self.guards.lock().push(guard);
    }

    fn collect_completed(&self) -> Vec<Arc<ChildSessionCompletionGuard>> {
        let mut guards = self.guards.lock();
        let (done, pending): (Vec<_>, Vec<_>) = guards
            .drain(..)
            .partition(|g| g.outcome_rx.borrow().is_some());
        *guards = pending;
        done
    }

    fn abort_all_direct(&self) -> Vec<Arc<ChildSessionCompletionGuard>> {
        let guards: Vec<_> = self.guards.lock().drain(..).collect();
        for guard in &guards {
            guard.abort();
        }
        guards
    }
}

/// 子 agent session 完成、turn 提交与回收的 server 侧协调者。
pub struct ChildSessionCoordinator {
    session_manager: Arc<SessionManager>,
    by_parent: Mutex<HashMap<SessionId, ChildSessionTracker>>,
    completed_tx: mpsc::UnboundedSender<SessionId>,
    completed_rx: Mutex<Option<mpsc::UnboundedReceiver<SessionId>>>,
}

impl ChildSessionCoordinator {
    pub fn new(session_manager: Arc<SessionManager>) -> Self {
        let (completed_tx, completed_rx) = mpsc::unbounded_channel();
        Self {
            session_manager,
            by_parent: Mutex::new(HashMap::new()),
            completed_tx,
            completed_rx: Mutex::new(Some(completed_rx)),
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
        tokio::spawn(async move {
            while let Some(parent_sid) = rx.recv().await {
                coordinator
                    .drain_completed(scheduler.as_ref(), &parent_sid)
                    .await;
            }
        });
    }

    pub fn session_manager(&self) -> &Arc<SessionManager> {
        &self.session_manager
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
            match model.parent_session_id {
                Some(parent) => {
                    if &parent == caller {
                        return Ok(());
                    }
                    current = parent;
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
                .map_err(|e| SessionApiError::Internal(format!("read session: {e}")))?;
            match model.parent_session_id {
                Some(parent) => {
                    depth += 1;
                    current = parent;
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
        let max_depth = self
            .session_manager
            .config()
            .read_effective()
            .agent
            .max_depth;
        if depth >= max_depth {
            return Err(SessionApiError::MaxDepthExceeded {
                current: depth,
                max: max_depth,
            });
        }

        let parent_model = parent_session
            .read_model()
            .await
            .map_err(|e| SessionApiError::Internal(e.to_string()))?;

        let working_dir = request.working_dir.unwrap_or(parent_model.working_dir);
        let model_id = request
            .model_preference
            .filter(|m| m != "inherit" && !m.is_empty())
            .unwrap_or(parent_model.model_id);

        let child = parent_session
            .spawn_child(
                &working_dir,
                &model_id,
                request.name,
                String::new(),
                request.system_prompt,
                request.tool_policy,
                request.source_extension.as_deref(),
                request.tool_call_id.into(),
            )
            .await
            .map_err(|e| SessionApiError::Internal(format!("spawn child: {e}")))?;

        self.session_manager.register_child_session(&child);
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
            .start_with_completion(target_sid.clone(), user_prompt)
            .await
            .map_err(|e| SessionApiError::Internal(format!("submit: {e}")))?;

        let turn_id = started.turn_id.clone();
        let result = started.handle.wait().await;
        let next = scheduler
            .finish_execution(ExecutionCompletion {
                session_id: target_sid.clone(),
                turn_id,
            })
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
                    return Err(SessionApiError::Internal(format!("turn error: {e}")));
                },
            },
            None => {
                self.record_failed(caller_sid, target_sid, "turn task panicked")
                    .await;
                return Err(SessionApiError::Internal("turn task panicked".into()));
            },
        };

        self.drain_completed(scheduler, caller_sid).await;
        Ok(content)
    }

    /// 后台 turn：注册 completion guard，并 drain 父 session 上已完成的 child。
    pub async fn submit_turn_background(
        &self,
        scheduler: &TurnScheduler,
        caller_sid: &SessionId,
        target_sid: &SessionId,
        user_prompt: String,
        cleanup: ChildCleanup,
        notify_on_complete: Option<String>,
    ) -> Result<(TurnId, SessionId), SessionApiError> {
        self.prepare_turn_target(target_sid).await?;
        let started = scheduler
            .start_with_completion(target_sid.clone(), user_prompt)
            .await
            .map_err(|e| SessionApiError::Internal(format!("submit: {e}")))?;

        let turn_id = started.turn_id.clone();
        let config = ChildSessionCompletionConfig {
            child_session_id: target_sid.clone(),
            parent_session_id: caller_sid.clone(),
            cleanup,
            notify_on_complete,
        };
        self.register_completion_guard(started.handle, config);
        self.drain_completed(scheduler, caller_sid).await;
        Ok((turn_id, target_sid.clone()))
    }

    pub async fn record_completed(
        &self,
        parent_sid: &SessionId,
        child_sid: &SessionId,
        summary: &str,
    ) {
        write_agent_completed(&self.session_manager, parent_sid, child_sid, summary).await;
    }

    pub async fn record_failed(&self, parent_sid: &SessionId, child_sid: &SessionId, error: &str) {
        write_agent_failed(&self.session_manager, parent_sid, child_sid, error).await;
    }

    pub async fn recycle_child(
        &self,
        scheduler: &TurnScheduler,
        parent_sid: &SessionId,
        child_sid: &SessionId,
    ) {
        scheduler.release_completed_execution(child_sid).await;
        if let Err(e) = self.session_manager.recycle_session(child_sid).await {
            tracing::warn!(
                session_id = %child_sid,
                error = %e,
                "failed to recycle session"
            );
            return;
        }
        if let Ok(parent_session) = self.session_manager.open(parent_sid.clone()).await {
            if let Err(e) = parent_session
                .append_event(astrcode_core::event::Event::new(
                    parent_sid.clone(),
                    None,
                    EventPayload::AgentSessionRecycled {
                        child_session_id: child_sid.clone(),
                    },
                ))
                .await
            {
                tracing::warn!(
                    parent_session_id = %parent_sid,
                    child_session_id = %child_sid,
                    error = %e,
                    "failed to append AgentSessionRecycled event"
                );
            }
            scheduler.sync_durable_events(parent_sid).await;
        }
    }

    pub async fn drain_completed(&self, scheduler: &TurnScheduler, parent_sid: &SessionId) {
        let completed = self.drain_completed_guards(parent_sid);
        for guard in completed {
            self.write_terminal_for_guard(&guard).await;
            if guard.cleanup_policy() == ChildCleanup::Recycle {
                self.recycle_child(
                    scheduler,
                    guard.parent_session_id(),
                    guard.child_session_id(),
                )
                .await;
            } else {
                scheduler.registry().remove(guard.child_session_id());
            }
            if let Some(notify_text) = guard.notify_text() {
                if let Err(e) = scheduler
                    .deliver_input(
                        guard.parent_session_id().clone(),
                        notify_text.to_string(),
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

    pub fn register_completion_guard(
        &self,
        handle: TurnHandle,
        config: ChildSessionCompletionConfig,
    ) {
        let parent_sid = config.parent_session_id.clone();
        let guard = ChildSessionCompletionGuard::spawn(handle, config, self.completed_tx.clone());
        self.register_guard(&parent_sid, Arc::new(guard));
    }

    pub async fn cascade_abort_children(&self, scheduler: &TurnScheduler, parent_sid: &SessionId) {
        let guards = self
            .collect_guards_deep(parent_sid, Duration::from_secs(10))
            .await;
        if guards.is_empty() {
            return;
        }
        self.finalize_aborted_children(scheduler, &guards).await;
    }

    async fn prepare_turn_target(&self, target_sid: &SessionId) -> Result<(), SessionApiError> {
        let session = self
            .session_manager
            .open(target_sid.clone())
            .await
            .map_err(|e| SessionApiError::NotFound(e.to_string()))?;
        session
            .ensure_runtime_ready()
            .await
            .map_err(|e| SessionApiError::Internal(format!("runtime init: {e}")))?;
        Ok(())
    }

    fn register_guard(&self, parent_sid: &SessionId, guard: Arc<ChildSessionCompletionGuard>) {
        self.by_parent
            .lock()
            .entry(parent_sid.clone())
            .or_insert_with(ChildSessionTracker::new)
            .register(guard);
    }

    fn drain_completed_guards(
        &self,
        parent_sid: &SessionId,
    ) -> Vec<Arc<ChildSessionCompletionGuard>> {
        let mut map = self.by_parent.lock();
        let Some(tracker) = map.get_mut(parent_sid) else {
            return Vec::new();
        };
        tracker.collect_completed()
    }

    fn abort_all_direct(&self, parent_sid: &SessionId) -> Vec<Arc<ChildSessionCompletionGuard>> {
        let mut map = self.by_parent.lock();
        let Some(tracker) = map.get_mut(parent_sid) else {
            return Vec::new();
        };
        tracker.abort_all_direct()
    }

    async fn write_terminal_for_guard(&self, guard: &ChildSessionCompletionGuard) {
        let parent_sid = guard.parent_session_id();
        let child_sid = guard.child_session_id();
        match guard.outcome().await {
            ChildOutcome::Completed { summary } => {
                self.record_completed(parent_sid, child_sid, &summary).await;
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
            let error = match guard.outcome().await {
                ChildOutcome::TimedOut => "abort timed out",
                _ => "aborted",
            };
            self.record_failed(parent_sid, child_sid, error).await;
            self.recycle_child(scheduler, parent_sid, child_sid).await;
        }
    }
}

/// 只等待 `TurnHandle` 并记录 outcome；不写父 session 事件。
pub struct ChildSessionCompletionGuard {
    config: ChildSessionCompletionConfig,
    outcome_tx: watch::Sender<Option<ChildOutcome>>,
    outcome_rx: watch::Receiver<Option<ChildOutcome>>,
    _abort_handle: tokio::task::AbortHandle,
}

fn try_set_outcome(tx: &watch::Sender<Option<ChildOutcome>>, outcome: ChildOutcome) {
    let _ = tx.send_if_modified(|cur| {
        if cur.is_none() {
            *cur = Some(outcome);
            true
        } else {
            false
        }
    });
}

impl ChildSessionCompletionGuard {
    pub fn spawn(
        handle: TurnHandle,
        config: ChildSessionCompletionConfig,
        completed_tx: mpsc::UnboundedSender<SessionId>,
    ) -> Self {
        let (outcome_tx, outcome_rx) = watch::channel(None);
        let outcome_tx_for_task = outcome_tx.clone();
        let abort_handle = handle.abort_handle();
        let parent_sid = config.parent_session_id.clone();

        tokio::spawn(async move {
            let result = handle.wait().await;
            let outcome = match result {
                Some(r) => match r.output {
                    Ok(out) => ChildOutcome::Completed {
                        summary: one_line_summary(&out.text),
                    },
                    Err(e) => ChildOutcome::Failed {
                        error: e.to_string(),
                    },
                },
                None => ChildOutcome::Aborted,
            };
            try_set_outcome(&outcome_tx_for_task, outcome);
            let _ = completed_tx.send(parent_sid);
        });

        Self {
            config,
            outcome_tx,
            outcome_rx,
            _abort_handle: abort_handle,
        }
    }

    pub async fn outcome(&self) -> ChildOutcome {
        if let Some(outcome) = self.outcome_rx.borrow().clone() {
            return outcome;
        }
        let mut rx = self.outcome_rx.clone();
        let result = rx.wait_for(|v| v.is_some()).await;
        match result {
            Ok(ref_val) => {
                let val: &Option<ChildOutcome> = &ref_val;
                val.clone().unwrap_or(ChildOutcome::Aborted)
            },
            Err(_) => ChildOutcome::Aborted,
        }
    }

    pub fn abort(&self) {
        self._abort_handle.abort();
        try_set_outcome(&self.outcome_tx, ChildOutcome::Aborted);
    }

    pub fn force_timeout(&self) {
        try_set_outcome(&self.outcome_tx, ChildOutcome::TimedOut);
    }

    pub fn child_session_id(&self) -> &SessionId {
        &self.config.child_session_id
    }

    pub fn parent_session_id(&self) -> &SessionId {
        &self.config.parent_session_id
    }

    pub fn cleanup_policy(&self) -> ChildCleanup {
        self.config.cleanup
    }

    pub fn notify_text(&self) -> Option<&str> {
        self.config.notify_on_complete.as_deref()
    }
}

async fn write_agent_completed(
    session_manager: &Arc<SessionManager>,
    parent_sid: &SessionId,
    child_sid: &SessionId,
    summary: &str,
) {
    if let Ok(parent_session) = session_manager.open(parent_sid.clone()).await {
        if let Err(e) = parent_session
            .append_event(astrcode_core::event::Event::new(
                parent_sid.clone(),
                None,
                astrcode_session::payload::agent_session_completed_payload(
                    child_sid.clone(),
                    one_line_summary(summary),
                ),
            ))
            .await
        {
            tracing::warn!(
                parent_session_id = %parent_sid,
                child_session_id = %child_sid,
                error = %e,
                "failed to append AgentSessionCompleted event"
            );
        }
    }
}

async fn write_agent_failed(
    session_manager: &Arc<SessionManager>,
    parent_sid: &SessionId,
    child_sid: &SessionId,
    error: &str,
) {
    if let Ok(parent_session) = session_manager.open(parent_sid.clone()).await {
        if let Err(e) = parent_session
            .append_event(astrcode_core::event::Event::new(
                parent_sid.clone(),
                None,
                astrcode_session::payload::agent_session_failed_payload(
                    child_sid.clone(),
                    error.to_string(),
                ),
            ))
            .await
        {
            tracing::warn!(
                parent_session_id = %parent_sid,
                child_session_id = %child_sid,
                error = %e,
                "failed to append AgentSessionFailed event"
            );
        }
    }
}

fn one_line_summary(text: &str) -> String {
    astrcode_support::text::compact_inline(text, 159)
}

#[cfg(test)]
mod tests {
    use astrcode_core::event::EventPayload;

    use super::*;

    #[test]
    fn try_set_outcome_is_first_write_wins() {
        let (tx, rx) = watch::channel(None);
        try_set_outcome(
            &tx,
            ChildOutcome::Completed {
                summary: "first".into(),
            },
        );
        try_set_outcome(
            &tx,
            ChildOutcome::Failed {
                error: "second".into(),
            },
        );
        assert_eq!(
            rx.borrow().clone(),
            Some(ChildOutcome::Completed {
                summary: "first".into(),
            })
        );
    }

    #[test]
    fn terminal_payload_uses_matching_child_and_final_session_ids() {
        let child = SessionId::from("child-session");
        match astrcode_session::payload::agent_session_completed_payload(
            child.clone(),
            "done".into(),
        ) {
            EventPayload::AgentSessionCompleted {
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
