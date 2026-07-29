//! Prompt 提交、注入与斜杠命令拦截。

use astrcode_core::{message_attachment::MessageAttachment, types::SessionId};

use super::{CommandHandler, HandlerError, PromptSubmission, slash};
use crate::turn_scheduler::PromptInput;

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
        if self.scheduler.registry().has_active(&sid) {
            return self.inject_mid_turn_message_for_session(&sid, text).await;
        }
        match self.submit_input_for_session(sid.clone(), input).await {
            Ok(_) => Ok(()),
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
        self.inject_input_for_session(sid.clone(), text)
            .await
            .map(|_| ())
    }

    /// Mid-turn 注入：要求当前 session 有活跃 turn，经 [`InputDelivery::InjectOnly`]
    /// 写入 durable `UserMessage`，由 `TurnRunner` 在下一 agent step 并入 LLM 上下文。
    pub async fn inject_input_for_session(
        &self,
        sid: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HandlerError> {
        self.session_commands.inject_input(sid, text).await
    }

    pub async fn submit_input_for_session(
        &mut self,
        sid: SessionId,
        input: PromptInput,
    ) -> Result<PromptSubmission, HandlerError> {
        if let Some(command) =
            slash::parse_slash_command(&input.text).filter(|command| command.has_name())
        {
            if command.name == "model" {
                return self.execute_command_for_session(sid, command).await;
            }
        }
        self.session_commands.submit_input(sid, input).await
    }
}
