use astrcode_core::{event::LiveEventPayload, types::SessionId};
use astrcode_protocol::events::ClientNotification;
use astrcode_session::compaction::{
    IdleCompactionError, IdleCompactionOutcome, compact_idle_session,
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
        let Some(sid) = self.focused_session_id.clone() else {
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
            .emit_live(None, LiveEventPayload::CompactionStarted)
            .await;

        let outcome = self
            .run_manual_compaction(&session, sid, keep_recent_turns)
            .await;
        let terminal_event = match &outcome {
            Ok((ManualCompactOutcome::Compacted { .. }, messages_removed)) => {
                LiveEventPayload::CompactionCompleted {
                    messages_removed: *messages_removed,
                }
            },
            Ok((ManualCompactOutcome::Skipped { message }, _)) => {
                LiveEventPayload::CompactionSkipped {
                    reason: message.clone(),
                }
            },
            Err(error) => LiveEventPayload::CompactionFailed {
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
        let result = compact_idle_session(session, keep_recent_turns)
            .await
            .map_err(|error| match error {
                IdleCompactionError::Session(error) => HandlerError::Session(error),
                IdleCompactionError::Extension(error) => HandlerError::Extension(error),
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
