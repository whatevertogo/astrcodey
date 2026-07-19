//! `ClientCommand` 路由。

use astrcode_core::types::SessionId;
use astrcode_protocol::{
    commands::ClientCommand,
    events::{ClientNotification, SessionListItem},
};

use super::{CommandHandler, CommandInvocation, HandlerError, slash};

impl CommandHandler {
    /// 处理客户端命令，路由到对应处理方法。
    pub async fn handle(&mut self, cmd: ClientCommand) -> Result<(), HandlerError> {
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
                match self.runtime.session_manager().list_summaries().await {
                    Ok(summaries) => {
                        let items: Vec<_> = summaries
                            .into_iter()
                            .map(|summary| SessionListItem {
                                session_id: summary.session_id.into_string(),
                                created_at: summary.created_at,
                                last_active_at: summary.updated_at,
                                working_dir: summary.working_dir.clone(),
                                parent_session_id: summary
                                    .parent_session_id
                                    .map(SessionId::into_string),
                                title: summary.first_user_message,
                            })
                            .collect();
                        self.event_bus
                            .send_notification(ClientNotification::SessionList { sessions: items });
                    },
                    Err(e) => {
                        self.send_error(-32603, &e.to_string());
                        return Err(HandlerError::SessionManager(e));
                    },
                }
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
                self.scheduler.abort_and_cleanup(&session_id).await;
                match self.runtime.session_manager().delete(&session_id).await {
                    Ok(()) => {
                        if self.focused_session_id.as_ref() == Some(&session_id) {
                            self.focused_session_id = None;
                        }
                    },
                    Err(e) => self.send_error(40401, &format!("Session not found: {e}")),
                }
            },

            ClientCommand::ListExtensionCommands => {
                let working_dir = match self.active_session_working_dir().await {
                    Ok(working_dir) => working_dir,
                    Err(error) => {
                        self.send_error(40400, &error);
                        return Ok(());
                    },
                };
                let infos = self
                    .command_list_for_working_dir(&working_dir)
                    .await
                    .commands;
                let keybindings = self.runtime.extension_runner().collect_keybindings();
                let status_items: Vec<astrcode_protocol::events::StatusItemInfoDto> = self
                    .runtime
                    .extension_runner()
                    .collect_status_items()
                    .into_iter()
                    .map(|item| astrcode_protocol::events::StatusItemInfoDto {
                        id: item.id,
                        text: item.text,
                        priority: item.priority,
                    })
                    .collect();
                self.event_bus
                    .send_notification(ClientNotification::ExtensionCommandList {
                        commands: infos,
                        keybindings,
                        status_items,
                    });
            },

            ClientCommand::ExecuteExtensionCommand {
                command_name,
                arguments,
            } => {
                let sid = self.ensure_session().await?;
                match self
                    .invoke_command_for_session(
                        sid,
                        slash::ParsedSlashCommand {
                            name: command_name,
                            arguments,
                        },
                    )
                    .await
                {
                    Ok(CommandInvocation::Display { .. })
                    | Ok(CommandInvocation::Handled { .. })
                    | Ok(CommandInvocation::Started { .. }) => {},
                    Err(error) => {
                        self.send_error(slash::command_error_code(&error), &error.to_string());
                    },
                }
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
                let Some(ops) = self.runtime.capabilities().session_ops() else {
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
