//! 协议路由层。
//!
//! `CommandRouter` 是 transport / acp / http 进入服务的唯一入口。它解析
//! `ClientCommand`，把会改变 session durable state 的命令转发给对应的
//! `SessionActor`（通过 `SessionSupervisor`），自身只持有一个"前台 session"
//! 这样的全局交互状态。

use std::sync::Arc;

use astrcode_core::{event::EventPayload, types::*};
use astrcode_protocol::{
    commands::ClientCommand,
    events::{ClientNotification, SessionListItem},
};
use astrcode_tools::registry::ToolRegistry;

use crate::{
    bootstrap::ServerRuntime,
    events::ClientEventPublisher,
    session::{SessionDirectoryError, SessionSupervisor, session_snapshot},
};

mod handle;

pub use handle::CommandRouterHandle;

pub(crate) use crate::session::{ManualCompactOutcome, TurnCompletion};

/// 用户输入提交结果。
#[derive(Debug)]
pub enum PromptSubmission {
    Accepted { turn_id: TurnId },
    Handled { message: String },
}

/// 协议层错误。
#[derive(Debug, thiserror::Error)]
pub enum HandlerError {
    #[error("A turn is already running")]
    TurnAlreadyRunning,
    #[error("No active turn")]
    NoActiveTurn,
    #[error("No active session")]
    NoActiveSession,
    #[error("Session not found: {0}")]
    SessionNotFound(String),
    #[error("Unknown command: /{0}")]
    UnknownCommand(String),
    #[error("Cannot compact while a turn is running")]
    CompactBlocked,
    #[error("Compaction skipped: {0}")]
    CompactionSkipped(String),
    #[error(transparent)]
    SessionDirectory(#[from] SessionDirectoryError),
    #[error("{0}")]
    Other(String),
}

/// 协议路由器：把 ClientCommand 转发给对应 SessionActor，自身只保留前台 session id。
pub struct CommandRouter {
    runtime: Arc<ServerRuntime>,
    event_publisher: Arc<ClientEventPublisher>,
    active_session_id: Option<SessionId>,
}

impl CommandRouter {
    pub async fn handle(&mut self, cmd: ClientCommand) -> Result<(), HandlerError> {
        match cmd {
            ClientCommand::CreateSession { working_dir } => {
                self.create_session(working_dir).await?;
            },

            ClientCommand::SubmitPrompt { text, .. } => {
                self.submit_prompt(text).await?;
            },

            ClientCommand::ListSessions => {
                let items: Vec<_> = self
                    .runtime
                    .session_directory
                    .list_summaries()
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|summary| SessionListItem {
                        session_id: summary.session_id.into_string(),
                        created_at: summary.created_at,
                        last_active_at: summary.updated_at,
                        working_dir: summary.working_dir,
                        parent_session_id: summary.parent_session_id.map(SessionId::into_string),
                    })
                    .collect();
                self.event_publisher
                    .publish(ClientNotification::SessionList { sessions: items });
            },

            ClientCommand::Abort => {
                self.abort_active_turn().await?;
            },

            ClientCommand::Compact => {
                self.compact_active_session().await?;
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
                let supervisor = self.runtime.session_supervisor.read().clone();
                if let Some(supervisor) = supervisor {
                    let _ = supervisor.abort(session_id.clone()).await;
                }
                match self.runtime.session_directory.delete(&session_id).await {
                    Ok(()) => {
                        self.runtime
                            .session_bootstrapper
                            .remove_session(&session_id);
                        if self.active_session_id.as_ref() == Some(&session_id) {
                            self.active_session_id = None;
                        }
                        let supervisor = self.runtime.session_supervisor.read().clone();
                        if let Some(supervisor) = supervisor {
                            supervisor.remove(&session_id);
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
                let infos = crate::session::slash::command_infos_for_working_dir(
                    &self.runtime.extension_runner,
                    &working_dir,
                )
                .await;
                self.event_publisher
                    .publish(ClientNotification::ExtensionCommandList { commands: infos });
            },

            ClientCommand::ExecuteExtensionCommand {
                command_name,
                arguments,
            } => {
                let sid = self.ensure_session().await?;
                let combined_text = if arguments.trim().is_empty() {
                    format!("/{command_name}")
                } else {
                    format!("/{command_name} {}", arguments.trim())
                };
                let supervisor = self.session_supervisor()?;
                if let Err(error) = supervisor.submit_input(sid, combined_text).await {
                    self.send_error(
                        crate::session::slash::command_error_code(&error),
                        &error.to_string(),
                    );
                }
            },

            _ => {
                return Err(HandlerError::Other("Not implemented".into()));
            },
        }
        Ok(())
    }

    async fn send_current_state(&mut self) {
        let Some(session_id) = self.active_session_id.clone() else {
            self.send_error(40400, "No active session");
            return;
        };
        match self
            .runtime
            .event_store
            .session_read_model(&session_id)
            .await
        {
            Ok(state) => {
                self.event_publisher
                    .publish(ClientNotification::SessionResumed {
                        session_id: session_id.into_string(),
                        snapshot: session_snapshot(&state),
                    });
            },
            Err(e) => self.send_error(40401, &format!("Session not found: {e}")),
        }
    }

    pub async fn create_session(&mut self, working_dir: String) -> Result<SessionId, HandlerError> {
        tracing::info!(working_dir = %working_dir, "creating session");
        let created = match self.runtime.session_directory.create(&working_dir).await {
            Ok(created) => created,
            Err(error) => {
                tracing::error!(working_dir = %working_dir, error = %error, "create session failed");
                self.send_error(-32603, &error.to_string());
                return Err(error.into());
            },
        };
        let sid = created.session.id().clone();
        self.active_session_id = Some(sid.clone());

        tracing::info!(session_id = %sid, "session created, dispatching SessionStart");
        self.event_publisher
            .publish(ClientNotification::Event(created.start_event));

        match self.initialize_session_prompt(&sid, &working_dir).await {
            Ok(()) => {
                tracing::info!(session_id = %sid, "session fully initialized");
                Ok(sid)
            },
            Err(e) => {
                tracing::error!(session_id = %sid, error = %e, "session prompt init failed");
                self.send_error(-32603, &e);
                Err(HandlerError::Other(e))
            },
        }
    }

    async fn submit_prompt(&mut self, text: String) -> Result<(), HandlerError> {
        let sid = self.ensure_session().await?;
        match self.submit_input_for_session(sid, text).await {
            Ok(_) => Ok(()),
            Err(HandlerError::TurnAlreadyRunning) => Ok(()),
            Err(error) => {
                self.send_error(
                    crate::session::slash::command_error_code(&error),
                    &error.to_string(),
                );
                Err(error)
            },
        }
    }

    pub async fn submit_input_for_session(
        &mut self,
        sid: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HandlerError> {
        self.session_supervisor()?.submit_input(sid, text).await
    }

    async fn abort_active_turn(&mut self) -> Result<(), HandlerError> {
        let Some(sid) = self.active_session_id.clone() else {
            self.send_error(40400, "No active turn");
            return Ok(());
        };
        self.session_supervisor()?.abort(sid).await
    }

    async fn compact_active_session(&mut self) -> Result<(), HandlerError> {
        let Some(sid) = self.active_session_id.clone() else {
            self.send_error(40400, "No active session");
            return Ok(());
        };
        match self.session_supervisor()?.compact(sid).await {
            Ok(ManualCompactOutcome::Compacted { .. }) => Ok(()),
            Ok(ManualCompactOutcome::Skipped { message }) => {
                self.send_error(40000, &message);
                Ok(())
            },
            Err(error) => {
                self.send_error(-32603, &error.to_string());
                Err(error)
            },
        }
    }

    fn session_supervisor(&self) -> Result<Arc<SessionSupervisor>, HandlerError> {
        self.runtime
            .session_supervisor
            .read()
            .clone()
            .ok_or_else(|| HandlerError::Other("session supervisor not bound".into()))
    }

    pub async fn command_infos_for_session(
        &self,
        sid: &SessionId,
    ) -> Result<Vec<astrcode_protocol::events::ExtensionCommandInfo>, HandlerError> {
        let state = self.runtime.session_directory.read_model(sid).await?;
        Ok(crate::session::slash::command_infos_for_working_dir(
            &self.runtime.extension_runner,
            &state.working_dir,
        )
        .await)
    }

    async fn active_session_working_dir(&self) -> Result<String, String> {
        let Some(sid) = self.active_session_id.clone() else {
            return Ok(std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned());
        };
        self.runtime
            .session_directory
            .read_model(&sid)
            .await
            .map(|state| state.working_dir)
            .map_err(|e| format!("read session {sid}: {e}"))
    }

    async fn resume_session(&mut self, session_id: SessionId) {
        match self
            .runtime
            .session_directory
            .open(session_id.clone())
            .await
        {
            Ok(_session) => {
                let supervisor = match self.session_supervisor() {
                    Ok(supervisor) => supervisor,
                    Err(error) => {
                        self.send_error(-32603, &error.to_string());
                        return;
                    },
                };
                if let Err(e) = supervisor.repair(session_id.clone()).await {
                    self.send_error(-32603, &e);
                    return;
                }
                let state = match self.runtime.session_directory.read_model(&session_id).await {
                    Ok(state) => state,
                    Err(e) => {
                        self.send_error(40401, &format!("Session not found: {e}"));
                        return;
                    },
                };
                let working_dir = state.working_dir.clone();
                let needs_prompt = state.system_prompt.is_none();
                let snapshot = session_snapshot(&state);

                let tool_registry = self
                    .runtime
                    .session_bootstrapper
                    .ensure_tool_registry(&session_id, &working_dir)
                    .await;
                if needs_prompt {
                    if let Err(e) = self
                        .configure_session_prompt(&session_id, &working_dir, &tool_registry, None)
                        .await
                    {
                        self.send_error(-32603, &e);
                        return;
                    }
                }
                self.active_session_id = Some(session_id.clone());
                self.event_publisher
                    .publish(ClientNotification::SessionResumed {
                        session_id: session_id.into_string(),
                        snapshot,
                    });
            },
            Err(e) => self.send_error(40401, &format!("Session not found: {e}")),
        }
    }

    async fn ensure_session(&mut self) -> Result<SessionId, HandlerError> {
        if let Some(ref sid) = self.active_session_id {
            return Ok(sid.clone());
        }

        let wd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".into());
        let created = self.runtime.session_directory.create(&wd).await?;
        let sid = created.session.id().clone();
        self.active_session_id = Some(sid.clone());
        self.event_publisher
            .publish(ClientNotification::Event(created.start_event));
        self.initialize_session_prompt(&sid, &wd)
            .await
            .map_err(HandlerError::Other)?;
        Ok(sid)
    }

    async fn initialize_session_prompt(
        &mut self,
        session_id: &SessionId,
        working_dir: &str,
    ) -> Result<(), String> {
        let (_, payload) = self
            .runtime
            .session_bootstrapper
            .initialize_system_prompt(session_id, working_dir, None)
            .await
            .map_err(|error| error.to_string())?;
        self.emit_via_actor(session_id, None, payload).await;
        Ok(())
    }

    async fn configure_session_prompt(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        tool_registry: &ToolRegistry,
        extra_system_prompt: Option<&str>,
    ) -> Result<String, String> {
        let payload = self
            .runtime
            .session_bootstrapper
            .configure_system_prompt(session_id, working_dir, tool_registry, extra_system_prompt)
            .await
            .map_err(|error| error.to_string())?;
        self.emit_via_actor(session_id, None, payload.clone()).await;
        let EventPayload::SystemPromptConfigured { text, .. } = payload else {
            return Err("expected system prompt event".into());
        };
        Ok(text)
    }

    fn send_error(&self, code: i32, message: &str) {
        self.event_publisher.publish(ClientNotification::Error {
            code,
            message: message.into(),
        });
    }

    /// Router 不直接写 durable event；统一通过 SessionActor 持久化和广播。
    async fn emit_via_actor(
        &self,
        session_id: &SessionId,
        turn_id: Option<TurnId>,
        payload: EventPayload,
    ) {
        let supervisor = self.runtime.session_supervisor.read().clone();
        if let Some(supervisor) = supervisor {
            if let Err(error) = supervisor
                .emit_event(session_id.clone(), turn_id, payload)
                .await
            {
                tracing::error!(session_id = %session_id, %error, "failed to forward event to session actor");
            }
        } else {
            tracing::error!(session_id = %session_id, "session supervisor not bound; dropping event");
        }
    }
}

#[cfg(test)]
mod tests;
