//! 交互式 handler 对 session-scoped command 服务的薄适配。

use astrcode_core::types::SessionId;
use astrcode_protocol::events::ClientNotification;

use super::{
    CommandHandler, CommandInvocation, CommandOutcome, HandlerError, PromptSubmission, slash,
};
use crate::{
    protocol_mapping::{command_info_to_dto, keybinding_to_dto, status_item_to_info_dto},
    session_command_contract::{CommandList, ParsedSlashCommand},
};

impl CommandHandler {
    pub(super) async fn send_extension_command_list(&self) {
        let working_dir = match self.active_session_working_dir().await {
            Ok(working_dir) => working_dir,
            Err(error) => {
                self.send_error(40400, &error);
                return;
            },
        };
        let command_list = self.command_list_for_working_dir(&working_dir).await;
        let commands = command_list
            .commands
            .into_iter()
            .map(command_info_to_dto)
            .collect();
        let keybindings = command_list
            .keybindings
            .into_iter()
            .map(keybinding_to_dto)
            .collect();
        let status_items = command_list
            .status_items
            .into_iter()
            .map(status_item_to_info_dto)
            .collect();
        self.event_bus
            .send_notification(ClientNotification::ExtensionCommandList {
                commands,
                keybindings,
                status_items,
            });
    }

    pub(super) async fn execute_extension_command(
        &mut self,
        command_name: String,
        arguments: String,
    ) -> Result<(), HandlerError> {
        let session_id = self.ensure_session().await?;
        let command = ParsedSlashCommand {
            name: command_name,
            arguments,
        };
        if let Err(error) = self.invoke_command_for_session(session_id, command).await {
            self.send_error(slash::command_error_code(&error), &error.to_string());
        }
        Ok(())
    }

    pub(in crate::handler) async fn invoke_command_for_session(
        &mut self,
        session_id: SessionId,
        command: ParsedSlashCommand,
    ) -> Result<CommandInvocation, HandlerError> {
        match self
            .session_commands
            .invoke_interactive_command(session_id, command)
            .await?
        {
            CommandOutcome::Invocation(invocation) => Ok(invocation),
            CommandOutcome::ModelSelection => {
                self.start_model_selection();
                Ok(CommandInvocation::Handled {
                    message: "model selection started".into(),
                })
            },
        }
    }

    pub(in crate::handler) async fn execute_command_for_session(
        &mut self,
        session_id: SessionId,
        command: ParsedSlashCommand,
    ) -> Result<PromptSubmission, HandlerError> {
        Ok(CommandInvocation::into_prompt_submission(
            self.invoke_command_for_session(session_id, command).await?,
        ))
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
