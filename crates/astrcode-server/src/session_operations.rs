//! Extension-facing [`SessionOperations`] 适配层：参数转换与错误映射。

use std::sync::Arc;

use astrcode_core::{
    event::EventPayload,
    tool::{
        CreateRootSessionRequest, CreateSessionRequest, SessionApiError, SessionHandle,
        SessionOperations, SessionStatus, SubmitTurnRequest, SubmitTurnResult,
    },
    types::{SessionId, new_message_id},
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
            .map_err(|e| SessionApiError::Internal(e.to_string()))?;

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
        caller_session_id: &str,
        target_session_id: &str,
        content: String,
    ) -> Result<(), SessionApiError> {
        let caller_sid = SessionId::from(caller_session_id);
        let target_sid = SessionId::from(target_session_id);

        self.child_sessions
            .verify_access(&caller_sid, &target_sid)
            .await?;

        if self.scheduler.registry().has_active(&target_sid) {
            self.scheduler
                .deliver_input(
                    target_sid.clone(),
                    content,
                    InputDelivery::InjectIfRunningElseStart,
                )
                .await
                .map_err(|e| SessionApiError::Internal(e.to_string()))?;
        } else {
            let session = self
                .session_manager
                .open(target_sid.clone())
                .await
                .map_err(|e| SessionApiError::NotFound(e.to_string()))?;

            let message_id = new_message_id();
            session
                .emit_durable(
                    None,
                    EventPayload::UserMessage {
                        message_id,
                        text: content,
                    },
                )
                .await
                .map_err(|e| SessionApiError::Internal(e.to_string()))?;
        }

        self.session_manager.sync_durable_events(&target_sid).await;
        Ok(())
    }

    async fn submit_turn(
        &self,
        caller_session_id: &str,
        request: SubmitTurnRequest,
    ) -> Result<SubmitTurnResult, SessionApiError> {
        let caller_sid = SessionId::from(caller_session_id);
        let target_sid = SessionId::from(request.target_session_id.as_str());

        self.child_sessions
            .verify_access(&caller_sid, &target_sid)
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
        caller_session_id: &str,
        target_session_id: &str,
    ) -> Result<SessionStatus, SessionApiError> {
        let caller_sid = SessionId::from(caller_session_id);
        let target_sid = SessionId::from(target_session_id);

        self.child_sessions
            .verify_access(&caller_sid, &target_sid)
            .await?;

        let view = self
            .scheduler
            .execution_view(&target_sid)
            .await
            .map_err(|e| SessionApiError::NotFound(e.to_string()))?;

        Ok(SessionStatus {
            alive: true,
            has_active_turn: view.active_turn_id.is_some(),
            last_finish_reason: None,
            message_count: self
                .session_manager
                .read_model(&target_sid)
                .await
                .map_err(|e| SessionApiError::NotFound(e.to_string()))?
                .messages
                .len(),
        })
    }

    async fn recycle_session(
        &self,
        caller_session_id: &str,
        target_session_id: &str,
    ) -> Result<(), SessionApiError> {
        let caller_sid = SessionId::from(caller_session_id);
        let target_sid = SessionId::from(target_session_id);

        self.child_sessions
            .verify_access(&caller_sid, &target_sid)
            .await?;

        self.child_sessions
            .recycle_child(self.scheduler.as_ref(), &caller_sid, &target_sid)
            .await;

        Ok(())
    }

    async fn delete_session(
        &self,
        caller_session_id: &str,
        target_session_id: &str,
    ) -> Result<(), SessionApiError> {
        let caller_sid = SessionId::from(caller_session_id);
        let target_sid = SessionId::from(target_session_id);

        self.child_sessions
            .verify_access(&caller_sid, &target_sid)
            .await?;

        if let Err(e) = self.scheduler.abort(&target_sid).await {
            tracing::warn!(%target_sid, error = %e, "abort failed before session delete");
        }
        self.scheduler.abort_and_cleanup(&target_sid).await;
        self.session_manager
            .delete(&target_sid)
            .await
            .map_err(|e| SessionApiError::Internal(e.to_string()))?;

        Ok(())
    }

    async fn restore_session(
        &self,
        caller_session_id: &str,
        target_session_id: &str,
    ) -> Result<(), SessionApiError> {
        let caller_sid = SessionId::from(caller_session_id);
        let target_sid = SessionId::from(target_session_id);

        self.child_sessions
            .verify_access(&caller_sid, &target_sid)
            .await?;

        self.session_manager
            .restore_session(&target_sid)
            .await
            .map_err(|e| SessionApiError::Internal(e.to_string()))?;

        Ok(())
    }
}
