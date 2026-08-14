use std::{collections::HashSet, sync::Arc, time::Duration};

use astrcode_core::{
    event::{DurableEventPayload, Phase},
    types::{SessionId, TurnId, new_message_id},
    user_input::UserInput,
};
use astrcode_session::{
    InterruptedToolOutcome, Session, TurnFinalization, emit_interrupted_tool_results,
    emit_turn_aborted_context, finalize_aborted_turn, finalize_turn,
    payload::{TURN_FINISH_INTERRUPTED, agent_run_completed_payload, turn_completed_payload},
};
use astrcode_session_projection::{AgentSessionStatus, SessionReadModel};
use astrcode_storage::StorageError;

#[cfg(any(test, feature = "testing"))]
use super::CompletedRecycleOutcome;
use super::{
    ABORT_WAIT_EXTRA_MS, ABORT_WAIT_POLL_MS, FORCE_KILL_GRACE_MS, TurnScheduleError, TurnScheduler,
};
use crate::{
    delivery_gates::{SessionClosure, SessionOperationGuard},
    session_manager::SessionManagerError,
};

#[derive(Clone, Copy)]
enum SessionTreeCloseAction {
    Delete,
    Recycle,
}

struct FrozenSessionNode {
    session_id: SessionId,
    parent_session_id: Option<SessionId>,
    exists: bool,
    closure: SessionClosure,
}

struct FrozenSessionTree {
    nodes: Vec<FrozenSessionNode>,
    running_children: Vec<(SessionId, SessionId)>,
    visited: HashSet<SessionId>,
}

impl FrozenSessionTree {
    fn new(root_session_id: SessionId, root_closure: SessionClosure) -> Self {
        Self {
            nodes: vec![FrozenSessionNode {
                session_id: root_session_id.clone(),
                parent_session_id: None,
                exists: true,
                closure: root_closure,
            }],
            running_children: Vec::new(),
            visited: HashSet::from([root_session_id]),
        }
    }

    fn session_ids(&self) -> Vec<SessionId> {
        self.nodes
            .iter()
            .map(|node| node.session_id.clone())
            .collect()
    }

    fn finish(self) {
        for node in self.nodes {
            node.closure.finish();
        }
    }
}

impl TurnScheduler {
    /// 中止活跃 turn（含级联子 session）。
    pub async fn abort(&self, session_id: &SessionId) -> Result<bool, TurnScheduleError> {
        let operation = self.begin_session_operation(session_id).await?;
        operation.wait_for_starts().await;
        let had_active = self.registry.has_active(session_id);
        let cancelled_descendant = self.abort_in_operation(session_id).await?;
        let next = self.reserve_next_pending(&operation).await?;
        drop(operation);
        if let Some((reserved, entry)) = next {
            self.start_reserved_pending_detached(reserved, entry, "abort-queue-drain")
                .await?;
        }
        Ok(had_active || cancelled_descendant)
    }

    pub(crate) async fn abort_in_operation(
        &self,
        session_id: &SessionId,
    ) -> Result<bool, TurnScheduleError> {
        let cancelled_descendant = self
            .child_sessions
            .cascade_abort_children(self, session_id)
            .await?;
        self.abort_current_in_operation(session_id).await?;
        Ok(cancelled_descendant)
    }

    pub(crate) async fn abort_current_in_operation(
        &self,
        session_id: &SessionId,
    ) -> Result<(), TurnScheduleError> {
        if self.registry.has_active(session_id) {
            let deadline = tokio::time::Instant::now()
                + Duration::from_millis(FORCE_KILL_GRACE_MS + ABORT_WAIT_EXTRA_MS);
            let mut shutdown_turn = None;
            while self.registry.has_active(session_id) && tokio::time::Instant::now() < deadline {
                if self.registry.active_is_finished(session_id) {
                    let turn_id = self
                        .registry
                        .active_turn_id(session_id)
                        .ok_or(TurnScheduleError::NoActiveTurn)?;
                    let finalization = aborted_finalization();
                    self.settle_finished_execution_locked(
                        session_id,
                        &turn_id,
                        Some(&finalization),
                    )
                    .await?;
                    break;
                }
                let active_turn = self.registry.active_turn_id(session_id);
                if active_turn != shutdown_turn
                    && let Some(turn_id) = self.registry.request_shutdown(session_id)
                {
                    shutdown_turn = Some(turn_id);
                }
                tokio::time::sleep(Duration::from_millis(ABORT_WAIT_POLL_MS)).await;
            }
            if self.registry.has_active(session_id) {
                let turn_id = self
                    .registry
                    .active_turn_id(session_id)
                    .ok_or(TurnScheduleError::NoActiveTurn)?;
                if self.registry.active_is_finished(session_id) {
                    let finalization = aborted_finalization();
                    self.settle_finished_execution_locked(
                        session_id,
                        &turn_id,
                        Some(&finalization),
                    )
                    .await?;
                } else if let Some((turn_id, session)) =
                    self.registry.force_kill_if_running(session_id, &turn_id)
                {
                    tracing::warn!(
                        %session_id,
                        %turn_id,
                        "turn did not stop after cooperative shutdown; forced kill"
                    );
                    self.emit_turn_aborted(&turn_id, &session, session_id)
                        .await?;
                }
            }
        }

        let session = self
            .session_manager
            .open(session_id.clone())
            .await
            .map_err(|_| TurnScheduleError::SessionNotFound(session_id.to_string()))?;
        let state = session
            .read_model()
            .await
            .map_err(TurnScheduleError::Session)?;
        if matches!(state.execution.phase, Phase::Idle | Phase::Error) {
            return Ok(());
        }

        self.repair_stale_locked(session_id).await
    }

    /// completion 已产出但 task 可能尚未退出；只按 turn identity 非破坏性移除。
    #[cfg(any(test, feature = "testing"))]
    pub(crate) async fn release_completed_execution(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        finalization: Option<&TurnFinalization>,
    ) {
        let operation = self.delivery_gates.lock(session_id).await;
        if let Err(error) = self
            .release_completed_execution_in_operation(&operation, turn_id, finalization)
            .await
        {
            tracing::error!(
                %session_id,
                %turn_id,
                %error,
                "failed to settle completed execution; registry ownership retained"
            );
        }
    }

    pub(crate) async fn release_completed_execution_in_operation(
        &self,
        operation: &SessionOperationGuard,
        turn_id: &TurnId,
        finalization: Option<&TurnFinalization>,
    ) -> Result<bool, TurnScheduleError> {
        let session_id = operation.session_id();
        self.sync_durable_events_required(session_id).await?;
        self.settle_finished_execution_locked(session_id, turn_id, finalization)
            .await
    }

    pub(super) async fn settle_finished_execution_locked(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        finalization: Option<&TurnFinalization>,
    ) -> Result<bool, TurnScheduleError> {
        if self.registry.active_turn_id(session_id).as_ref() != Some(turn_id) {
            return Ok(false);
        }

        let recorded_finalization = self.registry.active_finalization(session_id);
        match recorded_finalization.as_ref().or(finalization) {
            Some(finalization) if !finalization.terminal_persisted => {
                let session = self
                    .session_manager
                    .open(session_id.clone())
                    .await
                    .map_err(TurnScheduleError::SessionManager)?;
                finalize_turn(&session, turn_id, finalization)
                    .await
                    .map_err(TurnScheduleError::EventEmit)?;
                self.session_manager
                    .sync_durable_events_required(session_id)
                    .await?;
            },
            Some(_) => {},
            None => self.repair_finished_turn_state(session_id).await?,
        }
        Ok(self
            .registry
            .remove_if_matches(session_id, turn_id)
            .is_some())
    }

    /// 清理 session 的 execution 与待处理输入；仍在运行的 turn 会被中止。
    pub async fn cleanup_execution(&self, session_id: &SessionId) {
        let operation = self.delivery_gates.lock(session_id).await;
        operation.wait_for_starts().await;
        if let Err(error) = self.cleanup_execution_locked(session_id).await {
            tracing::error!(%session_id, %error, "failed to clean up session execution");
        }
    }

    pub(super) async fn cleanup_execution_locked(
        &self,
        session_id: &SessionId,
    ) -> Result<bool, TurnScheduleError> {
        loop {
            let Some(turn_id) = self.registry.active_turn_id(session_id) else {
                return Ok(false);
            };
            if self.registry.active_is_finished(session_id) {
                return self
                    .settle_finished_execution_locked(session_id, &turn_id, None)
                    .await;
            }
            if let Some((turn_id, session)) =
                self.registry.force_kill_if_running(session_id, &turn_id)
            {
                self.emit_turn_aborted(&turn_id, &session, session_id)
                    .await?;
                return Ok(true);
            }
            tokio::task::yield_now().await;
        }
    }

    pub async fn delete_session(&self, session_id: &SessionId) -> Result<(), TurnScheduleError> {
        self.close_session_tree(session_id.clone(), SessionTreeCloseAction::Delete)
            .await
    }

    pub async fn recycle_session(&self, session_id: &SessionId) -> Result<(), TurnScheduleError> {
        self.close_session_tree(session_id.clone(), SessionTreeCloseAction::Recycle)
            .await
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) async fn recycle_completed_session(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
        finalization: Option<&TurnFinalization>,
    ) -> Result<CompletedRecycleOutcome, TurnScheduleError> {
        let operation = self.begin_session_operation(session_id).await?;
        self.recycle_completed_session_in_operation(operation, turn_id, finalization)
            .await
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) async fn recycle_completed_session_in_operation(
        &self,
        operation: SessionOperationGuard,
        turn_id: &TurnId,
        finalization: Option<&TurnFinalization>,
    ) -> Result<CompletedRecycleOutcome, TurnScheduleError> {
        let session_id = operation.session_id().clone();
        self.sync_durable_events_required(&session_id).await?;
        if !self
            .settle_finished_execution_locked(&session_id, turn_id, finalization)
            .await?
        {
            return Ok(CompletedRecycleOutcome::StaleCompletion);
        }
        self.recycle_settled_session_in_operation(operation).await?;
        Ok(CompletedRecycleOutcome::Recycled)
    }

    pub(crate) async fn recycle_settled_session_in_operation(
        &self,
        operation: SessionOperationGuard,
    ) -> Result<(), TurnScheduleError> {
        let session_id = operation.session_id().clone();
        let root_closure = self.delivery_gates.close_operation(operation);
        self.run_close_session_tree(session_id, root_closure, SessionTreeCloseAction::Recycle)
            .await
    }

    async fn close_session_tree(
        &self,
        root_session_id: SessionId,
        action: SessionTreeCloseAction,
    ) -> Result<(), TurnScheduleError> {
        let admission = self.admit_owned()?;
        let scheduler = self.clone();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        admission.spawn_named("session_tree_close_owner", async move {
            let result = async {
                let root_closure = scheduler.delivery_gates.close(&root_session_id).await?;
                scheduler
                    .run_close_session_tree(root_session_id, root_closure, action)
                    .await
            }
            .await;
            let _ = result_tx.send(result);
        });
        wait_for_close_owner(result_rx).await
    }

    async fn run_close_session_tree(
        &self,
        root_session_id: SessionId,
        root_closure: SessionClosure,
        action: SessionTreeCloseAction,
    ) -> Result<(), TurnScheduleError> {
        let mut tree = FrozenSessionTree::new(root_session_id, root_closure);
        if let Err(error) = self.freeze_session_tree(&mut tree).await {
            tree.finish();
            return Err(error);
        }

        let result = self.finish_session_tree_close(&tree, action).await;
        tree.finish();
        result
    }

    /// 每个节点先关闭 gate，再读取其 child links；child spawn 全程持有 parent gate，
    /// 因此读取后该节点的直接子集合不会再增长。
    async fn freeze_session_tree(
        &self,
        tree: &mut FrozenSessionTree,
    ) -> Result<(), TurnScheduleError> {
        let mut next_node = 0;
        while next_node < tree.nodes.len() {
            let session_id = tree.nodes[next_node].session_id.clone();
            tree.nodes[next_node].closure.wait_for_starts().await;
            let model = match self.session_manager.read_model(&session_id).await {
                Ok(model) => model,
                Err(SessionManagerError::Storage(StorageError::NotFound(_)))
                    if tree.nodes[next_node].parent_session_id.is_some() =>
                {
                    tree.nodes[next_node].exists = false;
                    next_node += 1;
                    continue;
                },
                Err(error) => return Err(error.into()),
            };
            if next_node == 0 {
                tree.nodes[next_node].parent_session_id = model
                    .identity
                    .parent
                    .as_ref()
                    .map(|parent| parent.session_id.clone());
            }

            for link in &model.agent_sessions {
                let child_session_id = link.child_session_id.clone();
                if link.status == AgentSessionStatus::Running {
                    tree.running_children
                        .push((session_id.clone(), child_session_id.clone()));
                }
                if !tree.visited.insert(child_session_id.clone()) {
                    continue;
                }
                let closure = self.delivery_gates.close(&child_session_id).await?;
                tree.nodes.push(FrozenSessionNode {
                    session_id: child_session_id,
                    parent_session_id: Some(session_id.clone()),
                    exists: true,
                    closure,
                });
            }
            next_node += 1;
        }
        self.capture_external_parent_link(tree).await?;
        Ok(())
    }

    async fn capture_external_parent_link(
        &self,
        tree: &mut FrozenSessionTree,
    ) -> Result<(), TurnScheduleError> {
        let root_session_id = &tree.nodes[0].session_id;
        let Some(parent_session_id) = &tree.nodes[0].parent_session_id else {
            return Ok(());
        };
        if tree.visited.contains(parent_session_id) {
            return Ok(());
        }
        let parent_model = match self.session_manager.read_model(parent_session_id).await {
            Ok(model) => model,
            Err(SessionManagerError::Storage(StorageError::NotFound(_))) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if parent_model.agent_sessions.iter().any(|link| {
            link.child_session_id == *root_session_id && link.status == AgentSessionStatus::Running
        }) {
            tree.running_children
                .push((parent_session_id.clone(), root_session_id.clone()));
        }
        Ok(())
    }

    async fn finish_session_tree_close(
        &self,
        tree: &FrozenSessionTree,
        action: SessionTreeCloseAction,
    ) -> Result<(), TurnScheduleError> {
        let session_ids = tree.session_ids();
        let child_shutdown = self.child_sessions.begin_tree_shutdown(&session_ids);

        let mut cleanup_error = None;
        let mut settled_sessions = HashSet::new();
        for node in tree.nodes.iter().rev().filter(|node| node.exists) {
            let session_id = &node.session_id;
            match self.cleanup_execution_locked(session_id).await {
                Ok(true) => {
                    settled_sessions.insert(session_id.clone());
                },
                Ok(false) => {
                    if let Err(error) = self.repair_stale_locked(session_id).await {
                        if cleanup_error.is_none() {
                            cleanup_error = Some(error);
                        } else {
                            tracing::warn!(
                                %session_id,
                                %error,
                                "additional stale session repair failure during tree close"
                            );
                        }
                    }
                },
                Err(error) => {
                    if cleanup_error.is_none() {
                        cleanup_error = Some(error);
                    } else {
                        tracing::warn!(
                            %session_id,
                            %error,
                            "additional session tree execution cleanup failure"
                        );
                    }
                },
            }
            self.queue_drains.clear_session(session_id);
        }
        let guard_finish = self
            .child_sessions
            .finish_tree_shutdown(child_shutdown, &settled_sessions)
            .await;
        if let Some(error) = cleanup_error {
            if let Err(terminal_error) = guard_finish {
                tracing::warn!(
                    error = %terminal_error,
                    "additional session tree terminal persistence failure"
                );
            }
            return Err(error);
        }
        let guarded_children = guard_finish?;

        match action {
            SessionTreeCloseAction::Delete => {
                self.delete_frozen_session_tree(tree, &guarded_children)
                    .await
            },
            SessionTreeCloseAction::Recycle => self.recycle_frozen_session_tree(tree).await,
        }
    }

    async fn delete_frozen_session_tree(
        &self,
        tree: &FrozenSessionTree,
        guarded_children: &HashSet<SessionId>,
    ) -> Result<(), TurnScheduleError> {
        for node in tree.nodes.iter().rev() {
            if !node.exists {
                continue;
            }
            self.session_manager.delete(&node.session_id).await?;
            if !guarded_children.contains(&node.session_id) {
                for (parent_session_id, child_session_id) in &tree.running_children {
                    if child_session_id == &node.session_id
                        && !tree.visited.contains(parent_session_id)
                        && let Err(relation_error) = self
                            .child_sessions
                            .record_child_deleted(parent_session_id, child_session_id)
                            .await
                    {
                        return Err(TurnScheduleError::DeleteRelationUpdateFailed {
                            session_id: child_session_id.clone(),
                            parent_session_id: parent_session_id.clone(),
                            relation_error: relation_error.to_string(),
                        });
                    }
                }
            }
        }
        Ok(())
    }

    async fn recycle_frozen_session_tree(
        &self,
        tree: &FrozenSessionTree,
    ) -> Result<(), TurnScheduleError> {
        for node in tree.nodes.iter().rev().filter(|node| node.exists) {
            self.session_manager
                .recycle_session(&node.session_id)
                .await?;
            let Some(parent_session_id) = &node.parent_session_id else {
                continue;
            };
            if let Err(relation_error) = self
                .child_sessions
                .record_child_recycled(parent_session_id, &node.session_id)
                .await
            {
                match self.session_manager.restore_session(&node.session_id).await {
                    Ok(()) => {
                        tracing::warn!(
                            session_id = %node.session_id,
                            parent_session_id = %parent_session_id,
                            error = %relation_error,
                            "restored recycled session after parent relation update failed"
                        );
                        return Err(relation_error);
                    },
                    Err(restore_error) => {
                        return Err(TurnScheduleError::RecycleRelationRollbackFailed {
                            session_id: node.session_id.clone(),
                            relation_error: relation_error.to_string(),
                            restore_error: restore_error.to_string(),
                        });
                    },
                }
            }
        }
        Ok(())
    }

    async fn emit_turn_aborted(
        &self,
        turn_id: &TurnId,
        session: &Session,
        session_id: &SessionId,
    ) -> Result<(), TurnScheduleError> {
        finalize_aborted_turn(session, turn_id)
            .await
            .map_err(TurnScheduleError::EventEmit)?;
        self.session_manager
            .sync_durable_events_required(session_id)
            .await?;
        self.registry.remove_if_matches(session_id, turn_id);
        Ok(())
    }

    pub(super) async fn inject_internal(
        &self,
        turn_id: &TurnId,
        session: &Session,
        input: UserInput,
    ) -> Result<(), TurnScheduleError> {
        session
            .emit_durable(
                Some(turn_id),
                DurableEventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: input.text,
                    attachments: input.attachments,
                    accepted_seq: None,
                },
            )
            .await
            .map_err(TurnScheduleError::EventEmit)?;
        Ok(())
    }

    pub async fn repair_stale(&self, session_id: &SessionId) -> Result<(), TurnScheduleError> {
        let admission = self.admit_owned()?;
        let operation = self.begin_session_operation(session_id).await?;
        if let Some(started) = self.resume_stale_execution(&operation).await? {
            let turn_id = started.turn_id.clone();
            drop(operation);
            self.watch_owned_turn(
                admission,
                session_id.clone(),
                turn_id,
                started.handle,
                super::CompletionWatch {
                    source: "stale-resume",
                    completion_tx: None,
                    output_tx: None,
                },
            );
            return Ok(());
        }
        self.repair_stale_locked(session_id).await?;
        let next = self.reserve_next_pending(&operation).await?;
        drop(operation);
        if let Some((reserved, entry)) = next {
            self.start_reserved_pending_detached(reserved, entry, "stale-repair")
                .await?;
        }
        Ok(())
    }

    async fn resume_stale_execution(
        &self,
        operation: &SessionOperationGuard,
    ) -> Result<Option<super::StartedExecution>, TurnScheduleError> {
        let session_id = operation.session_id();
        if self.registry.has_active(session_id) {
            return Ok(None);
        }
        let session = self.session_manager.open(session_id.clone()).await?;
        let state = session
            .read_model()
            .await
            .map_err(TurnScheduleError::Session)?;
        let (Some(turn_id), Some(step)) = (
            state.execution.unsettled_turn_id.clone(),
            state.execution.active_step.as_ref(),
        ) else {
            return Ok(None);
        };

        if let Some(finish_reason) = &step.finish_reason {
            finalize_turn(
                &session,
                &turn_id,
                &TurnFinalization {
                    finish_reason: finish_reason.clone(),
                    pending_error: None,
                    aborted: false,
                    terminal_persisted: false,
                },
            )
            .await
            .map_err(TurnScheduleError::EventEmit)?;
            self.session_manager
                .sync_durable_events_required(session_id)
                .await?;
            return Ok(None);
        }

        emit_interrupted_tool_results(
            &session,
            &state,
            Some(&turn_id),
            InterruptedToolOutcome::Failed,
        )
        .await
        .map_err(TurnScheduleError::EventEmit)?;
        self.session_manager
            .sync_durable_events_required(session_id)
            .await?;

        let reservation = self
            .registry
            .reserve(session_id.clone(), turn_id.clone())
            .ok_or(TurnScheduleError::TurnAlreadyRunning)?;
        let _start_lease = operation.start_lease();
        tracing::info!(
            %session_id,
            %turn_id,
            step_index = if step.completed { step.step_index.saturating_add(1) } else { step.step_index },
            next_attempt = if step.completed { 1 } else { step.attempt.saturating_add(1) },
            "resuming stale turn from durable step"
        );
        let handle = session.resume(turn_id.clone()).await?;
        if !reservation.activate(handle.shutdown_handle(), Arc::new(session)) {
            handle.force_kill();
            return Err(TurnScheduleError::TurnAlreadyRunning);
        }
        Ok(Some(super::StartedExecution { turn_id, handle }))
    }

    async fn repair_stale_locked(&self, session_id: &SessionId) -> Result<(), TurnScheduleError> {
        if self.registry.has_active(session_id) {
            return Ok(());
        }

        let (session, state) = self.repair_turn_protocol_state(session_id).await?;
        repair_stale_runs_for_state(self, &session, &state).await?;
        self.session_manager
            .sync_durable_events_required(session_id)
            .await?;
        Ok(())
    }

    async fn repair_finished_turn_state(
        &self,
        session_id: &SessionId,
    ) -> Result<(), TurnScheduleError> {
        self.repair_turn_protocol_state(session_id).await?;
        self.session_manager
            .sync_durable_events_required(session_id)
            .await?;
        Ok(())
    }

    async fn repair_turn_protocol_state(
        &self,
        session_id: &SessionId,
    ) -> Result<(Session, Arc<SessionReadModel>), TurnScheduleError> {
        let session = self
            .session_manager
            .open(session_id.clone())
            .await
            .map_err(|error| {
                TurnScheduleError::SessionNotFound(format!("{session_id}: {error}"))
            })?;
        let state = session
            .read_model()
            .await
            .map_err(TurnScheduleError::Session)?;

        if matches!(state.execution.phase, Phase::Idle | Phase::Error) {
            repair_incomplete_tool_protocol_for_state(&session, &state).await?;
        } else {
            repair_stale_phase_for_state(session_id, &session, &state).await?;
        }
        Ok((session, state))
    }

    pub(crate) fn needs_stale_repair(state: &SessionReadModel) -> bool {
        !matches!(state.execution.phase, Phase::Idle | Phase::Error)
            || state.execution.active_step.is_some()
            || !state.tool_calls_needing_interruption().is_empty()
            || state
                .agent_sessions
                .iter()
                .any(|link| link.status == AgentSessionStatus::Running)
            || !state.execution.pending_inputs.is_empty()
    }
}

async fn wait_for_close_owner(
    result_rx: tokio::sync::oneshot::Receiver<Result<(), TurnScheduleError>>,
) -> Result<(), TurnScheduleError> {
    result_rx.await.map_err(|error| {
        TurnScheduleError::SessionManager(SessionManagerError::CloseTask(error.to_string()))
    })?
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
        let child_session_id = &link.child_session_id;
        if scheduler.registry().has_active(child_session_id) {
            scheduler.cleanup_execution(child_session_id).await;
            continue;
        }
        session
            .emit_durable(
                None,
                astrcode_session::payload::agent_session_failed_payload(
                    child_session_id.clone(),
                    "interrupted".into(),
                ),
            )
            .await
            .map_err(TurnScheduleError::EventEmit)?;
    }
    Ok(())
}

fn aborted_finalization() -> TurnFinalization {
    TurnFinalization {
        finish_reason: astrcode_session::payload::TURN_FINISH_ABORTED.into(),
        pending_error: None,
        aborted: true,
        terminal_persisted: false,
    }
}
