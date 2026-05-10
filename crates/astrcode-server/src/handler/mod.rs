//! 命令处理器 — 使用 ServerRuntime 处理客户端命令。
//!
//! 传输层无关：同时被 stdio 二进制和进程内 CLI 使用。
//! 负责将 `ClientCommand` 路由到对应的服务方法，并通过广播通道发送通知。

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use astrcode_core::{
    config::ModelSelection,
    event::{Event, EventPayload, Phase},
    extension::{ExtensionCommandResult, ExtensionError, ExtensionEvent},
    llm::{LlmContent, LlmMessage, LlmRole},
    storage::SessionReadModel,
    tool::ToolResult,
    types::{SessionId, ToolCallId, TurnId, new_message_id, new_turn_id},
};
use astrcode_extensions::context::ServerExtensionContext;
use astrcode_protocol::{
    commands::ClientCommand,
    events::{ClientNotification, SessionListItem},
};
use astrcode_tools::registry::ToolRegistry;
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    agent::{
        AgentError, AgentLoop, AgentServices, AgentSignal, AgentTurnOutput, drive_agent,
        tool_types::BackgroundTaskCompletion,
    },
    bootstrap::{ServerRuntime, build_system_prompt_snapshot, build_tool_registry_snapshot},
};

mod actor;
mod compact;
mod events;
pub(crate) mod snapshot;

pub use actor::CommandHandle;
use actor::CommandMessage;
pub use compact::ManualCompactOutcome;
use events::record_and_broadcast;
#[cfg(test)]
use snapshot::message_to_dto;
use snapshot::session_snapshot;

struct AgentTurnInput {
    sid: SessionId,
    turn_id: TurnId,
    working_dir: String,
    tool_registry: Arc<ToolRegistry>,
    system_prompt: String,
    history: Vec<LlmMessage>,
    text: String,
    transient_instructions: Option<String>,
    actor_tx: mpsc::UnboundedSender<CommandMessage>,
}

struct PendingRequestedToolCall {
    call_id: String,
    tool_name: String,
}

#[derive(Debug)]
pub enum PromptSubmission {
    Accepted { turn_id: TurnId },
    Handled { message: String },
}

/// Structured handler error, replacing ad-hoc string matching.
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
    #[error("{0}")]
    Other(String),
}

struct ParsedSlashCommand {
    name: String,
    arguments: String,
}

fn parse_slash_command(text: &str) -> Option<ParsedSlashCommand> {
    let trimmed = text.trim();
    let body = trimmed.strip_prefix('/')?.trim();
    if body.is_empty() {
        return Some(ParsedSlashCommand {
            name: String::new(),
            arguments: String::new(),
        });
    }

    let (name, arguments) = body
        .split_once(char::is_whitespace)
        .map(|(name, arguments)| (name, arguments.trim()))
        .unwrap_or((body, ""));

    Some(ParsedSlashCommand {
        name: name.to_ascii_lowercase(),
        arguments: arguments.to_string(),
    })
}

fn push_command_info(
    infos: &mut Vec<astrcode_protocol::events::ExtensionCommandInfo>,
    seen: &mut HashSet<String>,
    name: &str,
    description: &str,
    needs_argument: bool,
    source: &str,
) {
    if !seen.insert(name.to_string()) {
        return;
    }
    infos.push(astrcode_protocol::events::ExtensionCommandInfo {
        name: name.into(),
        description: description.into(),
        needs_argument,
        source: source.into(),
    });
}

fn command_source(extension_id: &str) -> &'static str {
    if extension_id == "astrcode-skill" {
        "skill"
    } else {
        "plugin"
    }
}

fn command_error_code(error: &HandlerError) -> i32 {
    match error {
        HandlerError::UnknownCommand(_) => 40402,
        _ => -32603,
    }
}

/// 命令处理器，处理客户端命令并通过广播通道发送通知。
///
/// 维护当前活跃会话和活跃回合的状态，确保同一时间只有一个回合在运行。
pub struct CommandHandler {
    runtime: Arc<ServerRuntime>,
    /// 事件广播发送端，所有客户端通知都通过此通道发送
    event_tx: broadcast::Sender<ClientNotification>,
    /// 当前活跃的会话 ID
    active_session_id: Option<SessionId>,
    /// 每个会话在创建/恢复时固定下来的工具表快照。
    session_tool_registries: HashMap<SessionId, Arc<ToolRegistry>>,
    /// 当前正在执行的回合，按 session 隔离。
    active_turns: HashMap<SessionId, ActiveTurn>,
    actor_tx: mpsc::UnboundedSender<CommandMessage>,
}

/// 正在执行的回合信息，持有对应的 tokio 任务句柄。
struct ActiveTurn {
    session_id: SessionId,
    turn_id: TurnId,
    /// 后台任务的 JoinHandle，可用于取消（abort）回合
    handle: JoinHandle<()>,
    working_dir: String,
    model_id: String,
    system_prompt: String,
    tool_registry: Arc<ToolRegistry>,
    switch_active_on_continuation: bool,
}


impl CommandHandler {
    /// 处理一个客户端命令，将其路由到对应的处理方法。
    ///
    /// 支持的命令包括：创建会话、提交提示词、列出会话、中止回合、
    /// 恢复/切换会话、删除会话等。
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
                    .session_manager
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
                let _ = self
                    .event_tx
                    .send(ClientNotification::SessionList { sessions: items });
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
                // Dispatch SessionShutdown hook before deletion
                {
                    let ext_ctx = ServerExtensionContext::new(
                        session_id.to_string(),
                        String::new(),
                        ModelSelection::simple(self.runtime.read_effective().llm.model_id.clone()),
                    );
                    if let Err(e) = self
                        .runtime
                        .extension_runner
                        .dispatch(ExtensionEvent::SessionShutdown, &ext_ctx)
                        .await
                    {
                        self.send_error(-32603, &e.to_string());
                        return Ok(());
                    }
                }
                match self.runtime.session_manager.delete(&session_id).await {
                    Ok(()) => {
                        // 中止该会话的活跃回合（包括后台任务）
                        if let Some(turn) = self.active_turns.remove(&session_id) {
                            if !turn.handle.is_finished() {
                                turn.handle.abort();
                            }
                        }
                        self.session_tool_registries.remove(&session_id);
                        if self.active_session_id.as_ref() == Some(&session_id) {
                            self.active_session_id = None;
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
                let infos = self.command_infos_for_working_dir(&working_dir).await;
                let _ = self
                    .event_tx
                    .send(ClientNotification::ExtensionCommandList { commands: infos });
            },

            ClientCommand::ExecuteExtensionCommand {
                command_name,
                arguments,
            } => {
                let sid = self.ensure_session().await?;
                let visible_text = if arguments.trim().is_empty() {
                    format!("/{command_name}")
                } else {
                    format!("/{command_name} {}", arguments.trim())
                };
                if let Err(error) = self
                    .execute_slash_command_for_session(
                        sid,
                        ParsedSlashCommand {
                            name: command_name,
                            arguments,
                        },
                        visible_text,
                    )
                    .await
                {
                    self.send_error(command_error_code(&error), &error.to_string());
                }
            },

            _ => {
                return Err(HandlerError::Other("Not implemented".into()));
            },
        }
        Ok(())
    }

    /// 发送当前活跃会话快照，用于客户端初次同步或事件流 lag 后恢复。
    async fn send_current_state(&mut self) {
        let Some(session_id) = self.active_session_id.clone() else {
            self.send_error(40400, "No active session");
            return;
        };
        match self.runtime.session_manager.read_model(&session_id).await {
            Ok(state) => {
                let snapshot = session_snapshot(&state);
                let _ = self.event_tx.send(ClientNotification::SessionResumed {
                    session_id: session_id.into_string(),
                    snapshot,
                });
            },
            Err(e) => self.send_error(40401, &format!("Session not found: {e}")),
        }
    }

    /// 创建新会话，分发 SessionStart 扩展事件，并固定该会话的工具和 system prompt 快照。
    pub async fn create_session(&mut self, working_dir: String) -> Result<SessionId, HandlerError> {
        let model_id = self.runtime.read_effective().llm.model_id.clone();
        tracing::info!(working_dir = %working_dir, model_id = %model_id, "creating session");
        match self
            .runtime
            .session_manager
            .create(&working_dir, &model_id, 2048, None)
            .await
        {
            Ok(event) => {
                self.active_session_id = Some(event.session_id.clone());
                tracing::info!(session_id = %event.session_id, "session created, dispatching SessionStart");
                let _ = self.event_tx.send(ClientNotification::Event(event.clone()));
                let ext_ctx = ServerExtensionContext::new(
                    event.session_id.to_string(),
                    working_dir.clone(),
                    ModelSelection::simple(self.runtime.read_effective().llm.model_id.clone()),
                );
                if let Err(e) = self
                    .runtime
                    .extension_runner
                    .dispatch(ExtensionEvent::SessionStart, &ext_ctx)
                    .await
                {
                    tracing::error!(error = %e, "SessionStart extension dispatch failed");
                    self.send_error(-32603, &e.to_string());
                    return Err(HandlerError::Other(e.to_string()));
                }

                match self
                    .initialize_session_prompt(&event.session_id, &working_dir)
                    .await
                {
                    Ok(_) => {
                        tracing::info!(session_id = %event.session_id, "session fully initialized");
                        Ok(event.session_id)
                    },
                    Err(e) => {
                        tracing::error!(session_id = %event.session_id, error = %e, "session prompt init failed");
                        self.send_error(-32603, &e);
                        Err(HandlerError::Other(e))
                    },
                }
            },
            Err(e) => {
                tracing::error!(working_dir = %working_dir, error = %e, "session_manager.create failed");
                self.send_error(-32603, &e.to_string());
                Err(HandlerError::Other(e.to_string()))
            },
        }
    }

    /// 提交用户提示词，创建回合并在后台启动 Agent 处理。
    ///
    /// 如果已有回合在运行则拒绝（返回 40900 错误）。
    /// 成功提交后，回合在独立的 tokio 任务中异步执行。
    async fn submit_prompt(&mut self, text: String) -> Result<(), HandlerError> {
        let sid = self.ensure_session().await?;
        match self.submit_input_for_session(sid, text).await {
            Ok(_) => Ok(()),
            Err(HandlerError::TurnAlreadyRunning) => Ok(()),
            Err(error) => {
                self.send_error(command_error_code(&error), &error.to_string());
                Err(error)
            },
        }
    }

    /// 向指定会话提交用户输入。斜杠命令在这里被后端统一拦截和派发。
    pub async fn submit_input_for_session(
        &mut self,
        sid: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HandlerError> {
        if let Some(command) = parse_slash_command(&text) {
            return self
                .execute_slash_command_for_session(sid, command, text)
                .await;
        }

        self.submit_prompt_for_session(sid, text)
            .await
            .map(|turn_id| PromptSubmission::Accepted { turn_id })
    }

    /// 向指定会话提交用户提示词。
    ///
    /// HTTP 调用必须走这个显式 session 入口；stdio 的 active session 只是一层
    /// convenience adapter。
    pub async fn submit_prompt_for_session(
        &mut self,
        sid: SessionId,
        text: String,
    ) -> Result<TurnId, HandlerError> {
        self.start_turn_for_session(sid, text.clone(), text, None)
            .await
    }

    async fn start_turn_for_session(
        &mut self,
        sid: SessionId,
        visible_text: String,
        user_text: String,
        transient_instructions: Option<String>,
    ) -> Result<TurnId, HandlerError> {
        tracing::info!(session_id = %sid, text_len = user_text.len(), "submit_prompt_for_session");
        if self.active_turns.contains_key(&sid) {
            self.send_error(40900, "A turn is already running");
            return Err(HandlerError::TurnAlreadyRunning);
        }

        self.runtime
            .session_manager
            .resume(&sid)
            .await
            .map_err(|e| HandlerError::SessionNotFound(format!("Session {sid} not found: {e}")))?;
        self.repair_stale_pending_tool_calls(&sid)
            .await
            .map_err(HandlerError::Other)?;
        let state = self
            .runtime
            .session_manager
            .read_model(&sid)
            .await
            .map_err(|e| HandlerError::Other(format!("read session {sid}: {e}")))?;
        let history = state.provider_messages();
        let working_dir = state.working_dir;
        let model_id = state.model_id;
        let system_prompt = state.system_prompt;
        let tool_registry = self.ensure_tool_registry(&sid, &working_dir).await;
        let system_prompt = match system_prompt {
            Some(system_prompt) => system_prompt,
            None => self
                .configure_session_prompt(&sid, &working_dir, &tool_registry, None)
                .await
                .map_err(HandlerError::Other)?,
        };
        let turn_id = new_turn_id();

        self.record_and_broadcast(&sid, Some(&turn_id), EventPayload::TurnStarted)
            .await
            .map_err(HandlerError::Other)?;
        self.record_and_broadcast(
            &sid,
            Some(&turn_id),
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: visible_text,
            },
        )
        .await
        .map_err(HandlerError::Other)?;
        self.record_and_broadcast(&sid, Some(&turn_id), EventPayload::AgentRunStarted)
            .await
            .map_err(HandlerError::Other)?;

        let switch_active_on_continuation = self.active_session_id.as_ref() == Some(&sid);
        let handle = self.spawn_agent_turn(AgentTurnInput {
            sid: sid.clone(),
            turn_id: turn_id.clone(),
            working_dir: working_dir.clone(),
            tool_registry: Arc::clone(&tool_registry),
            system_prompt: system_prompt.clone(),
            history,
            text: user_text,
            transient_instructions,
            actor_tx: self.actor_tx.clone(),
        });
        self.active_turns.insert(
            sid.clone(),
            ActiveTurn {
                session_id: sid,
                turn_id: turn_id.clone(),
                handle,
                working_dir,
                model_id,
                system_prompt,
                tool_registry,
                switch_active_on_continuation,
            },
        );
        Ok(turn_id)
    }

    async fn execute_slash_command_for_session(
        &mut self,
        sid: SessionId,
        command: ParsedSlashCommand,
        visible_text: String,
    ) -> Result<PromptSubmission, HandlerError> {
        if command.name == "compact" {
            return match self.compact_session(&sid).await? {
                ManualCompactOutcome::Created { .. } => Ok(PromptSubmission::Handled {
                    message: "compact accepted".into(),
                }),
                ManualCompactOutcome::Skipped { message } => {
                    Ok(PromptSubmission::Handled { message })
                },
            };
        }

        let state = self
            .runtime
            .session_manager
            .read_model(&sid)
            .await
            .map_err(|e| HandlerError::Other(format!("read session {sid}: {e}")))?;
        let ext_ctx = ServerExtensionContext::new(
            sid.to_string(),
            state.working_dir.clone(),
            ModelSelection::simple(self.runtime.read_effective().llm.model_id.clone()),
        );

        match self
            .runtime
            .extension_runner
            .dispatch_command(
                &command.name,
                &command.arguments,
                &state.working_dir,
                &ext_ctx,
            )
            .await
        {
            Ok(ExtensionCommandResult::Display { content, is_error }) => {
                let _ = self
                    .event_tx
                    .send(ClientNotification::ExtensionCommandResult {
                        command_name: command.name,
                        content,
                        is_error,
                    });
                Ok(PromptSubmission::Handled {
                    message: "command handled".into(),
                })
            },
            Ok(ExtensionCommandResult::Handled { message }) => {
                Ok(PromptSubmission::Handled { message })
            },
            Ok(ExtensionCommandResult::StartTurn { instructions }) => self
                .start_turn_for_session(sid, visible_text.clone(), visible_text, Some(instructions))
                .await
                .map(|turn_id| PromptSubmission::Accepted { turn_id }),
            Err(ExtensionError::NotFound(name)) => Err(HandlerError::UnknownCommand(
                name.trim_start_matches('/').to_string(),
            )),
            Err(error) => Err(HandlerError::Other(format!("Command error: {error}"))),
        }
    }

    pub async fn command_infos_for_session(
        &self,
        sid: &SessionId,
    ) -> Result<Vec<astrcode_protocol::events::ExtensionCommandInfo>, HandlerError> {
        let state = self
            .runtime
            .session_manager
            .read_model(sid)
            .await
            .map_err(|e| HandlerError::Other(format!("read session {sid}: {e}")))?;
        Ok(self.command_infos_for_working_dir(&state.working_dir).await)
    }

    async fn command_infos_for_working_dir(
        &self,
        working_dir: &str,
    ) -> Vec<astrcode_protocol::events::ExtensionCommandInfo> {
        let mut infos = Vec::new();
        let mut seen = HashSet::new();

        push_command_info(
            &mut infos,
            &mut seen,
            "compact",
            "Compact the current session context",
            false,
            "builtin",
        );

        let mut extension_commands = self
            .runtime
            .extension_runner
            .collect_commands_for(working_dir)
            .await;
        extension_commands.sort_by_key(|registered| {
            match command_source(&registered.extension_id) {
                "plugin" => 0,
                "skill" => 1,
                _ => 2,
            }
        });

        for registered in extension_commands {
            let source = command_source(&registered.extension_id);
            if !seen.insert(registered.command.name.clone()) {
                tracing::warn!(
                    command = %registered.command.name,
                    source,
                    extension_id = %registered.extension_id,
                    "slash command ignored because a higher priority command already exists"
                );
                continue;
            }
            infos.push(astrcode_protocol::events::ExtensionCommandInfo {
                name: registered.command.name,
                description: registered.command.description,
                needs_argument: registered.command.args_schema.is_some(),
                source: source.into(),
            });
        }

        infos
    }

    async fn active_session_working_dir(&self) -> Result<String, String> {
        let Some(sid) = self.active_session_id.clone() else {
            return Ok(std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned());
        };
        self.runtime
            .session_manager
            .read_model(&sid)
            .await
            .map(|state| state.working_dir)
            .map_err(|e| format!("read session {sid}: {e}"))
    }

    /// 中止当前活跃的回合，取消后台任务并记录完成事件。
    async fn abort_active_turn(&mut self) -> Result<(), HandlerError> {
        let Some(sid) = self.active_session_id.clone() else {
            self.send_error(40400, "No active turn");
            return Ok(());
        };
        self.abort_session(&sid).await
    }

    /// 中止指定会话的活跃回合。
    pub async fn abort_session(&mut self, session_id: &SessionId) -> Result<(), HandlerError> {
        let Some(active_turn) = self.active_turns.remove(session_id) else {
            self.send_error(40400, "No active turn");
            return Err(HandlerError::NoActiveTurn);
        };

        // 扩展的TurnAborted事件
        let ext_ctx = ServerExtensionContext::new(
            active_turn.session_id.to_string(),
            active_turn.working_dir.clone(),
            ModelSelection::simple(active_turn.model_id.clone()),
        );
        if let Err(e) = self
            .runtime
            .extension_runner
            .dispatch(ExtensionEvent::TurnAborted, &ext_ctx)
            .await
        {
            tracing::warn!(error = %e, "TurnAborted extension dispatch failed");
        }

        if !active_turn.handle.is_finished() {
            active_turn.handle.abort();
        }

        record_and_broadcast(
            &self.runtime,
            &self.event_tx,
            &active_turn.session_id,
            Some(&active_turn.turn_id),
            EventPayload::TurnCompleted {
                finish_reason: "aborted".into(),
            },
        )
        .await
        .map_err(HandlerError::Other)?;
        record_and_broadcast(
            &self.runtime,
            &self.event_tx,
            &active_turn.session_id,
            Some(&active_turn.turn_id),
            EventPayload::AgentRunCompleted {
                reason: "aborted".into(),
            },
        )
        .await
        .map_err(HandlerError::Other)?;
        Ok(())
    }

    /// 恢复或切换到指定会话，从磁盘重放事件并构建快照发送给客户端。
    async fn resume_session(&mut self, session_id: SessionId) {
        match self.runtime.session_manager.resume(&session_id).await {
            Ok(_) => {
                if let Err(e) = self.repair_stale_pending_tool_calls(&session_id).await {
                    self.send_error(-32603, &e);
                    return;
                }
                let state = match self.runtime.session_manager.read_model(&session_id).await {
                    Ok(state) => state,
                    Err(e) => {
                        self.send_error(40401, &format!("Session not found: {e}"));
                        return;
                    },
                };
                let working_dir = state.working_dir.clone();
                let needs_prompt = state.system_prompt.is_none();
                let snapshot = session_snapshot(&state);

                let tool_registry = self.ensure_tool_registry(&session_id, &working_dir).await;
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
                let _ = self.event_tx.send(ClientNotification::SessionResumed {
                    session_id: session_id.into_string(),
                    snapshot,
                });
            },
            Err(e) => self.send_error(40401, &format!("Session not found: {e}")),
        }
    }

    /// 确保存在活跃会话，如果没有则自动创建一个。
    /// 使用当前工作目录作为新会话的工作目录。
    async fn ensure_session(&mut self) -> Result<SessionId, HandlerError> {
        if let Some(ref sid) = self.active_session_id {
            return Ok(sid.clone());
        }

        let model_id = self.runtime.read_effective().llm.model_id.clone();
        let wd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".into());
        let event = self
            .runtime
            .session_manager
            .create(&wd, &model_id, 2048, None)
            .await
            .map_err(|e| HandlerError::Other(format!("create session: {e}")))?;

        let sid = event.session_id.clone();
        self.active_session_id = Some(sid.clone());
        let _ = self.event_tx.send(ClientNotification::Event(event));
        let ext_ctx = ServerExtensionContext::new(
            sid.to_string(),
            wd.clone(),
            ModelSelection::simple(self.runtime.read_effective().llm.model_id.clone()),
        );
        self.runtime
            .extension_runner
            .dispatch(ExtensionEvent::SessionStart, &ext_ctx)
            .await
            .map_err(|e| HandlerError::Other(e.to_string()))?;
        self.initialize_session_prompt(&sid, &wd)
            .await
            .map_err(HandlerError::Other)?;
        Ok(sid)
    }

    async fn initialize_session_prompt(
        &mut self,
        session_id: &SessionId,
        working_dir: &str,
    ) -> Result<String, String> {
        let tool_registry = self.refresh_tool_registry(session_id, working_dir).await;
        self.configure_session_prompt(session_id, working_dir, &tool_registry, None)
            .await
    }

    async fn ensure_tool_registry(
        &mut self,
        session_id: &SessionId,
        working_dir: &str,
    ) -> Arc<ToolRegistry> {
        if let Some(tool_registry) = self.session_tool_registries.get(session_id) {
            return Arc::clone(tool_registry);
        }

        self.refresh_tool_registry(session_id, working_dir).await
    }

    async fn refresh_tool_registry(
        &mut self,
        session_id: &SessionId,
        working_dir: &str,
    ) -> Arc<ToolRegistry> {
        let tool_registry = self.build_tool_registry_for(working_dir).await;
        self.session_tool_registries
            .insert(session_id.clone(), Arc::clone(&tool_registry));
        tool_registry
    }

    async fn build_tool_registry_for(&self, working_dir: &str) -> Arc<ToolRegistry> {
        let timeout = self.runtime.read_effective().llm.read_timeout_secs;
        build_tool_registry_snapshot(&self.runtime.extension_runner, working_dir, timeout).await
    }

    async fn configure_session_prompt(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        tool_registry: &ToolRegistry,
        extra_system_prompt: Option<&str>,
    ) -> Result<String, String> {
        let tools = tool_registry.list_definitions();
        let model_id = self.runtime.read_effective().llm.model_id.clone();
        let (system_prompt, fingerprint) = build_system_prompt_snapshot(
            &self.runtime.extension_runner,
            session_id.as_str(),
            working_dir,
            &model_id,
            &tools,
            extra_system_prompt,
        )
        .await
        .map_err(|e| e.to_string())?;

        self.record_and_broadcast(
            session_id,
            None,
            EventPayload::SystemPromptConfigured {
                text: system_prompt.clone(),
                fingerprint,
            },
        )
        .await?;
        Ok(system_prompt)
    }

    /// 在后台 tokio 任务中启动 Agent 回合处理。
    ///
    /// 使用 `tokio::select!` 同时等待 Agent 完成和事件流，
    /// 确保事件实时广播给客户端。Agent 完成后发送回合完成事件。
    fn spawn_agent_turn(&self, input: AgentTurnInput) -> JoinHandle<()> {
        let runtime = self.runtime.clone();

        tokio::spawn(async move {
            let AgentTurnInput {
                sid,
                turn_id,
                working_dir,
                tool_registry,
                system_prompt,
                history,
                text,
                transient_instructions,
                actor_tx,
            } = input;
            let current_session_id = Arc::new(tokio::sync::Mutex::new(sid.clone()));

            let (background_result_tx, mut background_result_rx) =
                mpsc::unbounded_channel::<BackgroundTaskCompletion>();

            let bg_actor_tx = actor_tx.clone();
            tokio::spawn(async move {
                while let Some(completion) = background_result_rx.recv().await {
                    let _ = bg_actor_tx.send(CommandMessage::BackgroundTaskCompleted {
                        session_id: completion.session_id,
                        task_id: completion.task_id,
                        call_id: completion.call_id,
                        tool_name: completion.tool_name,
                        result: completion.result,
                    });
                }
            });

            let model_id = runtime.read_effective().llm.model_id.clone();
            let system_prompt = transient_instructions
                .filter(|instructions| !instructions.trim().is_empty())
                .map(|instructions| {
                    format!(
                        "{system_prompt}\n\n[Slash Command Instructions]\n{}",
                        instructions.trim()
                    )
                })
                .unwrap_or(system_prompt);

            let agent = AgentLoop::new(
                sid.clone(),
                working_dir,
                system_prompt,
                model_id,
                AgentServices {
                    llm: runtime.llm_provider.clone(),
                    tool_registry,
                    extension_runner: runtime.extension_runner.clone(),
                    context_assembler: runtime.context_assembler.clone(),
                    session_manager: runtime.session_manager.clone(),
                    auto_compact_failures: runtime.auto_compact_failures.clone(),
                    background_result_tx: Some(background_result_tx),
                    background_tasks: Default::default(),
                },
            );

            let (output, emitted_error) = drive_agent(&agent, &text, history, |signal| {
                let actor_tx = actor_tx.clone();
                let current_session_id = Arc::clone(&current_session_id);
                let turn_id = turn_id.clone();
                async move {
                    match signal {
                        AgentSignal::Event(payload) => {
                            let session_id = current_session_id.lock().await.clone();
                            let _ = actor_tx.send(CommandMessage::AgentEvent {
                                session_id,
                                turn_id,
                                payload,
                            });
                        },
                        AgentSignal::AutoCompact {
                            trigger,
                            compaction,
                            reply,
                        } => {
                            let session_id = current_session_id.lock().await.clone();
                            let (actor_reply, actor_rx) = oneshot::channel();
                            let result: Result<SessionId, HandlerError> = if actor_tx
                                .send(CommandMessage::AgentAutoCompact {
                                    session_id,
                                    turn_id,
                                    trigger,
                                    compaction,
                                    reply: actor_reply,
                                })
                                .is_err()
                            {
                                Err(HandlerError::Other("command actor is unavailable".into()))
                            } else {
                                match actor_rx.await {
                                    Ok(result) => result,
                                    Err(_) => Err(HandlerError::Other(
                                        "command actor dropped auto compact response".into(),
                                    )),
                                }
                            };
                            if let Ok(child_session_id) = &result {
                                *current_session_id.lock().await = child_session_id.clone();
                            }
                            let _ = reply.send(result.map_err(|e| e.to_string()));
                        },
                    }
                }
            })
            .await;
            let final_session_id = current_session_id.lock().await.clone();

            match output {
                Ok(output) => {
                    let _ = actor_tx.send(CommandMessage::AgentTurnFinished {
                        session_id: final_session_id,
                        turn_id,
                        output,
                    });
                },
                Err(error) => {
                    let _ = actor_tx.send(CommandMessage::AgentTurnFailed {
                        session_id: final_session_id,
                        turn_id,
                        error,
                        emitted_error,
                    });
                },
            }
        })
    }

    /// 记录事件到存储并广播给客户端的便捷方法。
    async fn record_and_broadcast(
        &self,
        session_id: &SessionId,
        turn_id: Option<&TurnId>,
        payload: EventPayload,
    ) -> Result<Event, String> {
        record_and_broadcast(&self.runtime, &self.event_tx, session_id, turn_id, payload).await
    }

    fn active_turn_matches(&self, session_id: &SessionId, turn_id: &TurnId) -> bool {
        self.active_turns
            .get(session_id)
            .is_some_and(|active_turn| &active_turn.turn_id == turn_id)
    }

    async fn repair_stale_pending_tool_calls(&self, session_id: &SessionId) -> Result<(), String> {
        if self.active_turns.contains_key(session_id) {
            return Ok(());
        }

        let state = self
            .runtime
            .session_manager
            .read_model(session_id)
            .await
            .map_err(|e| format!("read session {session_id}: {e}"))?;
        if state.phase != Phase::CallingTool || state.pending_tool_calls.is_empty() {
            return Ok(());
        }

        for pending in pending_requested_tool_calls(&state) {
            self.record_and_broadcast(
                session_id,
                None,
                EventPayload::ToolCallCompleted {
                    call_id: pending.call_id.clone().into(),
                    tool_name: pending.tool_name,
                    result: interrupted_tool_result(&pending.call_id),
                },
            )
            .await?;
        }
        self.record_and_broadcast(
            session_id,
            None,
            EventPayload::TurnCompleted {
                finish_reason: "interrupted".into(),
            },
        )
        .await?;
        self.record_and_broadcast(
            session_id,
            None,
            EventPayload::AgentRunCompleted {
                reason: "interrupted".into(),
            },
        )
        .await?;
        Ok(())
    }

    async fn finish_agent_turn(
        &mut self,
        session_id: SessionId,
        turn_id: TurnId,
        output: AgentTurnOutput,
    ) {
        if !self.active_turn_matches(&session_id, &turn_id) {
            return;
        }
        self.active_turns.remove(&session_id);
        let _ = self
            .record_and_broadcast(
                &session_id,
                Some(&turn_id),
                EventPayload::TurnCompleted {
                    finish_reason: output.finish_reason.clone(),
                },
            )
            .await;
        let _ = self
            .record_and_broadcast(
                &session_id,
                Some(&turn_id),
                EventPayload::AgentRunCompleted {
                    reason: output.finish_reason,
                },
            )
            .await;
    }

    async fn fail_agent_turn(
        &mut self,
        session_id: SessionId,
        turn_id: TurnId,
        error: AgentError,
        emitted_error: bool,
    ) {
        if !self.active_turn_matches(&session_id, &turn_id) {
            return;
        }
        self.active_turns.remove(&session_id);
        if !emitted_error {
            let _ = self
                .record_and_broadcast(
                    &session_id,
                    Some(&turn_id),
                    EventPayload::ErrorOccurred {
                        code: -32603,
                        message: error.to_string(),
                        recoverable: false,
                    },
                )
                .await;
        }
        let _ = self
            .record_and_broadcast(
                &session_id,
                Some(&turn_id),
                EventPayload::TurnCompleted {
                    finish_reason: "error".into(),
                },
            )
            .await;
        let _ = self
            .record_and_broadcast(
                &session_id,
                Some(&turn_id),
                EventPayload::AgentRunCompleted {
                    reason: "error".into(),
                },
            )
            .await;
    }

    /// 通过广播通道发送错误通知给客户端。
    fn send_error(&self, code: i32, message: &str) {
        let _ = self.event_tx.send(ClientNotification::Error {
            code,
            message: message.into(),
        });
    }
}

fn pending_requested_tool_calls(state: &SessionReadModel) -> Vec<PendingRequestedToolCall> {
    let mut remaining = state.pending_tool_calls.clone();
    let mut pending = Vec::new();

    for message in &state.messages {
        if message.role != LlmRole::Assistant {
            continue;
        }
        for content in &message.content {
            let LlmContent::ToolCall { call_id, name, .. } = content else {
                continue;
            };
            if remaining.remove(&ToolCallId::from(call_id.clone())) {
                pending.push(PendingRequestedToolCall {
                    call_id: call_id.clone(),
                    tool_name: name.clone(),
                });
            }
        }
    }

    pending
}

fn interrupted_tool_result(call_id: &str) -> ToolResult {
    let content = "Tool execution interrupted before completion".to_string();
    ToolResult {
        call_id: call_id.to_string(),
        content: content.clone(),
        is_error: true,
        error: Some(content),
        metadata: Default::default(),
        duration_ms: None,
    }
}

#[cfg(test)]
mod tests;
