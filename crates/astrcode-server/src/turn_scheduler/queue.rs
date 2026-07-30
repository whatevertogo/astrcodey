use astrcode_core::{event::DurableEventPayload, types::SessionId, user_input::UserInput};
use astrcode_session_projection::PendingInput;

use super::{
    MAX_PENDING_INPUTS_PER_SESSION, ReservedExecution, StartedExecution, TurnScheduleError,
    TurnScheduler, validate_user_input,
};
use crate::{delivery_gates::SessionOperationGuard, queue_drains::QueueDrainRetry};

impl TurnScheduler {
    async fn pending_inputs(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<PendingInput>, TurnScheduleError> {
        let state = self.session_manager.read_model(session_id).await?;
        Ok(state.execution.pending_inputs.clone())
    }

    pub(super) async fn pending_input_count(
        &self,
        session_id: &SessionId,
    ) -> Result<usize, TurnScheduleError> {
        Ok(self.pending_inputs(session_id).await?.len())
    }

    pub(super) async fn ensure_no_pending_inputs(
        &self,
        session_id: &SessionId,
    ) -> Result<(), TurnScheduleError> {
        if self.pending_input_count(session_id).await? > 0 {
            return Err(TurnScheduleError::TurnAlreadyRunning);
        }
        Ok(())
    }

    pub(super) async fn accept_pending_input(
        &self,
        session_id: &SessionId,
        input: UserInput,
    ) -> Result<usize, TurnScheduleError> {
        let pending_count = self.pending_input_count(session_id).await?;
        if pending_count >= MAX_PENDING_INPUTS_PER_SESSION {
            return Err(TurnScheduleError::QueueFull {
                max: MAX_PENDING_INPUTS_PER_SESSION,
            });
        }
        let session = self.session_manager.open(session_id.clone()).await?;
        session
            .emit_durable(None, DurableEventPayload::UserInputAccepted { input })
            .await
            .map_err(TurnScheduleError::EventEmit)?;
        Ok(pending_count + 1)
    }

    pub(super) async fn enqueue_behind_pending_and_start_head(
        &self,
        operation: SessionOperationGuard,
        input: UserInput,
        source: &'static str,
    ) -> Result<usize, TurnScheduleError> {
        let session_id = operation.session_id().clone();
        let queue_len = self.accept_pending_input(&session_id, input).await?;
        let next = self.reserve_next_pending(&operation).await?;
        drop(operation);
        let queue_len = if let Some((reserved, entry)) = next {
            match self
                .start_reserved_pending_detached(reserved, entry, source)
                .await
            {
                Ok(()) => queue_len.saturating_sub(1),
                Err(_) => queue_len,
            }
        } else {
            queue_len
        };
        Ok(queue_len)
    }

    pub(super) async fn reserve_next_pending(
        &self,
        operation: &SessionOperationGuard,
    ) -> Result<Option<(ReservedExecution, PendingInput)>, TurnScheduleError> {
        let session_id = operation.session_id();
        let Some(entry) = self.pending_inputs(session_id).await?.into_iter().next() else {
            return Ok(None);
        };
        validate_user_input(&entry.input)?;
        let reserved =
            self.reserve_execution(operation, entry.input.clone(), Some(entry.accepted_seq))?;
        tracing::info!(
            session_id = %session_id,
            accepted_seq = entry.accepted_seq,
            "auto-submitting next queued message for new turn"
        );
        Ok(Some((reserved, entry)))
    }

    pub(super) async fn start_reserved_pending(
        &self,
        reserved: ReservedExecution,
        entry: PendingInput,
    ) -> Result<StartedExecution, TurnScheduleError> {
        let session_id = reserved.session_id.clone();
        match self.start_reserved_execution(reserved).await {
            Ok(started) => {
                self.queue_drains.clear_session(&session_id);
                Ok(started)
            },
            Err(failure) => {
                tracing::warn!(
                    session_id = %session_id,
                    accepted_seq = entry.accepted_seq,
                    error = %failure.error,
                    "failed to auto-submit durable queued message"
                );
                let error = failure.error;
                self.schedule_queue_retry(session_id, &error);
                Err(error)
            },
        }
    }

    pub(super) async fn start_reserved_pending_detached(
        &self,
        reserved: ReservedExecution,
        entry: PendingInput,
        source: &'static str,
    ) -> Result<(), TurnScheduleError> {
        let admission = self.admit_owned()?;
        let session_id = reserved.session_id.clone();
        let StartedExecution { turn_id, handle } =
            self.start_reserved_pending(reserved, entry).await?;
        self.watch_owned_turn(admission, session_id, turn_id, handle, source, None, None);
        Ok(())
    }

    fn schedule_queue_retry(&self, session_id: SessionId, error: &TurnScheduleError) {
        tracing::warn!(
            session_id = %session_id,
            %error,
            "failed to start queued input; scheduling retry"
        );
        let Some(retry) = self.queue_drains.record_start_failure(&session_id) else {
            return;
        };
        let scheduler = self.clone();
        if let Err(spawn_error) = self.spawn_owned_named("queue_retry", async move {
            scheduler.run_queue_retry(session_id, retry).await;
        }) {
            tracing::debug!(%spawn_error, "queue retry rejected during shutdown");
        }
    }

    async fn run_queue_retry(&self, session_id: SessionId, retry: QueueDrainRetry) {
        let elapsed = tokio::select! {
            biased;
            () = self.background_shutdown.cancelled() => false,
            () = retry.cancel.cancelled() => false,
            () = tokio::time::sleep(retry.delay) => true,
        };
        if !self.queue_drains.finish_retry_wait(&session_id, &retry)
            || !elapsed
            || self.background_shutdown.is_cancelled()
        {
            return;
        }

        let operation = match self.begin_session_operation(&session_id).await {
            Ok(operation) => operation,
            Err(_) => return,
        };
        if self.registry.has_active(&session_id) {
            return;
        }
        let next = match self.reserve_next_pending(&operation).await {
            Ok(next) => next,
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    %error,
                    "failed to reserve queued input during retry"
                );
                if matches!(&error, TurnScheduleError::SessionManager(_)) {
                    drop(operation);
                    self.schedule_queue_retry(session_id, &error);
                }
                return;
            },
        };
        drop(operation);
        let Some((reserved, entry)) = next else {
            self.queue_drains.clear_session(&session_id);
            return;
        };
        let _ = self
            .start_reserved_pending_detached(reserved, entry, "queue-retry")
            .await;
    }
}
