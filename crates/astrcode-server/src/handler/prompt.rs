//! Prompt 提交、注入与斜杠命令拦截。

use astrcode_core::{
    message_attachment::MessageAttachment, types::SessionId, user_input::UserInput,
};

use super::{CommandHandler, HandlerError, PromptSubmission, slash};
use crate::session_command_contract::{ParsedSlashCommand, parse_slash_command};

impl CommandHandler {
    /// 发送错误通知并原样返回错误，供 handler 的 `send_error + return Err` 两步合并。
    fn report_error(&self, code: i32, error: HandlerError) -> HandlerError {
        self.send_error(code, &error.to_string());
        error
    }

    pub(super) async fn submit_prompt(
        &mut self,
        text: String,
        attachments: Vec<MessageAttachment>,
    ) -> Result<(), HandlerError> {
        let sid = self.ensure_session().await?;
        let input = UserInput {
            text: text.clone(),
            attachments,
        };
        if let Some(command) = parse_slash_command(&input.text).filter(ParsedSlashCommand::has_name)
        {
            match self.execute_command_for_session(sid.clone(), command).await {
                Ok(_) => return Ok(()),
                // 未注册的斜杠命令按普通文本透传给模型，与 TUI/HTTP 端行为一致；
                // 只有显式命令端点(ExecuteExtensionCommand)保留 UnknownCommand 报错。
                Err(HandlerError::UnknownCommand(_)) => {},
                Err(error) => {
                    return Err(self.report_error(slash::command_error_code(&error), error));
                },
            }
        }
        if self.scheduler.registry().has_active(&sid) {
            return self.inject_mid_turn_message_for_session(&sid, text).await;
        }
        match self.session_commands.submit_input(sid, input).await {
            Ok(_) => Ok(()),
            Err(error) => Err(self.report_error(slash::command_error_code(&error), error)),
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
            return Err(self.report_error(40400, HandlerError::NoActiveTurn));
        }
        self.inject_input_for_session(sid.clone(), text)
            .await
            .map(|_| ())
    }

    /// Mid-turn 注入：要求当前 session 有活跃 turn，经 [`InputDelivery::InjectOnly`]
    /// 写入 durable `UserMessage`，由 `TurnRunner` 在下一 agent step 并入 LLM 上下文。
    pub(crate) async fn inject_input_for_session(
        &self,
        sid: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HandlerError> {
        self.session_commands.inject_input(sid, text).await
    }
}
