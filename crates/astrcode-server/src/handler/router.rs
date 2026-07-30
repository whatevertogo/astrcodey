//! `ClientCommand` 路由。

use astrcode_core::types::SessionId;
use astrcode_protocol::commands::ClientCommand;

use super::{CommandHandler, HandlerError};

impl CommandHandler {
    /// 处理客户端命令，路由到对应处理方法。
    pub(crate) async fn handle(&mut self, cmd: ClientCommand) -> Result<(), HandlerError> {
        match cmd {
            ClientCommand::CreateSession { working_dir } => {
                self.create_session(working_dir).await?;
            },

            ClientCommand::SubmitPrompt { text, attachments } => {
                self.submit_prompt(text, attachments).await?;
            },

            ClientCommand::InjectMessage { text } => {
                self.inject_mid_turn_message(text).await?;
            },

            ClientCommand::Recap => {
                self.recap_session().await?;
            },

            ClientCommand::ListSessions => {
                self.send_session_list().await?;
            },

            ClientCommand::Abort => {
                self.abort_active_turn().await?;
            },

            ClientCommand::Compact { keep_recent_turns } => {
                self.compact_active_session(keep_recent_turns).await?;
            },

            ClientCommand::GetState => {
                self.send_current_state().await;
            },

            ClientCommand::ResumeSession { session_id }
            | ClientCommand::SwitchSession { session_id } => {
                self.resume_session(session_id.into()).await;
            },

            ClientCommand::DeleteSession { session_id } => {
                let session_id = SessionId::from(session_id);
                match self.session_commands.delete_session(&session_id).await {
                    Ok(()) => {
                        if self.focused_session_id.as_ref() == Some(&session_id) {
                            self.focused_session_id = None;
                        }
                    },
                    Err(e) => self.send_error(40401, &format!("Session not found: {e}")),
                }
            },

            ClientCommand::ListExtensionCommands => {
                self.send_extension_command_list().await;
            },

            ClientCommand::ExecuteExtensionCommand {
                command_name,
                arguments,
            } => {
                self.execute_extension_command(command_name, arguments)
                    .await?;
            },

            ClientCommand::ForkSession {
                session_id,
                at_cursor,
            } => {
                self.fork_session(session_id.into(), at_cursor).await?;
            },

            ClientCommand::SetModel { model_id } => {
                self.set_model(model_id).await?;
            },

            ClientCommand::UiResponse { request_id, value } => {
                self.handle_ui_response(request_id, value).await?;
            },

            ClientCommand::ResolveToolApproval { call_id, decision } => {
                let sid = self.ensure_session().await?;
                let Some(ops) = self.runtime.runtime_services().session_ops() else {
                    self.send_error(-32603, "session operations unavailable");
                    return Ok(());
                };
                if let Err(error) = ops
                    .resolve_tool_approval(&sid.into_string(), &call_id, decision.into())
                    .await
                {
                    self.send_error(40400, &error.to_string());
                }
            },
        }
        Ok(())
    }
}
