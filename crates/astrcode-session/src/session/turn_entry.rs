use std::sync::Arc;

use astrcode_core::{event::EventPayload, storage::SessionReadModel, types::*};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::Session;
use crate::{
    payload::{TURN_FINISH_ABORTED, agent_run_completed_payload, turn_completed_payload},
    tool_exec::interrupted_tool_result,
    turn_context::TurnError,
    turn_handle::TurnHandle,
    turn_runner::{RunTurnResult, TurnLoop, run_turn},
};

impl Session {
    async fn emit_turn_start_events(&self, text: &str, turn_id: &TurnId) -> Result<(), TurnError> {
        self.emit_durable(Some(turn_id), EventPayload::TurnStarted)
            .await?;
        self.emit_durable(
            Some(turn_id),
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: text.to_string(),
            },
        )
        .await?;
        self.emit_live(Some(turn_id), EventPayload::AgentRunStarted)
            .await;
        Ok(())
    }

    async fn prepare_turn_runner(&self) -> Result<TurnLoop, TurnError> {
        let model = self.runtime.model_binding();
        if let Err(e) = self.update_model_id(model.model_id()).await {
            tracing::warn!(session_id = %self.id, error = %e, "failed to update session model_id");
        }

        let pre_state = self.read_model().await?;
        let working_dir = pre_state.working_dir.clone();

        if self
            .runtime
            .loaded_tool_registry()
            .list_definitions()
            .is_empty()
        {
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

        self.runtime.ensure_background_forwarder(self.clone());

        let session_state = if prompt_changed {
            // refresh_prompt 可能写入了 durable event，需重读 projection。
            self.read_model().await?
        } else {
            pre_state
        };
        let session_store_dir = self.session_store_dir().await;
        let cancellation_token = CancellationToken::new();
        TurnLoop::new_with_llm(
            self.clone(),
            &session_state,
            session_store_dir,
            Arc::clone(&model.llm),
            cancellation_token,
        )
    }

    async fn run_and_finalize_turn(
        session: Session,
        mut agent: TurnLoop,
        text: String,
        turn_id: TurnId,
        cancellation_token: CancellationToken,
        completion_tx: oneshot::Sender<RunTurnResult>,
    ) {
        let result = run_turn(&mut agent, &text, &turn_id).await;
        let finish_reason = match &result.output {
            Ok(out) => out.finish_reason.clone(),
            Err(TurnError::Aborted) => TURN_FINISH_ABORTED.into(),
            Err(_) => "error".into(),
        };
        let pending_error = match (&result.output, result.emitted_error) {
            (Err(TurnError::Aborted), _) => None,
            (Err(e), false) => Some(e.to_string()),
            _ => None,
        };
        let aborted = matches!(result.output, Err(TurnError::Aborted));

        if aborted {
            emit_aborted_turn_context(&session, &turn_id).await;
        }
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
                turn_completed_payload(finish_reason.clone()),
            )
            .await;
        session
            .emit_live(Some(&turn_id), agent_run_completed_payload(finish_reason))
            .await;
        cancellation_token.cancel();
        let _ = completion_tx.send(result);
    }

    pub async fn submit(&self, text: String, turn_id: TurnId) -> Result<TurnHandle, TurnError> {
        self.emit_turn_start_events(&text, &turn_id).await?;
        let agent = self.prepare_turn_runner().await?;
        let cancellation_token = agent.cancellation_token();
        let (completion_tx, completion_rx) = oneshot::channel();
        let turn_id_for_task = turn_id.clone();
        let session_for_completion = self.clone();
        let cancellation_for_task = cancellation_token.clone();
        let join = tokio::spawn(async move {
            Self::run_and_finalize_turn(
                session_for_completion,
                agent,
                text,
                turn_id_for_task,
                cancellation_for_task,
                completion_tx,
            )
            .await;
        });

        Ok(TurnHandle::new(
            turn_id,
            join,
            cancellation_token,
            completion_rx,
        ))
    }
}

async fn emit_aborted_turn_context(session: &Session, turn_id: &TurnId) {
    match session.read_model().await {
        Ok(state) => {
            if let Err(e) = emit_interrupted_tool_results(session, &state, turn_id).await {
                tracing::warn!(
                    session_id = %session.id(),
                    turn_id = %turn_id,
                    error = %e,
                    "failed to settle pending tool calls after abort"
                );
            }
        },
        Err(e) => {
            tracing::warn!(
                session_id = %session.id(),
                turn_id = %turn_id,
                error = %e,
                "failed to read session state after abort"
            );
        },
    }

    if let Err(e) = session
        .emit_durable(Some(turn_id), EventPayload::TurnAbortedContext)
        .await
    {
        tracing::warn!(
            session_id = %session.id(),
            turn_id = %turn_id,
            error = %e,
            "failed to write turn-aborted provider context"
        );
    }
}

async fn emit_interrupted_tool_results(
    session: &Session,
    state: &SessionReadModel,
    turn_id: &TurnId,
) -> Result<(), crate::SessionError> {
    for pending in state.tool_calls_needing_interruption() {
        let result = interrupted_tool_result(
            pending.call_id.clone(),
            &pending.tool_name,
            std::time::Duration::ZERO,
        );
        session
            .emit_durable(
                Some(turn_id),
                EventPayload::ToolCallCompleted {
                    call_id: pending.call_id.into(),
                    tool_name: pending.tool_name,
                    result,
                    arguments: String::new(),
                    arguments_json: None,
                },
            )
            .await?;
    }
    Ok(())
}
