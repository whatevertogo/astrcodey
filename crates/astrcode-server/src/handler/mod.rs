//! 命令处理器 — 使用 ServerRuntime 处理客户端命令。
//!
//! 传输层无关：同时被 stdio 二进制和进程内 CLI 使用。
//! 负责将 `ClientCommand` 路由到对应的服务方法，并通过广播通道发送通知。

use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    config::ModelSelection,
    event::{Event, EventPayload},
    extension::ExtensionEvent,
    types::*,
};
use astrcode_protocol::{
    commands::ClientCommand,
    events::{ClientNotification, SessionListItem},
};
use astrcode_session::Session;
use astrcode_tools::registry::ToolRegistry;
use tokio::sync::{broadcast, mpsc};

use crate::bootstrap::{
    PromptFiles, ServerRuntime, SystemPromptSnapshotInput, build_system_prompt_snapshot_with_files,
    build_tool_registry_snapshot, load_system_prompt_files,
};

mod actor;
mod compact;
pub(crate) mod slash;
pub(crate) mod snapshot;
pub(in crate::handler) mod turn;

pub use actor::CommandHandle;
use actor::CommandMessage;
pub use compact::ManualCompactOutcome;
#[cfg(test)]
use snapshot::message_to_dto;
use snapshot::session_snapshot;
use turn::ActiveTurn;

/// 用户输入提交结果：被接受进入 Turn，或被斜杠命令处理。
#[derive(Debug)]
pub enum PromptSubmission {
    Accepted { turn_id: TurnId },
    Handled { message: String },
}

/// Handler 错误类型，替代原来的字符串匹配。
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

pub(crate) use turn::TurnCompletion;

/// 命令处理器，处理客户端命令并通过广播通道发送通知。
///
/// 维护当前活跃会话和活跃回合的状态，确保同一时间只有一个回合在运行。
pub struct CommandHandler {
    runtime: Arc<ServerRuntime>,
    /// 事件广播发送端，所有客户端通知都通过此通道发送
    event_tx: broadcast::Sender<ClientNotification>,
    /// 当前活跃的会话 ID
    active_session_id: Option<SessionId>,
    /// 每个会话创建时固定的工具表快照，避免运行时工具变化影响会话
    session_tool_registries: HashMap<SessionId, Arc<ToolRegistry>>,
    /// 当前正在执行的回合，按 session 隔离
    active_turns: HashMap<SessionId, ActiveTurn>,
    /// Actor 消息通道发送端，用于在后台任务中发送消息回 Handler
    actor_tx: mpsc::UnboundedSender<CommandMessage>,
}

impl CommandHandler {
    // ─── 命令路由 ────────────────────────────────────────────────────────

    /// 处理客户端命令，路由到对应处理方法。
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
                    .event_store
                    .list_session_summaries()
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
                {
                    let lifecycle_ctx = astrcode_core::extension::LifecycleContext {
                        session_id: session_id.to_string(),
                        working_dir: String::new(),
                        model: ModelSelection::simple(
                            self.runtime.config.read_effective().llm.model_id.clone(),
                        ),
                    };
                    if let Err(e) = self
                        .runtime
                        .extension_runner
                        .emit_lifecycle(ExtensionEvent::SessionShutdown, lifecycle_ctx)
                        .await
                    {
                        self.send_error(-32603, &e.to_string());
                        return Ok(());
                    }
                }
                match self.runtime.event_store.delete_session(&session_id).await {
                    Ok(()) => {
                        // 中止该会话的活跃回合并清理资源
                        if let Some(mut turn) = self.active_turns.remove(&session_id) {
                            if !turn.handle.is_finished() {
                                turn.handle.abort();
                            }
                            turn.resolve_completion(turn::TurnCompletion::Aborted);
                        }
                        self.cleanup_background_tasks_for_session(&session_id);
                        self.runtime.remove_file_observation_store(&session_id);
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
                        slash::ParsedSlashCommand {
                            name: command_name,
                            arguments,
                        },
                        visible_text,
                    )
                    .await
                {
                    self.send_error(slash::command_error_code(&error), &error.to_string());
                }
            },

            _ => {
                return Err(HandlerError::Other("Not implemented".into()));
            },
        }
        Ok(())
    }

    // ─── 会话生命周期 ──────────────────────────────────────────────────

    /// 发送当前会话快照，用于客户端初次同步或恢复。
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
                let snapshot = session_snapshot(&state);
                let _ = self.event_tx.send(ClientNotification::SessionResumed {
                    session_id: session_id.into_string(),
                    snapshot,
                });
            },
            Err(e) => self.send_error(40401, &format!("Session not found: {e}")),
        }
    }

    /// 创建新会话，分发 SessionStart 事件，初始化工具表和 system prompt。
    pub async fn create_session(&mut self, working_dir: String) -> Result<SessionId, HandlerError> {
        let model_id = self.runtime.config.read_effective().llm.model_id.clone();
        tracing::info!(working_dir = %working_dir, model_id = %model_id, "creating session");
        let session = Session::create(
            self.runtime.event_store.clone(),
            &working_dir,
            &model_id,
            None,
        )
        .await
        .map_err(|e| {
            tracing::error!(working_dir = %working_dir, error = %e, "Session::create failed");
            HandlerError::Other(e.to_string())
        })?;

        let sid = session.id().clone();
        self.active_session_id = Some(sid.clone());

        // 读回 SessionStarted 事件用于广播
        let start_event = self
            .runtime
            .event_store
            .replay_events(&sid)
            .await
            .map_err(|e| HandlerError::Other(e.to_string()))?
            .into_iter()
            .next()
            .ok_or_else(|| HandlerError::Other("session created but no events found".into()))?;

        tracing::info!(session_id = %sid, "session created, dispatching SessionStart");
        let _ = self.event_tx.send(ClientNotification::Event(start_event));

        let lifecycle_ctx = astrcode_core::extension::LifecycleContext {
            session_id: sid.to_string(),
            working_dir: working_dir.clone(),
            model: ModelSelection::simple(
                self.runtime.config.read_effective().llm.model_id.clone(),
            ),
        };
        if let Err(e) = self
            .runtime
            .extension_runner
            .emit_lifecycle(ExtensionEvent::SessionStart, lifecycle_ctx)
            .await
        {
            tracing::error!(error = %e, "SessionStart extension dispatch failed");
            self.send_error(-32603, &e.to_string());
            return Err(HandlerError::Other(e.to_string()));
        }

        match self.initialize_session_prompt(&sid, &working_dir).await {
            Ok(_) => {
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

    /// 提交用户输入，如有已有 Turn 运行则静默忽略（返回 OK）。
    async fn submit_prompt(&mut self, text: String) -> Result<(), HandlerError> {
        let sid = self.ensure_session().await?;
        match self.submit_input_for_session(sid, text).await {
            Ok(_) => Ok(()),
            Err(HandlerError::TurnAlreadyRunning) => Ok(()),
            Err(error) => {
                self.send_error(slash::command_error_code(&error), &error.to_string());
                Err(error)
            },
        }
    }

    /// 向指定会话提交输入。斜杠命令在此被拦截并派发，普通输入启动新 Turn。
    pub async fn submit_input_for_session(
        &mut self,
        sid: SessionId,
        text: String,
    ) -> Result<PromptSubmission, HandlerError> {
        if let Some(command) = slash::parse_slash_command(&text) {
            return self
                .execute_slash_command_for_session(sid, command, text)
                .await;
        }

        self.start_turn_for_session(sid, text.clone(), text, None, None)
            .await
            .map(|turn_id| PromptSubmission::Accepted { turn_id })
    }

    /// 获取指定会话的可用命令列表。
    pub async fn command_infos_for_session(
        &self,
        sid: &SessionId,
    ) -> Result<Vec<astrcode_protocol::events::ExtensionCommandInfo>, HandlerError> {
        let state = self
            .runtime
            .event_store
            .session_read_model(sid)
            .await
            .map_err(|e| HandlerError::Other(format!("read session {sid}: {e}")))?;
        Ok(self.command_infos_for_working_dir(&state.working_dir).await)
    }

    /// 获取当前会话的工作目录，无活跃会话则返回当前目录。
    async fn active_session_working_dir(&self) -> Result<String, String> {
        let Some(sid) = self.active_session_id.clone() else {
            return Ok(std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned());
        };
        self.runtime
            .event_store
            .session_read_model(&sid)
            .await
            .map(|state| state.working_dir)
            .map_err(|e| format!("read session {sid}: {e}"))
    }

    /// 恢复或切换到指定会话，修复可能的遗留状态后发送快照。
    async fn resume_session(&mut self, session_id: SessionId) {
        let store = self.runtime.event_store.clone();
        match Session::open(store, session_id.clone()).await {
            Ok(_session) => {
                if let Err(e) = self.repair_stale_pending_tool_calls(&session_id).await {
                    self.send_error(-32603, &e);
                    return;
                }
                let state = match self
                    .runtime
                    .event_store
                    .session_read_model(&session_id)
                    .await
                {
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

    // ─── 工具表与提示词配置 ────────────────────────────────────────────

    /// 确保存在活跃会话，无则自动创建。
    async fn ensure_session(&mut self) -> Result<SessionId, HandlerError> {
        if let Some(ref sid) = self.active_session_id {
            return Ok(sid.clone());
        }

        let model_id = self.runtime.config.read_effective().llm.model_id.clone();
        let wd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".into());
        let session = Session::create(self.runtime.event_store.clone(), &wd, &model_id, None)
            .await
            .map_err(|e| HandlerError::Other(format!("create session: {e}")))?;

        let sid = session.id().clone();
        self.active_session_id = Some(sid.clone());

        let start_event = self
            .runtime
            .event_store
            .replay_events(&sid)
            .await
            .map_err(|e| HandlerError::Other(e.to_string()))?
            .into_iter()
            .next()
            .ok_or_else(|| HandlerError::Other("session created but no events found".into()))?;
        let _ = self.event_tx.send(ClientNotification::Event(start_event));

        let lifecycle_ctx = astrcode_core::extension::LifecycleContext {
            session_id: sid.to_string(),
            working_dir: wd.clone(),
            model: ModelSelection::simple(
                self.runtime.config.read_effective().llm.model_id.clone(),
            ),
        };
        self.runtime
            .extension_runner
            .emit_lifecycle(ExtensionEvent::SessionStart, lifecycle_ctx)
            .await
            .map_err(|e| HandlerError::Other(e.to_string()))?;
        self.initialize_session_prompt(&sid, &wd)
            .await
            .map_err(HandlerError::Other)?;
        Ok(sid)
    }

    /// 初始化会话的 system prompt：加载工具表和提示词文件，生成最终 prompt。
    async fn initialize_session_prompt(
        &mut self,
        session_id: &SessionId,
        working_dir: &str,
    ) -> Result<String, String> {
        let timeout = self.runtime.config.read_effective().llm.read_timeout_secs;
        let registry_fut =
            build_tool_registry_snapshot(&self.runtime.extension_runner, working_dir, timeout);
        let prompt_files_fut = load_system_prompt_files(working_dir);
        let (tool_registry, prompt_files) = tokio::join!(registry_fut, prompt_files_fut);
        self.session_tool_registries
            .insert(session_id.clone(), Arc::clone(&tool_registry));
        self.configure_session_prompt_with_files(
            session_id,
            working_dir,
            &tool_registry,
            None,
            prompt_files,
        )
        .await
    }

    /// 获取会话的工具表，不存在则刷新。
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

    /// 刷新并缓存会话的工具表。
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

    /// 为指定工作目录构建工具表快照。
    async fn build_tool_registry_for(&self, working_dir: &str) -> Arc<ToolRegistry> {
        let timeout = self.runtime.config.read_effective().llm.read_timeout_secs;
        build_tool_registry_snapshot(&self.runtime.extension_runner, working_dir, timeout).await
    }

    /// 配置会话的 system prompt，包含工具描述和额外提示。
    async fn configure_session_prompt(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        tool_registry: &ToolRegistry,
        extra_system_prompt: Option<&str>,
    ) -> Result<String, String> {
        let prompt_files = load_system_prompt_files(working_dir).await;
        self.configure_session_prompt_with_files(
            session_id,
            working_dir,
            tool_registry,
            extra_system_prompt,
            prompt_files,
        )
        .await
    }

    /// 使用已加载的提示词文件配置 system prompt。
    async fn configure_session_prompt_with_files(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        tool_registry: &ToolRegistry,
        extra_system_prompt: Option<&str>,
        prompt_files: PromptFiles,
    ) -> Result<String, String> {
        let tools_with_meta = tool_registry.list_definitions_with_prompt_metadata();
        let tools: Vec<_> = tools_with_meta.iter().map(|(def, _)| def.clone()).collect();
        let tool_prompt_metadata = tools_with_meta
            .into_iter()
            .filter_map(|(def, meta)| meta.map(|m| (def.name, m)))
            .collect();
        let model_id = self.runtime.config.read_effective().llm.model_id.clone();
        let (system_prompt, fingerprint) =
            build_system_prompt_snapshot_with_files(SystemPromptSnapshotInput {
                extension_runner: &self.runtime.extension_runner,
                session_id: session_id.as_str(),
                working_dir,
                model_id: &model_id,
                tools: &tools,
                extra_system_prompt,
                tool_prompt_metadata,
                prompt_files,
            })
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

    // ─── 事件记录与内部辅助 ──────────────────────────────────────────

    /// 记录事件并广播给客户端。
    async fn record_and_broadcast(
        &self,
        session_id: &SessionId,
        turn_id: Option<&TurnId>,
        payload: EventPayload,
    ) -> Result<Event, String> {
        let event = Event::new(session_id.clone(), turn_id.cloned(), payload);
        let event = if event.payload.is_durable() {
            self.runtime
                .event_store
                .append_event(event)
                .await
                .map_err(|e| e.to_string())?
        } else {
            event
        };
        let _ = self.event_tx.send(ClientNotification::Event(event.clone()));
        Ok(event)
    }

    /// 批量记录多个事件。
    async fn record_turn_payloads<I>(
        &self,
        session_id: &SessionId,
        turn_id: Option<&TurnId>,
        payloads: I,
    ) -> Result<(), String>
    where
        I: IntoIterator<Item = EventPayload>,
    {
        for payload in payloads {
            self.record_and_broadcast(session_id, turn_id, payload)
                .await?;
        }
        Ok(())
    }

    /// 发送错误通知给客户端。
    fn send_error(&self, code: i32, message: &str) {
        let _ = self.event_tx.send(ClientNotification::Error {
            code,
            message: message.into(),
        });
    }
}

#[cfg(test)]
mod tests;
