//! Prompt 提交、注入与斜杠命令拦截。

use astrcode_core::types::SessionId;

use super::{CommandHandler, HandlerError, PromptSubmission, slash};

impl CommandHandler {
    pub(super) async fn submit_prompt(&mut self, text: String) -> Result<(), HandlerError> {
        let sid = self.ensure_session().await?;
        match self
            .submit_input_for_session(sid.clone(), text.clone())
            .await
        {
            Ok(_) => Ok(()),
            Err(HandlerError::TurnAlreadyRunning) => {
                self.inject_mid_turn_message_for_session(&sid, text).await
            },
            Err(error) => {
                self.send_error(slash::command_error_code(&error), &error.to_string());
                Err(error)
            },
        }
    }

    pub(super) async fn inject_mid_turn_message(
        &mut self,
        text: String,
    ) -> Result<(), HandlerError> {
        let sid = self.ensure_session().await?;
        self.inject_mid_turn_message_for_session(&sid, text).await
    }

    pub(super) async fn inject_mid_turn_message_for_session(
        &self,
        sid: &SessionId,
        text: String,
    ) -> Result<(), HandlerError> {
        if !self.scheduler.registry().has_active(sid) {
            self.send_error(40400, "No active turn");
            return Err(HandlerError::NoActiveTurn);
        }
        self.scheduler
            .deliver_input(
                sid.clone(),
                text,
                crate::turn_scheduler::InputDelivery::InjectIfRunningElseStart,
            )
            .await
            .map_err(HandlerError::from)?;
        Ok(())
    }

    pub async fn submit_input_for_session(
        &mut self,
        sid: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HandlerError> {
        if let Some(command) = slash::parse_slash_command(&text) {
            match self
                .execute_slash_command_for_session(sid.clone(), command, text.clone())
                .await
            {
                Err(HandlerError::UnknownCommand(_)) => {},
                other => return other,
            }
        }

        self.start_turn_for_session(sid, text, None)
            .await
            .map(|turn_id| PromptSubmission::Accepted { turn_id })
    }

    pub async fn command_infos_for_session(
        &self,
        sid: &SessionId,
    ) -> Result<Vec<astrcode_protocol::events::ExtensionCommandInfo>, HandlerError> {
        let state = self
            .runtime
            .session_manager()
            .read_model(sid)
            .await
            .map_err(HandlerError::SessionManager)?;
        Ok(self.command_infos_for_working_dir(&state.working_dir).await)
    }
}
