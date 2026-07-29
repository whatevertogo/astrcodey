//! 交互式 handler 对 turn 生命周期服务的薄适配。

use astrcode_core::types::SessionId;
#[cfg(test)]
use astrcode_core::types::TurnId;
#[cfg(test)]
use astrcode_core::user_input::UserInput;

use super::{CommandHandler, HandlerError};

impl CommandHandler {
    pub(in crate::handler) async fn abort_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), HandlerError> {
        match self.session_commands.abort_session(session_id).await {
            Err(HandlerError::NoActiveTurn) => {
                self.send_error(40400, "No active turn");
                Err(HandlerError::NoActiveTurn)
            },
            result => result,
        }
    }

    pub(in crate::handler) async fn abort_active_turn(&self) -> Result<(), HandlerError> {
        let Some(session_id) = self.focused_session_id.as_ref() else {
            self.send_error(40400, "No active turn");
            return Ok(());
        };
        self.abort_session(session_id).await
    }

    pub(in crate::handler) async fn repair_stale_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), HandlerError> {
        self.session_commands.repair_stale_session(session_id).await
    }

    #[cfg(test)]
    pub(in crate::handler) async fn submit_input_with_completion(
        &self,
        session_id: SessionId,
        input: UserInput,
    ) -> Result<
        (
            TurnId,
            tokio::sync::oneshot::Receiver<crate::turn_scheduler::TurnCompletion>,
        ),
        HandlerError,
    > {
        self.session_commands
            .submit_input_with_completion(session_id, input)
            .await
    }
}
