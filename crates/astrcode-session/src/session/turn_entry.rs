use std::sync::Arc;

use astrcode_core::{event::EventPayload, types::*};
use tokio::sync::oneshot;

use super::Session;
use crate::{
    background::{BackgroundTaskCompletion, spawn_background_forwarder},
    turn_context::TurnError,
    turn_handle::TurnHandle,
    turn_runner::{RunTurnResult, TurnRunner, run_turn},
};

impl Session {
    async fn emit_turn_start_events(&self, text: &str, turn_id: &TurnId) -> Result<(), TurnError> {
        self.emit_durable(Some(turn_id), EventPayload::TurnStarted)
            .await
            .map_err(|e| TurnError::DurableEmitFailed(format!("TurnStarted: {e}")))?;
        self.emit_durable(
            Some(turn_id),
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: text.to_string(),
            },
        )
        .await
        .map_err(|e| TurnError::DurableEmitFailed(format!("UserMessage: {e}")))?;
        self.emit_live(Some(turn_id), EventPayload::AgentRunStarted)
            .await;
        Ok(())
    }

    async fn prepare_turn_runner(&self) -> Result<TurnRunner, TurnError> {
        let model = self.runtime.model_binding();
        if let Err(e) = self.update_model_id(model.model_id()).await {
            tracing::warn!(session_id = %self.id, error = %e, "failed to update session model_id");
        }

        let pre_state = self
            .read_model()
            .await
            .map_err(|e| TurnError::SessionReadFailed(format!("read session: {e}")))?;
        let working_dir = pre_state.working_dir.clone();

        if self.runtime.tool_registry().list_definitions().is_empty() {
            self.refresh_tools(&working_dir).await;
        }

        let stored_fingerprint = pre_state.system_prompt_fingerprint.clone();
        let prompt_changed = match self
            .refresh_prompt_with_state(
                &working_dir,
                None,
                stored_fingerprint.as_deref(),
                Some(&pre_state),
                model.model_id(),
            )
            .await
        {
            Ok(changed) => changed,
            Err(e) => {
                tracing::warn!(session_id = %self.id, error = %e, "configure system prompt failed");
                false
            },
        };

        let (background_result_tx, background_result_rx) =
            tokio::sync::mpsc::unbounded_channel::<BackgroundTaskCompletion>();
        let bg_session = Arc::new(self.clone());
        let _forwarder = spawn_background_forwarder(background_result_rx, bg_session);

        let session_state = if prompt_changed {
            self.read_model()
                .await
                .map_err(|e| TurnError::SessionReadFailed(format!("re-read session: {e}")))?
        } else {
            pre_state
        };
        let session_store_dir = self.session_store_dir().await;
        TurnRunner::new_with_llm(
            Arc::new(self.clone()),
            &session_state,
            Some(background_result_tx),
            session_store_dir,
            Arc::clone(model.llm()),
        )
    }

    async fn run_and_finalize_turn(
        session: Arc<Self>,
        mut agent: TurnRunner,
        text: String,
        turn_id: TurnId,
        completion_tx: oneshot::Sender<RunTurnResult>,
    ) {
        let result = run_turn(&mut agent, &text, &turn_id).await;
        let finish_reason = match &result.output {
            Ok(out) => out.finish_reason.clone(),
            Err(_) => "error".into(),
        };
        let pending_error = match (&result.output, result.emitted_error) {
            (Err(e), false) => Some(e.to_string()),
            _ => None,
        };

        let _ = completion_tx.send(result);
        if let Some(error_msg) = pending_error {
            let _ = session
                .emit_durable(
                    Some(&turn_id),
                    EventPayload::ErrorOccurred {
                        code: -32603,
                        message: error_msg,
                        recoverable: false,
                    },
                )
                .await;
        }
        let _ = session
            .emit_durable(
                Some(&turn_id),
                EventPayload::TurnCompleted {
                    finish_reason: finish_reason.clone(),
                },
            )
            .await;
        session
            .emit_live(
                Some(&turn_id),
                EventPayload::AgentRunCompleted {
                    reason: finish_reason,
                },
            )
            .await;
    }

    pub async fn submit(&self, text: String, turn_id: TurnId) -> Result<TurnHandle, TurnError> {
        self.emit_turn_start_events(&text, &turn_id).await?;
        let agent = self.prepare_turn_runner().await?;
        let (completion_tx, completion_rx) = oneshot::channel();
        let turn_id_for_task = turn_id.clone();
        let session_for_completion = Arc::new(self.clone());
        let join = tokio::spawn(async move {
            Self::run_and_finalize_turn(
                session_for_completion,
                agent,
                text,
                turn_id_for_task,
                completion_tx,
            )
            .await;
        });

        Ok(TurnHandle::new(turn_id, join, completion_rx))
    }
}
