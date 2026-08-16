//! Extension-facing [`SessionOperations`] 适配层：参数转换与错误映射。

use std::sync::Arc;

use astrcode_core::{
    tool::{
        CreateRootSessionRequest, CreateSessionRequest, SessionAccess, SessionApiError,
        SessionDeliveryOutcome, SessionExecutionView, SessionHandle, SessionLifecycleState,
        SessionOperations, SessionReactivation, SessionState, SessionStatus, SessionToolSelection,
        SubmitTurnRequest, SubmitTurnResult,
    },
    types::{SessionId, TurnId},
};

use crate::{
    child_session::{ChildCleanup, ChildSessionCoordinator},
    session_manager::SessionManager,
    turn_scheduler::{InputDelivery, TurnScheduleError, TurnScheduler},
};

pub struct ServerSessionOperations {
    pub session_manager: Arc<SessionManager>,
    pub scheduler: Arc<TurnScheduler>,
    pub child_sessions: Arc<ChildSessionCoordinator>,
}

impl ServerSessionOperations {
    async fn verified_session_ids(
        &self,
        access: SessionAccess<'_>,
    ) -> Result<(SessionId, SessionId), SessionApiError> {
        let caller_sid = SessionId::from(access.caller_session_id);
        let target_sid = SessionId::from(access.target_session_id);
        self.child_sessions
            .verify_access(&caller_sid, &target_sid)
            .await?;
        Ok((caller_sid, target_sid))
    }

    async fn deliver_message(
        &self,
        access: SessionAccess<'_>,
        content: String,
        delivery: InputDelivery,
    ) -> Result<SessionDeliveryOutcome, SessionApiError> {
        let (_, target_sid) = self.verified_session_ids(access).await?;
        let outcome = self
            .scheduler
            .deliver_input(
                target_sid.clone(),
                astrcode_core::user_input::UserInput::text_only(content),
                delivery,
            )
            .await
            .map_err(SessionApiError::internal)?;

        self.session_manager.sync_durable_events(&target_sid).await;
        Ok(delivery_outcome(outcome))
    }

    async fn submit_same_session_turn(
        &self,
        session_id: SessionId,
        user_prompt: String,
        wait_for_result: bool,
    ) -> Result<SubmitTurnResult, SessionApiError> {
        let input = astrcode_core::user_input::UserInput::text_only(user_prompt);
        if !wait_for_result {
            let (turn_id, _completion) = self
                .scheduler
                .start_tracked_with_completion(session_id.clone(), input)
                .await
                .map_err(SessionApiError::internal)?;
            return Ok(SubmitTurnResult::Backgrounded {
                task_id: turn_id.into_string(),
                session_id: session_id.into_string(),
            });
        }

        let (_turn_id, output) = self
            .scheduler
            .start_tracked_with_output(session_id, input)
            .await
            .map_err(SessionApiError::internal)?;
        let content = output
            .await
            .map_err(SessionApiError::internal)?
            .map_err(SessionApiError::internal)?;
        Ok(SubmitTurnResult::Completed { content })
    }

    async fn rollback_reactivation(
        session_manager: &SessionManager,
        child_sessions: &ChildSessionCoordinator,
        parent_sid: &SessionId,
        target_sid: &SessionId,
        activation_error: SessionApiError,
    ) -> SessionApiError {
        let rollback_error = match session_manager.recycle_session(target_sid).await {
            Ok(()) => child_sessions
                .record_child_recycled(parent_sid, target_sid)
                .await
                .err()
                .map(|error| error.to_string()),
            Err(error) => Some(error.to_string()),
        };
        match rollback_error {
            None => activation_error,
            Some(rollback_error) => SessionApiError::internal(ReactivationRollbackError {
                session_id: target_sid.clone(),
                activation_error,
                rollback_error,
            }),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error(
    "session {session_id} reactivation failed ({activation_error}) and rollback also failed \
     ({rollback_error})"
)]
struct ReactivationRollbackError {
    session_id: SessionId,
    #[source]
    activation_error: SessionApiError,
    rollback_error: String,
}

#[async_trait::async_trait]
impl SessionOperations for ServerSessionOperations {
    async fn create_root_session(
        &self,
        request: CreateRootSessionRequest,
    ) -> Result<SessionHandle, SessionApiError> {
        let session = self
            .session_manager
            .create_for_extension(&request.working_dir, request.source_extension)
            .await
            .map_err(SessionApiError::internal)?;

        Ok(SessionHandle {
            session_id: session.id().clone().into_string(),
        })
    }

    async fn create_session(
        &self,
        parent_session_id: &str,
        request: CreateSessionRequest,
    ) -> Result<SessionHandle, SessionApiError> {
        let parent_sid = SessionId::from(parent_session_id);
        let operation = self
            .scheduler
            .begin_session_operation(&parent_sid)
            .await
            .map_err(SessionApiError::internal)?;
        let child = self.child_sessions.spawn_child(operation, request).await?;

        Ok(SessionHandle {
            session_id: child.id().clone().into_string(),
        })
    }

    async fn inject_message(
        &self,
        access: SessionAccess<'_>,
        content: String,
    ) -> Result<SessionDeliveryOutcome, SessionApiError> {
        self.deliver_message(access, content, InputDelivery::InjectIfRunningElseStart)
            .await
    }

    async fn queue_or_start(
        &self,
        access: SessionAccess<'_>,
        content: String,
    ) -> Result<SessionDeliveryOutcome, SessionApiError> {
        self.deliver_message(access, content, InputDelivery::QueueIfRunningElseStart)
            .await
    }

    async fn defer_context(
        &self,
        access: SessionAccess<'_>,
        content: String,
    ) -> Result<SessionDeliveryOutcome, SessionApiError> {
        let (_, target_sid) = self.verified_session_ids(access).await?;
        let outcome = self
            .scheduler
            .deliver_input(
                target_sid.clone(),
                astrcode_core::user_input::UserInput::text_only(content),
                InputDelivery::InjectOnly,
            )
            .await
            .map_err(|error| match error {
                TurnScheduleError::NoActiveTurn => {
                    SessionApiError::NoActiveTurn(target_sid.to_string())
                },
                error => SessionApiError::internal(error),
            })?;

        self.session_manager.sync_durable_events(&target_sid).await;
        Ok(delivery_outcome(outcome))
    }

    async fn interrupt_and_submit(
        &self,
        access: SessionAccess<'_>,
        content: String,
    ) -> Result<SessionDeliveryOutcome, SessionApiError> {
        self.deliver_message(access, content, InputDelivery::InterruptAndStart)
            .await
    }

    async fn cancel_turn(&self, access: SessionAccess<'_>) -> Result<bool, SessionApiError> {
        let (_, target_sid) = self.verified_session_ids(access).await?;
        self.scheduler
            .abort(&target_sid)
            .await
            .map_err(SessionApiError::internal)
    }

    async fn execution_view(
        &self,
        access: SessionAccess<'_>,
    ) -> Result<SessionExecutionView, SessionApiError> {
        let (_, target_sid) = self.verified_session_ids(access).await?;
        let view = self
            .scheduler
            .execution_view(&target_sid)
            .await
            .map_err(SessionApiError::internal)?;
        Ok(SessionExecutionView {
            phase: view.phase,
            active_turn_id: view.active_turn_id.map(TurnId::into_string),
            queued_inputs: view.queued_inputs,
        })
    }

    async fn configure_tools(
        &self,
        access: SessionAccess<'_>,
        selection: SessionToolSelection,
    ) -> Result<SessionToolSelection, SessionApiError> {
        let (_, target_sid) = self.verified_session_ids(access).await?;
        let session = self
            .session_manager
            .open(target_sid.clone())
            .await
            .map_err(|error| SessionApiError::NotFound(error.to_string()))?;
        let effective = self
            .session_manager
            .configure_session_tools(&session, selection)
            .await
            .map_err(SessionApiError::internal)?;
        Ok(effective)
    }

    async fn submit_turn(
        &self,
        request: SubmitTurnRequest,
    ) -> Result<SubmitTurnResult, SessionApiError> {
        let (caller_sid, target_sid) = self
            .verified_session_ids(request.access.as_access())
            .await?;

        if caller_sid == target_sid {
            if request.notify_parent_on_complete.is_some() || request.recycle_on_complete {
                return Err(SessionApiError::Unsupported(
                    "same-session turns cannot notify a parent or recycle themselves".into(),
                ));
            }
            return self
                .submit_same_session_turn(target_sid, request.user_prompt, request.wait_for_result)
                .await;
        }

        if request.wait_for_result {
            let content = self
                .child_sessions
                .submit_turn_sync(
                    Arc::clone(&self.scheduler),
                    &caller_sid,
                    &target_sid,
                    request.user_prompt,
                )
                .await?;
            Ok(SubmitTurnResult::Completed { content })
        } else {
            let cleanup = if request.recycle_on_complete {
                ChildCleanup::Recycle
            } else {
                ChildCleanup::Keep
            };
            let (turn_id, session_id) = self
                .child_sessions
                .submit_turn_background(
                    self.scheduler.as_ref(),
                    &caller_sid,
                    &target_sid,
                    request.user_prompt,
                    cleanup,
                    request.notify_parent_on_complete,
                    request.tool_call_id.clone(),
                )
                .await?;
            Ok(SubmitTurnResult::Backgrounded {
                task_id: turn_id.into_string(),
                session_id: session_id.into_string(),
            })
        }
    }

    async fn query_session(
        &self,
        access: SessionAccess<'_>,
    ) -> Result<SessionStatus, SessionApiError> {
        let (_, target_sid) = self.verified_session_ids(access).await?;

        let view = self
            .scheduler
            .execution_view(&target_sid)
            .await
            .map_err(|e| SessionApiError::NotFound(e.to_string()))?;

        Ok(SessionStatus {
            alive: true,
            has_active_turn: view.active_turn_id.is_some(),
            last_finish_reason: None,
            message_count: view.message_count,
        })
    }

    async fn session_state(
        &self,
        access: SessionAccess<'_>,
    ) -> Result<SessionState, SessionApiError> {
        let caller_sid = SessionId::from(access.caller_session_id);
        let target_sid = SessionId::from(access.target_session_id);
        match self.session_manager.read_model(&target_sid).await {
            Ok(_) => {
                self.child_sessions
                    .verify_access(&caller_sid, &target_sid)
                    .await?;
                let view = self
                    .scheduler
                    .execution_view(&target_sid)
                    .await
                    .map_err(SessionApiError::internal)?;
                Ok(SessionState {
                    lifecycle: SessionLifecycleState::Active,
                    phase: view.phase,
                    active_turn_id: view.active_turn_id.map(TurnId::into_string),
                    queued_inputs: view.queued_inputs,
                    message_count: view.message_count,
                })
            },
            Err(crate::session_manager::SessionManagerError::Storage(
                astrcode_storage::StorageError::NotFound(_),
            )) => {
                self.child_sessions
                    .verify_restore_access(&caller_sid, &target_sid)
                    .await?;
                let model = self
                    .session_manager
                    .read_recycled_model(&target_sid)
                    .await
                    .map_err(map_restore_error)?;
                Ok(SessionState {
                    lifecycle: SessionLifecycleState::Recycled,
                    phase: model.execution.phase,
                    active_turn_id: None,
                    queued_inputs: model.execution.pending_inputs.len(),
                    message_count: model.model_context.messages.len(),
                })
            },
            Err(error) => Err(SessionApiError::internal(error)),
        }
    }

    async fn recycle_session(&self, access: SessionAccess<'_>) -> Result<(), SessionApiError> {
        let (_, target_sid) = self.verified_session_ids(access).await?;

        self.scheduler
            .recycle_session(&target_sid)
            .await
            .map_err(SessionApiError::internal)
    }

    async fn delete_session(&self, access: SessionAccess<'_>) -> Result<(), SessionApiError> {
        let (_, target_sid) = self.verified_session_ids(access).await?;

        self.scheduler
            .delete_session(&target_sid)
            .await
            .map_err(SessionApiError::internal)?;

        Ok(())
    }

    async fn restore_session(&self, access: SessionAccess<'_>) -> Result<(), SessionApiError> {
        let caller_sid = SessionId::from(access.caller_session_id);
        let target_sid = SessionId::from(access.target_session_id);
        let admission = self
            .scheduler
            .admit_owned()
            .map_err(SessionApiError::internal)?;
        let scheduler = Arc::clone(&self.scheduler);
        let session_manager = Arc::clone(&self.session_manager);
        let child_sessions = Arc::clone(&self.child_sessions);
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();

        admission.spawn_named("session_restore_owner", async move {
            let result = async {
                let _operation = scheduler
                    .begin_session_operation(&target_sid)
                    .await
                    .map_err(SessionApiError::internal)?;
                let transition = session_manager.begin_session_transition(&target_sid).await;

                child_sessions
                    .verify_restore_access(&caller_sid, &target_sid)
                    .await?;
                session_manager
                    .restore_session_in_transition(&transition)
                    .await
                    .map_err(map_restore_error)
            }
            .await;
            let _ = result_tx.send(result);
        });

        result_rx.await.map_err(SessionApiError::internal)?
    }

    async fn reactivate_session(
        &self,
        access: SessionAccess<'_>,
    ) -> Result<SessionReactivation, SessionApiError> {
        let caller_sid = SessionId::from(access.caller_session_id);
        let target_sid = SessionId::from(access.target_session_id);
        if caller_sid == target_sid {
            return Err(SessionApiError::PermissionDenied(
                "session reactivation requires an active direct parent".into(),
            ));
        }

        let admission = self
            .scheduler
            .admit_owned()
            .map_err(SessionApiError::internal)?;
        let scheduler = Arc::clone(&self.scheduler);
        let session_manager = Arc::clone(&self.session_manager);
        let child_sessions = Arc::clone(&self.child_sessions);
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();

        admission.spawn_named("session_reactivation_owner", async move {
            let result = async {
                let _parent_operation = scheduler
                    .begin_session_operation(&caller_sid)
                    .await
                    .map_err(SessionApiError::internal)?;
                session_manager
                    .read_model(&caller_sid)
                    .await
                    .map_err(|error| match error {
                        crate::session_manager::SessionManagerError::Storage(
                            astrcode_storage::StorageError::NotFound(_),
                        ) => SessionApiError::NotFound(format!("parent session {caller_sid}")),
                        error => SessionApiError::internal(error),
                    })?;
                let _target_operation = scheduler
                    .begin_session_operation(&target_sid)
                    .await
                    .map_err(SessionApiError::internal)?;

                let (target_model, was_recycled) =
                    match session_manager.read_model(&target_sid).await {
                        Ok(model) => {
                            child_sessions
                                .verify_access(&caller_sid, &target_sid)
                                .await?;
                            (model, false)
                        },
                        Err(crate::session_manager::SessionManagerError::Storage(
                            astrcode_storage::StorageError::NotFound(_),
                        )) => {
                            child_sessions
                                .verify_restore_access(&caller_sid, &target_sid)
                                .await?;
                            let model = session_manager
                                .read_recycled_model(&target_sid)
                                .await
                                .map_err(map_restore_error)?;
                            (model, true)
                        },
                        Err(error) => return Err(SessionApiError::internal(error)),
                    };
                let direct_parent = target_model
                    .identity
                    .parent
                    .as_ref()
                    .map(|parent| &parent.session_id);
                if direct_parent != Some(&caller_sid) {
                    return Err(SessionApiError::PermissionDenied(format!(
                        "session {target_sid} is not a direct child of {caller_sid}"
                    )));
                }

                if was_recycled {
                    let transition = session_manager.begin_session_transition(&target_sid).await;
                    session_manager
                        .restore_session_in_transition(&transition)
                        .await
                        .map_err(map_restore_error)?;
                    drop(transition);
                }

                if let Err(error) = session_manager.open(target_sid.clone()).await {
                    let error = SessionApiError::internal(error);
                    return Err(if was_recycled {
                        Self::rollback_reactivation(
                            &session_manager,
                            &child_sessions,
                            &caller_sid,
                            &target_sid,
                            error,
                        )
                        .await
                    } else {
                        error
                    });
                }
                if let Err(error) = child_sessions
                    .reattach_recycled_child(&caller_sid, &target_sid)
                    .await
                {
                    return Err(if was_recycled {
                        Self::rollback_reactivation(
                            &session_manager,
                            &child_sessions,
                            &caller_sid,
                            &target_sid,
                            error,
                        )
                        .await
                    } else {
                        error
                    });
                }

                Ok(SessionReactivation {
                    reactivated: was_recycled,
                })
            }
            .await;
            let _ = result_tx.send(result);
        });

        result_rx.await.map_err(SessionApiError::internal)?
    }

    async fn resolve_tool_approval(
        &self,
        target_session_id: &str,
        call_id: &str,
        decision: astrcode_core::permission::ApprovalDecision,
    ) -> Result<(), SessionApiError> {
        let target_sid = SessionId::from(target_session_id);
        let session = self
            .session_manager
            .open(target_sid.clone())
            .await
            .map_err(|_| SessionApiError::NotFound("session not found".into()))?;
        session
            .runtime()
            .resolve_tool_approval(&astrcode_core::types::ToolCallId::from(call_id), decision)
            .map_err(|error| match error {
                astrcode_session::ToolApprovalResolveError::NotPending { .. } => {
                    SessionApiError::NotFound(error.to_string())
                },
                astrcode_session::ToolApprovalResolveError::ReceiverDropped { .. } => {
                    SessionApiError::SessionBusy(error.to_string())
                },
            })
    }
}

fn map_restore_error(error: crate::session_manager::SessionManagerError) -> SessionApiError {
    match error {
        crate::session_manager::SessionManagerError::Storage(
            astrcode_storage::StorageError::NotFound(_),
        ) => SessionApiError::NotFound(error.to_string()),
        crate::session_manager::SessionManagerError::Storage(
            astrcode_storage::StorageError::Unsupported(reason),
        ) => SessionApiError::Unsupported(reason),
        error => SessionApiError::internal(error),
    }
}

fn delivery_outcome(outcome: crate::turn_scheduler::DeliveryOutcome) -> SessionDeliveryOutcome {
    match outcome {
        crate::turn_scheduler::DeliveryOutcome::Started { turn_id } => {
            SessionDeliveryOutcome::Started {
                turn_id: turn_id.into_string(),
            }
        },
        crate::turn_scheduler::DeliveryOutcome::Injected { turn_id } => {
            SessionDeliveryOutcome::Injected {
                turn_id: turn_id.into_string(),
            }
        },
        crate::turn_scheduler::DeliveryOutcome::Queued { queue_len } => {
            SessionDeliveryOutcome::Queued { queue_len }
        },
    }
}
