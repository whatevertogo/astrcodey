use astrcode_core::types::SessionId;

use super::{CommandHandler, HandlerError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualCompactOutcome {
    Compacted { session_id: SessionId },
    Skipped { message: String },
}

impl CommandHandler {
    pub(super) async fn compact_active_session(
        &self,
        keep_recent_turns: Option<usize>,
    ) -> Result<(), HandlerError> {
        let Some(session_id) = self.focused_session_id.clone() else {
            self.send_error(40400, "No active session");
            return Ok(());
        };
        match self.compact_session(&session_id, keep_recent_turns).await {
            Ok(ManualCompactOutcome::Compacted { .. }) => Ok(()),
            Ok(ManualCompactOutcome::Skipped { message }) => {
                self.send_error(40000, &message);
                Ok(())
            },
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn compact_session(
        &self,
        session_id: &SessionId,
        keep_recent_turns: Option<usize>,
    ) -> Result<ManualCompactOutcome, HandlerError> {
        let result = self
            .session_commands
            .compact_session(session_id, keep_recent_turns)
            .await;
        if let Err(error) = &result {
            let code = if matches!(error, HandlerError::CompactBlocked) {
                40900
            } else {
                -32603
            };
            self.send_error(code, &error.to_string());
        }
        result
    }
}
