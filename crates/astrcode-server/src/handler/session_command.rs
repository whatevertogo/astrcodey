//! 交互式 handler 对 session-scoped command 服务的薄适配。

use astrcode_core::types::SessionId;
use astrcode_protocol::events::ClientNotification;

use super::{CommandHandler, CommandInvocation, HandlerError, PromptSubmission, slash};
use crate::{
    protocol_mapping::{command_info_to_stdio_dto, keybinding_to_dto, status_item_to_info_dto},
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
        let commands = self
            .command_list_for_working_dir(&working_dir)
            .await
            .commands
            .into_iter()
            .map(command_info_to_stdio_dto)
            .collect();
        let keybindings = self
            .runtime
            .extension_runner()
            .collect_keybindings()
            .into_iter()
            .map(keybinding_to_dto)
            .collect();
        let status_items = self
            .runtime
            .extension_runner()
            .collect_status_items()
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
        if command.name.trim().trim_start_matches('/') == "model" {
            self.start_model_selection();
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
        command: ParsedSlashCommand,
    ) -> Result<PromptSubmission, HandlerError> {
        Ok(self
            .invoke_command_for_session(session_id, command)
            .await?
            .into_prompt_submission())
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
