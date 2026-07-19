//! Extension-facing [`SessionOperations`] 适配层：参数转换与错误映射。

use std::sync::Arc;

use astrcode_core::{
    tool::{
        CreateRootSessionRequest, CreateSessionRequest, SessionAccess, SessionApiError,
        SessionDeliveryOutcome, SessionExecutionView, SessionHandle, SessionOperations,
        SessionStatus, SubmitTurnRequest, SubmitTurnResult,
    },
    types::SessionId,
};

use crate::{
    child_session::{ChildCleanup, ChildSessionCoordinator},
    session_manager::SessionManager,
    turn_scheduler::{InputDelivery, TurnScheduler},
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
                crate::turn_scheduler::PromptInput::text_only(content),
                delivery,
            )
            .await
            .map_err(SessionApiError::internal)?;

        self.session_manager.sync_durable_events(&target_sid).await;
        Ok(delivery_outcome(outcome))
    }
}

#[async_trait::async_trait]
impl SessionOperations for ServerSessionOperations {
    async fn create_root_session(
        &self,
        request: CreateRootSessionRequest,
    ) -> Result<SessionHandle, SessionApiError> {
        let created = self
            .session_manager
            .create(&request.working_dir)
            .await
            .map_err(SessionApiError::internal)?;

        Ok(SessionHandle {
            session_id: created.session.id().clone().into_string(),
        })
    }

    async fn create_session(
        &self,
        parent_session_id: &str,
        request: CreateSessionRequest,
    ) -> Result<SessionHandle, SessionApiError> {
        let parent_sid = SessionId::from(parent_session_id);
        let child = self
            .child_sessions
            .spawn_child(&parent_sid, request)
            .await?;

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

    async fn interrupt_and_submit(
        &self,
        access: SessionAccess<'_>,
        content: String,
    ) -> Result<SessionDeliveryOutcome, SessionApiError> {
        self.deliver_message(access, content, InputDelivery::InterruptAndStart)
            .await
    }

    async fn cancel_turn(&self, access: SessionAccess<'_>) -> Result<(), SessionApiError> {
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
            active_turn_id: view.active_turn_id.map(|turn_id| turn_id.into_string()),
            queued_inputs: view.queued_inputs,
        })
    }

    async fn submit_turn(
        &self,
        request: SubmitTurnRequest,
    ) -> Result<SubmitTurnResult, SessionApiError> {
        let (caller_sid, target_sid) = self
            .verified_session_ids(request.access.as_access())
            .await?;

        if request.wait_for_result {
            let content = self
                .child_sessions
                .submit_turn_sync(
                    self.scheduler.as_ref(),
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

    async fn recycle_session(&self, access: SessionAccess<'_>) -> Result<(), SessionApiError> {
        let (caller_sid, target_sid) = self.verified_session_ids(access).await?;

        self.child_sessions
            .recycle_child(self.scheduler.as_ref(), &caller_sid, &target_sid)
            .await;

        Ok(())
    }

    async fn delete_session(&self, access: SessionAccess<'_>) -> Result<(), SessionApiError> {
        let (_, target_sid) = self.verified_session_ids(access).await?;

        if let Err(e) = self.scheduler.abort(&target_sid).await {
            tracing::warn!(%target_sid, error = %e, "abort failed before session delete");
        }
        self.scheduler
            .delete_session(&target_sid)
            .await
            .map_err(SessionApiError::internal)?;

        Ok(())
    }

    async fn restore_session(&self, access: SessionAccess<'_>) -> Result<(), SessionApiError> {
        let (_, target_sid) = self.verified_session_ids(access).await?;

        self.session_manager
            .restore_session(&target_sid)
            .await
            .map_err(SessionApiError::internal)?;

        Ok(())
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
                astrcode_session::session_runtime::ToolApprovalResolveError::NotPending {
                    ..
                } => SessionApiError::NotFound(error.to_string()),
                astrcode_session::session_runtime::ToolApprovalResolveError::ReceiverDropped {
                    ..
                } => SessionApiError::SessionBusy(error.to_string()),
            })
    }

    async fn resolve_tool_ui_response(
        &self,
        target_session_id: &str,
        call_id: &str,
        answers: std::collections::BTreeMap<String, String>,
    ) -> Result<(), SessionApiError> {
        let target_sid = SessionId::from(target_session_id);
        let session = self
            .session_manager
            .open(target_sid.clone())
            .await
            .map_err(|_| SessionApiError::NotFound("session not found".into()))?;
        session
            .runtime()
            .resolve_tool_ui_response(&astrcode_core::types::ToolCallId::from(call_id), answers)
            .map_err(|error| {
                match error {
                astrcode_session::session_runtime::ToolUiResponseResolveError::NotPending {
                    ..
                } => SessionApiError::NotFound(error.to_string()),
                astrcode_session::session_runtime::ToolUiResponseResolveError::ReceiverDropped {
                    ..
                } => SessionApiError::SessionBusy(error.to_string()),
            }
            })
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
