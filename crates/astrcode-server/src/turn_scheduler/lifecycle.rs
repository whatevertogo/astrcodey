use std::time::Duration;

use astrcode_core::{
    event::{DurableEventPayload, Phase},
    types::{SessionId, TurnId, new_message_id},
    user_input::UserInput,
};
use astrcode_session::{
    InterruptedToolOutcome, Session, emit_interrupted_tool_results, emit_turn_aborted_context,
    payload::{
        TURN_FINISH_ABORTED, TURN_FINISH_INTERRUPTED, agent_run_completed_payload,
        turn_completed_payload,
    },
};
use astrcode_session_projection::{AgentSessionStatus, SessionReadModel};

use super::{
    ABORT_WAIT_EXTRA_MS, ABORT_WAIT_POLL_MS, CompletedRecycleOutcome, FORCE_KILL_GRACE_MS,
    TurnScheduleError, TurnScheduler,
};

impl TurnScheduler {
    /// 中止活跃 turn（含级联子 session）。
    pub async fn abort(&self, session_id: &SessionId) -> Result<(), TurnScheduleError> {
        let _operation = self.begin_session_operation(session_id).await?;
        self.abort_in_operation(session_id, false).await
    }

    pub(super) async fn abort_in_operation(
        &self,
        session_id: &SessionId,
        release_finished: bool,
    ) -> Result<(), TurnScheduleError> {
        self.child_sessions
            .cascade_abort_children(self, session_id)
            .await;

        if self.registry.has_active(session_id) {
            let deadline = tokio::time::Instant::now()
                + Duration::from_millis(FORCE_KILL_GRACE_MS + ABORT_WAIT_EXTRA_MS);
            let mut shutdown_turn = None;
            while self.registry.has_active(session_id) && tokio::time::Instant::now() < deadline {
                if release_finished && self.registry.remove_if_finished(session_id).is_some() {
                    break;
                }
                let active_turn = self.registry.active_turn_id(session_id);
                if active_turn != shutdown_turn {
                    if let Some(turn_id) = self.registry.request_shutdown(session_id) {
                        self.schedule_force_kill(session_id.clone(), turn_id.clone());
                        shutdown_turn = Some(turn_id);
                    }
                }
                tokio::time::sleep(Duration::from_millis(ABORT_WAIT_POLL_MS)).await;
            }
            if self.registry.has_active(session_id) {
                return Ok(());
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

    /// 仅对本 session 发起协作式 shutdown（不级联子 session、不跑 stale repair）。
    pub(super) fn request_turn_shutdown(&self, session_id: &SessionId) {
        if let Some(turn_id) = self.registry.request_shutdown(session_id) {
            self.schedule_force_kill(session_id.clone(), turn_id);
        }
    }

    /// completion 已产出但 task 可能尚未退出；只按 turn identity 非破坏性移除。
    pub async fn release_completed_execution(&self, session_id: &SessionId, turn_id: &TurnId) {
        let _operation = self.delivery_gates.lock(session_id).await;
        self.release_completed_execution_locked(session_id, turn_id);
    }

    fn release_completed_execution_locked(&self, session_id: &SessionId, turn_id: &TurnId) -> bool {
        self.registry
            .remove_if_matches(session_id, turn_id)
            .is_some()
    }

    /// 清理 session 的 execution 与待处理输入；仍在运行的 turn 会被中止。
    pub async fn cleanup_execution(&self, session_id: &SessionId) {
        let _operation = self.delivery_gates.lock(session_id).await;
        self.cleanup_execution_locked(session_id).await;
    }

    async fn cleanup_execution_locked(&self, session_id: &SessionId) {
        if self.registry.remove_if_finished(session_id).is_none() {
            if let Some((turn_id, session)) = self.registry.force_kill_current(session_id) {
                self.emit_turn_aborted(&turn_id, &session, session_id).await;
            }
        }
    }

    pub async fn delete_session(&self, session_id: &SessionId) -> Result<(), TurnScheduleError> {
        let gate = self.delivery_gates.close(session_id).await?;
        gate.wait_for_starts().await;
        self.cleanup_execution_locked(session_id).await;
        self.queue_drains.clear_session(session_id);
        let result = self.session_manager.delete(session_id).await;
        gate.finish();
        result.map_err(TurnScheduleError::from)
    }

    pub async fn recycle_session(&self, session_id: &SessionId) -> Result<(), TurnScheduleError> {
        let gate = self.delivery_gates.close(session_id).await?;
        gate.wait_for_starts().await;
        self.cleanup_execution_locked(session_id).await;
        self.queue_drains.clear_session(session_id);
        let result = self.session_manager.recycle_session(session_id).await;
        gate.finish();
        result.map_err(TurnScheduleError::from)
    }

    pub(crate) async fn recycle_completed_session(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<CompletedRecycleOutcome, TurnScheduleError> {
        let operation = self.begin_session_operation(session_id).await?;
        if !self.release_completed_execution_locked(session_id, turn_id) {
            return Ok(CompletedRecycleOutcome::StaleCompletion);
        }
        let gate = self.delivery_gates.close_operation(operation);
        gate.wait_for_starts().await;
        self.queue_drains.clear_session(session_id);
        let result = self.session_manager.recycle_session(session_id).await;
        gate.finish();
        result?;
        Ok(CompletedRecycleOutcome::Recycled)
    }

    fn schedule_force_kill(&self, session_id: SessionId, turn_id: TurnId) {
        let scheduler = self.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(FORCE_KILL_GRACE_MS)).await;
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
                    Err(error) => {
                        tracing::warn!(
                            session_id = %session_id,
                            turn_id = %turn_id,
                            error = %error,
                            "failed to settle pending tool calls during abort"
                        );
                        false
                    },
                }
            },
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    turn_id = %turn_id,
                    error = %error,
                    "failed to read session state during abort"
                );
                false
            },
        };

        if tool_protocol_settled {
            if let Err(error) = emit_turn_aborted_context(session, Some(turn_id)).await {
                tracing::warn!(
                    session_id = %session_id,
                    turn_id = %turn_id,
                    error = %error,
                    "failed to write turn-aborted provider context"
                );
            }
        }

        if let Err(error) = session
            .emit_durable(Some(turn_id), turn_completed_payload(TURN_FINISH_ABORTED))
            .await
        {
            tracing::error!(
                session_id = %session_id,
                turn_id = %turn_id,
                error = %error,
                "failed to write TurnCompleted during abort"
            );
        }
        session.emit_live(
            Some(turn_id),
            agent_run_completed_payload(TURN_FINISH_ABORTED),
        );
        self.session_manager.sync_durable_events(session_id).await;
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
        let operation = self.begin_session_operation(session_id).await?;
        self.repair_stale_locked(session_id).await?;
        let next = self.reserve_next_pending(&operation).await?;
        drop(operation);
        if let Some((reserved, entry)) = next {
            self.start_reserved_pending_detached(reserved, entry, "stale-repair")
                .await?;
        }
        Ok(())
    }

    async fn repair_stale_locked(&self, session_id: &SessionId) -> Result<(), TurnScheduleError> {
        if self.registry.has_active(session_id) {
            return Ok(());
        }

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
            || !state.execution.pending_inputs.is_empty()
    }
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
