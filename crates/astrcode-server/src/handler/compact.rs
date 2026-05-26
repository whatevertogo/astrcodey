use astrcode_core::{
    event::EventPayload, extension::CompactTrigger, storage::CompactSnapshotInput, types::SessionId,
};
use astrcode_protocol::events::ClientNotification;
use astrcode_session::{
    compact::compact_trigger_name,
    compaction_run::{IdleCompactionOutcome, IdleCompactionParams, compact_idle_session},
};

use super::{CommandHandler, HandlerError, session_snapshot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualCompactOutcome {
    Compacted { session_id: SessionId },
    Skipped { message: String },
}

impl CommandHandler {
    pub(super) async fn compact_active_session(
        &mut self,
        keep_recent_turns: Option<usize>,
    ) -> Result<(), HandlerError> {
        let Some(sid) = self.active_session_id.clone() else {
            self.send_error(40400, "No active session");
            return Ok(());
        };
        match self.compact_session(&sid, keep_recent_turns).await {
            Ok(ManualCompactOutcome::Compacted { .. }) => Ok(()),
            Ok(ManualCompactOutcome::Skipped { message }) => {
                self.send_error(40000, &message);
                Ok(())
            },
            Err(error) => {
                self.send_error(-32603, &error.to_string());
                Err(error)
            },
        }
    }

    /// 手动压缩指定会话。
    pub async fn compact_session(
        &mut self,
        sid: &SessionId,
        keep_recent_turns: Option<usize>,
    ) -> Result<ManualCompactOutcome, HandlerError> {
        if self.scheduler.registry().has_active(sid) {
            self.send_error(40900, "Cannot compact while a turn is running");
            return Err(HandlerError::CompactBlocked);
        }

        let session = self
            .runtime
            .session_manager
            .open(sid.clone())
            .await
            .map_err(HandlerError::SessionManager)?;

        session
            .emit_live(None, EventPayload::CompactionStarted)
            .await;

        let outcome = self
            .run_manual_compaction(&session, sid, keep_recent_turns)
            .await;
        let terminal_event = match &outcome {
            Ok((ManualCompactOutcome::Compacted { .. }, messages_removed)) => {
                EventPayload::CompactionCompleted {
                    messages_removed: *messages_removed,
                }
            },
            Ok((ManualCompactOutcome::Skipped { message }, _)) => EventPayload::CompactionSkipped {
                reason: message.clone(),
            },
            Err(error) => EventPayload::CompactionFailed {
                reason: error.to_string(),
            },
        };
        session.emit_live(None, terminal_event).await;

        outcome.map(|(result, _)| result)
    }

    async fn run_manual_compaction(
        &mut self,
        session: &astrcode_session::Session,
        sid: &SessionId,
        keep_recent_turns: Option<usize>,
    ) -> Result<(ManualCompactOutcome, usize), HandlerError> {
        let state = session.read_model().await.map_err(HandlerError::Session)?;
        let tool_registry = {
            let current = session.runtime().tool_registry();
            if current.list_definitions().is_empty() {
                session.refresh_tools(&state.working_dir).await
            } else {
                current
            }
        };
        let tools = tool_registry.list_definitions();
        let provider_messages = state.provider_messages();

        let snapshot_path = session
            .write_compact_snapshot(CompactSnapshotInput {
                trigger: compact_trigger_name(CompactTrigger::ManualCommand).into(),
                model_id: state.model_id.clone(),
                working_dir: state.working_dir.clone(),
                system_prompt: state.system_prompt.clone(),
                provider_messages: provider_messages.clone(),
            })
            .await
            .map_err(HandlerError::Session)?;

        let llm = self.runtime.config_manager().read_llm_provider();
        let context_assembler = self.runtime.context_assembler();
        let result = compact_idle_session(
            session,
            self.runtime.extension_runner(),
            context_assembler,
            llm,
            &state,
            &tools,
            IdleCompactionParams {
                keep_recent_turns,
                transcript_path: snapshot_path,
            },
        )
        .await
        .map_err(|error| match error {
            astrcode_session::compaction_run::IdleCompactionError::Session(e) => {
                HandlerError::Session(e)
            },
            astrcode_session::compaction_run::IdleCompactionError::Extension(e) => {
                HandlerError::Extension(e)
            },
            astrcode_session::compaction_run::IdleCompactionError::Persist(e) => {
                HandlerError::InvalidRequest(e.to_string())
            },
            astrcode_session::compaction_run::IdleCompactionError::InvalidRequest(message) => {
                HandlerError::InvalidRequest(message)
            },
        })?;

        match result {
            IdleCompactionOutcome::Skipped { message } => {
                Ok((ManualCompactOutcome::Skipped { message }, 0))
            },
            IdleCompactionOutcome::Compacted { messages_removed } => {
                let state = session.read_model().await.map_err(HandlerError::Session)?;
                self.event_bus
                    .send_notification(ClientNotification::SessionResumed {
                        session_id: sid.clone().into_string(),
                        snapshot: session_snapshot(&state),
                    });

                Ok((
                    ManualCompactOutcome::Compacted {
                        session_id: sid.clone(),
                    },
                    messages_removed,
                ))
            },
        }
    }
}
