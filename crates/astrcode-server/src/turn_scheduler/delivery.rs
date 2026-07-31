use astrcode_core::{types::SessionId, user_input::UserInput};

use super::{
    CompletionWatch, DeliveryOutcome, InputDelivery, ReservedExecution, StartedExecution,
    TurnScheduleError, TurnScheduler, validate_user_input,
};
use crate::delivery_gates::SessionOperationGuard;

impl TurnScheduler {
    /// 输入投递的唯一 public gateway。
    pub async fn deliver_input(
        &self,
        session_id: SessionId,
        input: UserInput,
        delivery: InputDelivery,
    ) -> Result<DeliveryOutcome, TurnScheduleError> {
        let _admission = self.admit_owned()?;
        let operation = self.begin_session_operation(&session_id).await?;
        self.deliver_input_in_operation(operation, input, delivery)
            .await
    }

    pub(crate) async fn begin_session_operation(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionOperationGuard, TurnScheduleError> {
        self.delivery_gates.begin(session_id).await
    }

    pub(crate) async fn deliver_input_in_operation(
        &self,
        operation: SessionOperationGuard,
        input: UserInput,
        delivery: InputDelivery,
    ) -> Result<DeliveryOutcome, TurnScheduleError> {
        validate_user_input(&input)?;
        let session_id = operation.session_id().clone();
        match delivery {
            InputDelivery::StartNew => {
                self.ensure_no_pending_inputs(&session_id).await?;
                self.start_detached_in_operation(operation, input, "deliver_input:start")
                    .await
            },
            InputDelivery::InjectOnly => {
                if self.registry.active_is_finished(&session_id) {
                    tracing::debug!(
                        session_id = %session_id,
                        "finished active turn was still registered; rejecting stale injection"
                    );
                    return Err(TurnScheduleError::NoActiveTurn);
                }
                let Some((turn_id, session)) = self.registry.active_execution(&session_id) else {
                    return Err(TurnScheduleError::NoActiveTurn);
                };
                self.inject_internal(&turn_id, &session, input).await?;
                Ok(DeliveryOutcome::Injected { turn_id })
            },
            InputDelivery::InjectIfRunningElseStart => {
                self.inject_if_running_else_start(operation, input).await
            },
            InputDelivery::QueueIfRunningElseStart => {
                self.queue_if_running_else_start(operation, input).await
            },
            InputDelivery::InterruptAndStart => {
                operation.wait_for_starts().await;
                self.abort_in_operation(&session_id).await?;
                self.start_detached_in_operation(operation, input, "deliver_input:interrupt")
                    .await
            },
        }
    }

    async fn inject_if_running_else_start(
        &self,
        operation: SessionOperationGuard,
        input: UserInput,
    ) -> Result<DeliveryOutcome, TurnScheduleError> {
        let session_id = operation.session_id().clone();
        if self.registry.active_is_finished(&session_id) {
            let queue_len = self.accept_pending_input(&session_id, input).await?;
            return Ok(DeliveryOutcome::Queued { queue_len });
        }
        if let Some((turn_id, session)) = self.registry.active_execution(&session_id) {
            self.inject_internal(&turn_id, &session, input).await?;
            return Ok(DeliveryOutcome::Injected { turn_id });
        }
        if self.registry.has_active(&session_id) {
            let queue_len = self.accept_pending_input(&session_id, input).await?;
            return Ok(DeliveryOutcome::Queued { queue_len });
        }
        if self.pending_input_count(&session_id).await? > 0 {
            let queue_len = self
                .enqueue_behind_pending_and_start_head(
                    operation,
                    input,
                    "deliver_input:inject-recovery",
                )
                .await?;
            return Ok(DeliveryOutcome::Queued { queue_len });
        }
        self.start_detached_in_operation(operation, input, "deliver_input:inject")
            .await
    }

    async fn queue_if_running_else_start(
        &self,
        operation: SessionOperationGuard,
        input: UserInput,
    ) -> Result<DeliveryOutcome, TurnScheduleError> {
        let session_id = operation.session_id().clone();
        if self.registry.has_active(&session_id) {
            // turn 已 durable 完成、registry 尚未 settle 的窗口：完成 watcher 可能仍在
            // 收尾（sync/child-drain）或重试中，此时若仅入队则完全依赖 watcher 的 drain；
            // watcher 一旦卡住，输入会永久滞留且永远不进入下一次 turn。由投递路径先收尾
            // finished turn 再入队并立即启动队首，消除该窗口（与 watcher 的 settle 幂等）。
            if self.registry.active_is_finished(&session_id) {
                let turn_id = self
                    .registry
                    .active_turn_id(&session_id)
                    .ok_or(TurnScheduleError::NoActiveTurn)?;
                let finalization = self.registry.active_finalization(&session_id);
                match self
                    .settle_finished_execution_locked(&session_id, &turn_id, finalization.as_ref())
                    .await
                {
                    Ok(true) => {
                        let queue_len = self
                            .enqueue_behind_pending_and_start_head(
                                operation,
                                input,
                                "deliver_input:finished-drain",
                            )
                            .await?;
                        return Ok(DeliveryOutcome::Queued { queue_len });
                    },
                    Ok(false) | Err(_) => {
                        tracing::debug!(
                            session_id = %session_id,
                            %turn_id,
                            "finished turn settle deferred to completion watcher; queueing input"
                        );
                    },
                }
            }
            let queue_len = self.accept_pending_input(&session_id, input).await?;
            tracing::info!(
                session_id = %session_id,
                queue_len,
                "message queued for next turn"
            );
            return Ok(DeliveryOutcome::Queued { queue_len });
        }
        if self.pending_input_count(&session_id).await? > 0 {
            let queue_len = self
                .enqueue_behind_pending_and_start_head(
                    operation,
                    input,
                    "deliver_input:queue-recovery",
                )
                .await?;
            return Ok(DeliveryOutcome::Queued { queue_len });
        }
        self.start_detached_in_operation(operation, input, "deliver_input:queue")
            .await
    }

    async fn start_detached_in_operation(
        &self,
        operation: SessionOperationGuard,
        input: UserInput,
        source: &'static str,
    ) -> Result<DeliveryOutcome, TurnScheduleError> {
        let reserved = self.reserve_execution(&operation, input, None)?;
        drop(operation);
        self.start_reserved_detached(reserved, source).await
    }

    async fn start_reserved_detached(
        &self,
        reserved: ReservedExecution,
        source: &'static str,
    ) -> Result<DeliveryOutcome, TurnScheduleError> {
        let admission = self.admit_owned()?;
        let session_id = reserved.session_id.clone();
        let StartedExecution { turn_id, handle } = self
            .start_reserved_execution(reserved)
            .await
            .map_err(|failure| failure.error)?;
        self.watch_owned_turn(
            admission,
            session_id,
            turn_id.clone(),
            handle,
            CompletionWatch {
                source,
                completion_tx: None,
                output_tx: None,
            },
        );
        Ok(DeliveryOutcome::Started { turn_id })
    }
}
