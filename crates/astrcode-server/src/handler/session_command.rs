//! 交互式 handler 对 session-scoped command 服务的薄适配。

use astrcode_core::types::SessionId;
use astrcode_extension_sdk::extension::CommandCompletions;
use astrcode_protocol::events::ExtensionCommandInfoDto;

use super::{CommandHandler, CommandInvocation, HandlerError, PromptSubmission, slash};

pub struct CommandList {
    pub commands: Vec<ExtensionCommandInfoDto>,
}

impl CommandHandler {
    pub(in crate::handler) async fn invoke_command_for_session(
        &mut self,
        session_id: SessionId,
        command: slash::ParsedSlashCommand,
    ) -> Result<CommandInvocation, HandlerError> {
        if command.name.trim().trim_start_matches('/') == "model" {
            self.start_model_selection().await?;
            return Ok(CommandInvocation::Handled {
                message: "model selection started".into(),
            });
        }
        self.session_commands
            .invoke_command(session_id, command)
            .await
    }

    pub(in crate::handler) async fn execute_command_for_session(
        &mut self,
        session_id: SessionId,
        command: slash::ParsedSlashCommand,
    ) -> Result<PromptSubmission, HandlerError> {
        Ok(
            match self.invoke_command_for_session(session_id, command).await? {
                CommandInvocation::Display { content, is_error } => PromptSubmission::Handled {
                    message: if is_error {
                        format!("Error: {content}")
                    } else {
                        content
                    },
                },
                CommandInvocation::Handled { message } => PromptSubmission::Handled { message },
                CommandInvocation::Started { turn_id } => PromptSubmission::Accepted { turn_id },
            },
        )
    }

    pub(in crate::handler) async fn complete_command_for_session(
        &self,
        session_id: SessionId,
        command_name: String,
        argument: String,
        cursor: Option<usize>,
    ) -> Result<CommandCompletions, HandlerError> {
        self.session_commands
            .complete_command(session_id, command_name, argument, cursor)
            .await
    }

    pub(in crate::handler) async fn command_list_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<CommandList, HandlerError> {
        self.session_commands.command_list(session_id, true).await
    }

    pub(in crate::handler) async fn command_list_for_working_dir(
        &self,
        working_dir: &str,
    ) -> CommandList {
        self.session_commands
            .command_list_for_working_dir(working_dir, true)
            .await
    }
}
