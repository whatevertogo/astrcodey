//! Prompt 提交、注入与斜杠命令拦截。

use astrcode_core::{message_attachment::MessageAttachment, types::SessionId};

use super::{CommandHandler, HandlerError, PromptSubmission, slash};
use crate::turn_scheduler::{DeliveryOutcome, InputDelivery, PromptInput};

fn validate_prompt_attachments(attachments: &[MessageAttachment]) -> Result<(), HandlerError> {
    astrcode_core::message_attachment::validate_attachments(attachments)
        .map_err(|error| HandlerError::InvalidRequest(error.to_string()))
}

impl CommandHandler {
    pub(super) async fn submit_prompt(
        &mut self,
        text: String,
        attachments: Vec<MessageAttachment>,
    ) -> Result<(), HandlerError> {
        let sid = self.ensure_session().await?;
        let input = PromptInput {
            text: text.clone(),
            attachments,
        };
        match self.submit_input_for_session(sid.clone(), input).await {
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
        match self.inject_input_for_session(sid.clone(), text).await? {
            PromptSubmission::Handled { .. } => Ok(()),
            PromptSubmission::Accepted { .. } => Ok(()),
        }
    }

    /// Mid-turn 注入：要求当前 session 有活跃 turn，经 [`InputDelivery::InjectIfRunningElseStart`]
    /// 写入 durable `UserMessage`，由 `TurnRunner` 在下一 agent step 并入 LLM 上下文。
    pub async fn inject_input_for_session(
        &self,
        sid: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HandlerError> {
        if !self.scheduler.registry().has_active(&sid) {
            return Err(HandlerError::NoActiveTurn);
        }
        match self
            .scheduler
            .deliver_input(
                sid,
                PromptInput::text_only(text),
                InputDelivery::InjectIfRunningElseStart,
            )
            .await?
        {
            DeliveryOutcome::Injected { .. } => Ok(PromptSubmission::Handled {
                message: "injected into active turn".into(),
            }),
            DeliveryOutcome::Started { turn_id } => Ok(PromptSubmission::Accepted { turn_id }),
            DeliveryOutcome::Queued { .. } => unreachable!("inject delivery never enqueues"),
        }
    }

    pub async fn submit_input_for_session(
        &mut self,
        sid: SessionId,
        input: PromptInput,
    ) -> Result<PromptSubmission, HandlerError> {
        validate_prompt_attachments(&input.attachments)?;
        if let Some(command) = slash::parse_slash_command(&input.text) {
            match self
                .execute_slash_command_for_session(sid.clone(), command, input.text.clone())
                .await
            {
                Err(HandlerError::UnknownCommand(_)) => {},
                other => return other,
            }
        }

        self.start_turn_for_session(sid, input.text, input.attachments, None)
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
