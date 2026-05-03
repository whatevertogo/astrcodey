//! 命令处理器 — 使用 ServerRuntime 处理客户端命令。
//!
//! 传输层无关：同时被 stdio 二进制和进程内 CLI 使用。
//! 负责将 `ClientCommand` 路由到对应的服务方法，并通过广播通道发送通知。

use std::{collections::HashMap, sync::Arc};

use astrcode_context::compaction::{
    CompactError, CompactResult, CompactSkipReason, CompactSummaryRenderOptions,
};
use astrcode_core::{
    config::ModelSelection,
    event::{Event, EventPayload},
    extension::{CompactTrigger, ExtensionEvent},
    llm::{LlmContent, LlmMessage},
    storage::CompactSnapshotInput,
    types::{SessionId, TurnId, new_message_id, new_turn_id},
};
use astrcode_extensions::context::ServerExtensionContext;
use astrcode_protocol::{
    commands::ClientCommand,
    events::{ClientNotification, MessageDto, SessionListItem, SessionSnapshot},
};
use astrcode_tools::registry::ToolRegistry;
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    agent::{
        Agent, AgentError, AgentServices, AgentSignal, AgentTurnOutput,
        compact::{
            CompactHookContext, collect_compact_instructions, compact_trigger_name,
            compact_with_forked_provider, dispatch_post_compact,
        },
        drive_agent,
    },
    bootstrap::{
        ServerRuntime, build_system_prompt_snapshot, build_tool_registry_snapshot,
        prompt_fingerprint,
    },
    session::{
        CompactContinuationAppendInput, CompactContinuationCreateInput,
        append_compact_continuation_events, create_compact_continuation_session,
    },
};

struct AgentTurnInput {
    sid: SessionId,
    turn_id: TurnId,
    working_dir: String,
    tool_registry: Arc<ToolRegistry>,
    system_prompt: String,
    history: Vec<LlmMessage>,
    text: String,
    actor_tx: mpsc::UnboundedSender<CommandMessage>,
}

#[derive(Clone)]
pub struct CommandHandle {
    tx: mpsc::UnboundedSender<CommandMessage>,
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

struct PendingCompactContinuation {
    parent_session_id: SessionId,
    working_dir: String,
    model_id: String,
    system_prompt: String,
    tool_registry: Arc<ToolRegistry>,
    trigger: CompactTrigger,
    compaction: CompactResult,
    switch_active: bool,
}

impl CommandHandle {
    pub async fn handle(&self, command: ClientCommand) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::ClientCommand { command, reply })
            .map_err(|_| "command actor is unavailable".to_string())?;
        rx.await
            .map_err(|_| "command actor dropped response".to_string())?
    }

    pub async fn create_session(&self, working_dir: String) -> Result<SessionId, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::CreateSession { working_dir, reply })
            .map_err(|_| "command actor is unavailable".to_string())?;
        rx.await
            .map_err(|_| "command actor dropped response".to_string())?
    }

    pub async fn submit_prompt_for_session(
        &self,
        session_id: SessionId,
        text: String,
    ) -> Result<TurnId, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::SubmitPromptForSession {
                session_id,
                text,
                reply,
            })
            .map_err(|_| "command actor is unavailable".to_string())?;
        rx.await
            .map_err(|_| "command actor dropped response".to_string())?
    }

    pub async fn compact_session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionId>, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::CompactSession { session_id, reply })
            .map_err(|_| "command actor is unavailable".to_string())?;
        rx.await
            .map_err(|_| "command actor dropped response".to_string())?
    }

    pub async fn abort_session(&self, session_id: SessionId) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CommandMessage::AbortSession { session_id, reply })
            .map_err(|_| "command actor is unavailable".to_string())?;
        rx.await
            .map_err(|_| "command actor dropped response".to_string())?
    }
}

enum CommandMessage {
    ClientCommand {
        command: ClientCommand,
        reply: oneshot::Sender<Result<(), String>>,
    },
    CreateSession {
        working_dir: String,
        reply: oneshot::Sender<Result<SessionId, String>>,
    },
    SubmitPromptForSession {
        session_id: SessionId,
        text: String,
        reply: oneshot::Sender<Result<TurnId, String>>,
    },
    CompactSession {
        session_id: SessionId,
        reply: oneshot::Sender<Result<Option<SessionId>, String>>,
    },
    AbortSession {
        session_id: SessionId,
        reply: oneshot::Sender<Result<(), String>>,
    },
    AgentEvent {
        session_id: SessionId,
        turn_id: TurnId,
        payload: EventPayload,
    },
    AgentTurnFinished {
        session_id: SessionId,
        turn_id: TurnId,
        output: AgentTurnOutput,
    },
    AgentTurnFailed {
        session_id: SessionId,
        turn_id: TurnId,
        error: AgentError,
        emitted_error: bool,
    },
    AgentAutoCompact {
        session_id: SessionId,
        turn_id: TurnId,
        trigger: CompactTrigger,
        compaction: CompactResult,
        reply: oneshot::Sender<Result<SessionId, String>>,
    },
}

impl CommandHandler {
    /// 创建新的命令处理器。
    ///
    /// # 参数
    /// - `runtime`: 服务器运行时服务集合
    /// - `event_tx`: 事件广播发送端
    fn new(
        runtime: Arc<ServerRuntime>,
        event_tx: broadcast::Sender<ClientNotification>,
        actor_tx: mpsc::UnboundedSender<CommandMessage>,
    ) -> Self {
        Self {
            runtime,
            event_tx,
            active_session_id: None,
            session_tool_registries: HashMap::new(),
            active_turns: HashMap::new(),
            actor_tx,
        }
    }

    pub fn spawn_actor(
        runtime: Arc<ServerRuntime>,
        event_tx: broadcast::Sender<ClientNotification>,
    ) -> CommandHandle {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut handler = Self::new(runtime, event_tx, tx.clone());
        tokio::spawn(async move {
            handler.run(rx).await;
        });
        CommandHandle { tx }
    }

    async fn run(&mut self, mut rx: mpsc::UnboundedReceiver<CommandMessage>) {
        while let Some(message) = rx.recv().await {
            self.handle_message(message).await;
        }
    }

    async fn handle_message(&mut self, message: CommandMessage) {
        match message {
            CommandMessage::ClientCommand { command, reply } => {
                let _ = reply.send(self.handle(command).await);
            },
            CommandMessage::CreateSession { working_dir, reply } => {
                let _ = reply.send(self.create_session(working_dir).await);
            },
            CommandMessage::SubmitPromptForSession {
                session_id,
                text,
                reply,
            } => {
                let _ = reply.send(self.submit_prompt_for_session(session_id, text).await);
            },
            CommandMessage::CompactSession { session_id, reply } => {
                let _ = reply.send(self.compact_session(&session_id).await);
            },
            CommandMessage::AbortSession { session_id, reply } => {
                let _ = reply.send(self.abort_session(&session_id).await);
            },
            CommandMessage::AgentEvent {
                session_id,
                turn_id,
                payload,
            } => {
                if self.active_turn_matches(&session_id, &turn_id) {
                    let _ = self
                        .record_and_broadcast(&session_id, Some(&turn_id), payload)
                        .await;
                }
            },
            CommandMessage::AgentTurnFinished {
                session_id,
                turn_id,
                output,
            } => {
                self.finish_agent_turn(session_id, turn_id, output).await;
            },
            CommandMessage::AgentTurnFailed {
                session_id,
                turn_id,
                error,
                emitted_error,
            } => {
                self.fail_agent_turn(session_id, turn_id, error, emitted_error)
                    .await;
            },
            CommandMessage::AgentAutoCompact {
                session_id,
                turn_id,
                trigger,
                compaction,
                reply,
            } => {
                let result = self
                    .continue_active_turn_from_compaction(session_id, turn_id, trigger, compaction)
                    .await;
                let _ = reply.send(result);
            },
        }
    }

    /// 处理一个客户端命令，将其路由到对应的处理方法。
    ///
    /// 支持的命令包括：创建会话、提交提示词、列出会话、中止回合、
    /// 恢复/切换会话、删除会话等。
    pub async fn handle(&mut self, cmd: ClientCommand) -> Result<(), String> {
        match cmd {
            ClientCommand::CreateSession { working_dir } => {
                let _ = self.create_session(working_dir).await;
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
                        session_id: summary.session_id,
                        created_at: summary.created_at,
                        last_active_at: summary.updated_at,
                        working_dir: summary.working_dir,
                        parent_session_id: summary.parent_session_id,
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

            ClientCommand::ResumeSession { session_id }
            | ClientCommand::SwitchSession { session_id } => {
                self.resume_session(session_id).await;
            },

            ClientCommand::DeleteSession { session_id } => {
                // Dispatch SessionShutdown hook before deletion
                {
                    let ext_ctx = ServerExtensionContext::new(
                        session_id.clone(),
                        String::new(),
                        ModelSelection {
                            profile_name: String::new(),
                            model: self.runtime.effective.llm.model_id.clone(),
                            provider_kind: String::new(),
                        },
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
                        self.session_tool_registries.remove(&session_id);
                        if self.active_session_id.as_ref() == Some(&session_id) {
                            self.active_session_id = None;
                        }
                    },
                    Err(e) => self.send_error(40401, &format!("Session not found: {e}")),
                }
            },

            _ => {
                self.send_error(-32601, "Not implemented");
            },
        }
        Ok(())
    }

    /// 创建新会话，分发 SessionStart 扩展事件，并固定该会话的工具和 system prompt 快照。
    pub async fn create_session(&mut self, working_dir: String) -> Result<SessionId, String> {
        let model_id = self.runtime.effective.llm.model_id.clone();
        match self
            .runtime
            .session_manager
            .create(&working_dir, &model_id, 2048, None)
            .await
        {
            Ok(event) => {
                self.active_session_id = Some(event.session_id.clone());
                let _ = self.event_tx.send(ClientNotification::Event(event.clone()));
                let ext_ctx = ServerExtensionContext::new(
                    event.session_id.clone(),
                    working_dir.clone(),
                    ModelSelection {
                        profile_name: String::new(),
                        model: self.runtime.effective.llm.model_id.clone(),
                        provider_kind: String::new(),
                    },
                );
                if let Err(e) = self
                    .runtime
                    .extension_runner
                    .dispatch(ExtensionEvent::SessionStart, &ext_ctx)
                    .await
                {
                    self.send_error(-32603, &e.to_string());
                    return Err(e.to_string());
                }

                match self
                    .initialize_session_prompt(&event.session_id, &working_dir)
                    .await
                {
                    Ok(_) => Ok(event.session_id),
                    Err(e) => {
                        self.send_error(-32603, &e);
                        Err(e)
                    },
                }
            },
            Err(e) => {
                self.send_error(-32603, &e.to_string());
                Err(e.to_string())
            },
        }
    }

    /// 提交用户提示词，创建回合并在后台启动 Agent 处理。
    ///
    /// 如果已有回合在运行则拒绝（返回 40900 错误）。
    /// 成功提交后，回合在独立的 tokio 任务中异步执行。
    async fn submit_prompt(&mut self, text: String) -> Result<(), String> {
        let sid = self.ensure_session().await?;
        match self.submit_prompt_for_session(sid, text).await {
            Ok(_) => Ok(()),
            Err(error) if error.contains("already running") => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// 向指定会话提交用户提示词。
    ///
    /// HTTP 调用必须走这个显式 session 入口；stdio 的 active session 只是一层
    /// convenience adapter。
    pub async fn submit_prompt_for_session(
        &mut self,
        sid: SessionId,
        text: String,
    ) -> Result<TurnId, String> {
        if self.active_turns.contains_key(&sid) {
            self.send_error(40900, "A turn is already running");
            return Err("A turn is already running".into());
        }

        self.runtime
            .session_manager
            .resume(&sid)
            .await
            .map_err(|e| format!("Session {sid} not found: {e}"))?;
        let state = self
            .runtime
            .session_manager
            .read_model(&sid)
            .await
            .map_err(|e| format!("read session {sid}: {e}"))?;
        let history = state.provider_messages();
        let working_dir = state.working_dir;
        let model_id = state.model_id;
        let system_prompt = state.system_prompt;
        let tool_registry = self.ensure_tool_registry(&sid, &working_dir).await;
        let system_prompt = match system_prompt {
            Some(system_prompt) => system_prompt,
            None => {
                self.configure_session_prompt(&sid, &working_dir, &tool_registry, None)
                    .await?
            },
        };
        let turn_id = new_turn_id();

        self.record_and_broadcast(&sid, Some(&turn_id), EventPayload::TurnStarted)
            .await?;
        self.record_and_broadcast(
            &sid,
            Some(&turn_id),
            EventPayload::UserMessage {
                message_id: new_message_id(),
                text: text.clone(),
            },
        )
        .await?;
        self.record_and_broadcast(&sid, Some(&turn_id), EventPayload::AgentRunStarted)
            .await?;

        let switch_active_on_continuation = self.active_session_id.as_ref() == Some(&sid);
        let handle = self.spawn_agent_turn(AgentTurnInput {
            sid: sid.clone(),
            turn_id: turn_id.clone(),
            working_dir: working_dir.clone(),
            tool_registry: Arc::clone(&tool_registry),
            system_prompt: system_prompt.clone(),
            history,
            text,
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

    /// 中止当前活跃的回合，取消后台任务并记录完成事件。
    async fn abort_active_turn(&mut self) -> Result<(), String> {
        let Some(sid) = self.active_session_id.clone() else {
            self.send_error(40400, "No active turn");
            return Ok(());
        };
        self.abort_session(&sid).await
    }

    /// 中止指定会话的活跃回合。
    pub async fn abort_session(&mut self, session_id: &SessionId) -> Result<(), String> {
        let Some(active_turn) = self.active_turns.remove(session_id) else {
            self.send_error(40400, "No active turn");
            return Err("No active turn".into());
        };

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
        .await?;
        record_and_broadcast(
            &self.runtime,
            &self.event_tx,
            &active_turn.session_id,
            Some(&active_turn.turn_id),
            EventPayload::AgentRunCompleted {
                reason: "aborted".into(),
            },
        )
        .await?;
        Ok(())
    }

    async fn compact_active_session(&mut self) -> Result<(), String> {
        let Some(sid) = self.active_session_id.clone() else {
            self.send_error(40400, "No active session");
            return Ok(());
        };
        self.compact_session(&sid).await.map(|_| ())
    }

    /// 手动压缩指定会话。
    pub async fn compact_session(&mut self, sid: &SessionId) -> Result<Option<SessionId>, String> {
        if self.active_turns.contains_key(sid) {
            self.send_error(40900, "Cannot compact while a turn is running");
            return Err("Cannot compact while a turn is running".into());
        }

        let state = self
            .runtime
            .session_manager
            .read_model(sid)
            .await
            .map_err(|e| format!("read session {sid}: {e}"))?;
        let tool_registry = self.ensure_tool_registry(sid, &state.working_dir).await;
        let provider_messages = state.provider_messages();
        let tools = tool_registry.list_definitions();
        let compact_instructions = match collect_compact_instructions(
            &self.runtime.extension_runner,
            CompactHookContext {
                session_id: sid,
                working_dir: &state.working_dir,
                model_id: &state.model_id,
                tools: &tools,
                trigger: CompactTrigger::ManualCommand,
                message_count: provider_messages.len(),
            },
        )
        .await
        {
            Ok(instructions) => instructions,
            Err(error) => {
                self.send_error(-32603, &format!("Compaction failed: {error}"));
                return Ok(None);
            },
        };
        let snapshot_path = match self
            .runtime
            .session_manager
            .write_compact_snapshot(
                sid,
                CompactSnapshotInput {
                    trigger: compact_trigger_name(CompactTrigger::ManualCommand).into(),
                    model_id: state.model_id.clone(),
                    working_dir: state.working_dir.clone(),
                    system_prompt: state.system_prompt.clone(),
                    provider_messages: provider_messages.clone(),
                },
            )
            .await
        {
            Ok(path) => path,
            Err(error) => {
                self.send_error(
                    -32603,
                    &format!("Compaction failed: could not write transcript snapshot: {error}"),
                );
                return Ok(None);
            },
        };
        let render_options = CompactSummaryRenderOptions {
            transcript_path: snapshot_path,
        };
        let compaction = match compact_with_forked_provider(
            Arc::clone(&self.runtime.llm_provider),
            tools.clone(),
            &provider_messages,
            state.system_prompt.as_deref(),
            self.runtime.context_assembler.settings(),
            &compact_instructions,
            &render_options,
        )
        .await
        {
            Ok(compaction) => compaction,
            Err(CompactError::Skip(
                CompactSkipReason::Empty | CompactSkipReason::NothingToCompact,
            )) => {
                self.send_error(40000, "Nothing to compact");
                return Ok(None);
            },
            Err(error) => {
                self.send_error(-32603, &format!("Compaction failed: {error}"));
                return Ok(None);
            },
        };

        if let Err(error) = dispatch_post_compact(
            &self.runtime.extension_runner,
            CompactHookContext {
                session_id: sid,
                working_dir: &state.working_dir,
                model_id: &state.model_id,
                tools: &tools,
                trigger: CompactTrigger::ManualCommand,
                message_count: provider_messages.len(),
            },
            &compaction,
        )
        .await
        {
            self.send_error(-32603, &format!("Compaction failed: {error}"));
            return Ok(None);
        }

        let system_prompt = match &state.system_prompt {
            Some(system_prompt) => system_prompt.clone(),
            None => {
                self.configure_session_prompt(sid, &state.working_dir, &tool_registry, None)
                    .await?
            },
        };
        let child_session_id = self
            .create_compact_continuation_child(PendingCompactContinuation {
                parent_session_id: sid.clone(),
                working_dir: state.working_dir.clone(),
                model_id: state.model_id.clone(),
                system_prompt,
                tool_registry,
                trigger: CompactTrigger::ManualCommand,
                compaction,
                switch_active: true,
            })
            .await?;
        Ok(Some(child_session_id))
    }

    /// 恢复或切换到指定会话，从磁盘重放事件并构建快照发送给客户端。
    async fn resume_session(&mut self, session_id: SessionId) {
        match self.runtime.session_manager.resume(&session_id).await {
            Ok(_) => {
                let state = match self.runtime.session_manager.read_model(&session_id).await {
                    Ok(state) => state,
                    Err(e) => {
                        self.send_error(40401, &format!("Session not found: {e}"));
                        return;
                    },
                };
                let working_dir = state.working_dir.clone();
                let needs_prompt = state.system_prompt.is_none();
                let snapshot = SessionSnapshot {
                    session_id: session_id.clone(),
                    cursor: state.cursor(),
                    messages: state.messages.iter().map(message_to_dto).collect(),
                    model_id: state.model_id.clone(),
                    working_dir: working_dir.clone(),
                };

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
                    session_id,
                    snapshot,
                });
            },
            Err(e) => self.send_error(40401, &format!("Session not found: {e}")),
        }
    }

    /// 确保存在活跃会话，如果没有则自动创建一个。
    /// 使用当前工作目录作为新会话的工作目录。
    async fn ensure_session(&mut self) -> Result<SessionId, String> {
        if let Some(ref sid) = self.active_session_id {
            return Ok(sid.clone());
        }

        let model_id = self.runtime.effective.llm.model_id.clone();
        let wd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".into());
        let event = self
            .runtime
            .session_manager
            .create(&wd, &model_id, 2048, None)
            .await
            .map_err(|e| format!("create session: {e}"))?;

        let sid = event.session_id.clone();
        self.active_session_id = Some(sid.clone());
        let _ = self.event_tx.send(ClientNotification::Event(event));
        let ext_ctx = ServerExtensionContext::new(
            sid.clone(),
            wd.clone(),
            ModelSelection {
                profile_name: String::new(),
                model: self.runtime.effective.llm.model_id.clone(),
                provider_kind: String::new(),
            },
        );
        self.runtime
            .extension_runner
            .dispatch(ExtensionEvent::SessionStart, &ext_ctx)
            .await
            .map_err(|e| e.to_string())?;
        self.initialize_session_prompt(&sid, &wd).await?;
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
        build_tool_registry_snapshot(
            &self.runtime.extension_runner,
            working_dir,
            self.runtime.effective.llm.read_timeout_secs,
        )
        .await
    }

    async fn configure_session_prompt(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        tool_registry: &ToolRegistry,
        extra_system_prompt: Option<&str>,
    ) -> Result<String, String> {
        let tools = tool_registry.list_definitions();
        let (system_prompt, fingerprint) = build_system_prompt_snapshot(
            &self.runtime.extension_runner,
            session_id,
            working_dir,
            &self.runtime.effective.llm.model_id,
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
                actor_tx,
            } = input;
            let current_session_id = Arc::new(tokio::sync::Mutex::new(sid.clone()));

            let agent = Agent::new(
                sid.clone(),
                working_dir,
                system_prompt,
                runtime.effective.llm.model_id.clone(),
                AgentServices {
                    llm: runtime.llm_provider.clone(),
                    tool_registry,
                    extension_runner: runtime.extension_runner.clone(),
                    context_assembler: runtime.context_assembler.clone(),
                    session_manager: runtime.session_manager.clone(),
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
                            let result = if actor_tx
                                .send(CommandMessage::AgentAutoCompact {
                                    session_id,
                                    turn_id,
                                    trigger,
                                    compaction,
                                    reply: actor_reply,
                                })
                                .is_err()
                            {
                                Err("command actor is unavailable".to_string())
                            } else {
                                match actor_rx.await {
                                    Ok(result) => result,
                                    Err(_) => {
                                        Err("command actor dropped auto compact response".into())
                                    },
                                }
                            };
                            if let Ok(child_session_id) = &result {
                                *current_session_id.lock().await = child_session_id.clone();
                            }
                            let _ = reply.send(result);
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

    async fn create_compact_continuation_child(
        &mut self,
        input: PendingCompactContinuation,
    ) -> Result<SessionId, String> {
        let working_dir = input.working_dir.clone();
        let model_id = input.model_id.clone();
        let system_prompt = input.system_prompt.clone();
        let parent_session_id = input.parent_session_id.clone();
        let is_manual_compact = input.trigger == CompactTrigger::ManualCommand;
        let continuation = create_compact_continuation_session(
            &self.runtime.session_manager,
            CompactContinuationCreateInput {
                parent_session_id: input.parent_session_id,
                working_dir: input.working_dir,
                model_id: input.model_id,
            },
        )
        .await?;
        let child_session_id = continuation.child_session_id.clone();
        self.session_tool_registries
            .insert(child_session_id.clone(), Arc::clone(&input.tool_registry));
        let _ = self.event_tx.send(ClientNotification::Event(
            continuation.child_started.clone(),
        ));

        let ext_ctx = ServerExtensionContext::new(
            child_session_id.clone(),
            working_dir,
            ModelSelection {
                profile_name: String::new(),
                model: model_id,
                provider_kind: String::new(),
            },
        );
        self.runtime
            .extension_runner
            .dispatch(ExtensionEvent::SessionStart, &ext_ctx)
            .await
            .map_err(|e| e.to_string())?;

        let events = append_compact_continuation_events(
            &self.runtime.session_manager,
            CompactContinuationAppendInput {
                session: continuation,
                system_prompt_fingerprint: prompt_fingerprint(&system_prompt),
                system_prompt,
                trigger_name: compact_trigger_name(input.trigger).into(),
                compaction: input.compaction,
            },
        )
        .await?;
        if is_manual_compact {
            // Auto compact emits this from the agent loop at the real compact
            // point. Manual compact has no agent loop, so emit it here after
            // failure/skip paths are behind us and before the boundary event.
            self.record_and_broadcast(&parent_session_id, None, EventPayload::CompactionStarted)
                .await?;
        }
        for event in events.appended_events {
            let _ = self.event_tx.send(ClientNotification::Event(event));
        }
        if input.switch_active {
            self.active_session_id = Some(child_session_id.clone());
        }
        let child_state = self
            .runtime
            .session_manager
            .read_model(&child_session_id)
            .await
            .map_err(|e| format!("read session {child_session_id}: {e}"))?;
        let _ = self.event_tx.send(ClientNotification::SessionResumed {
            session_id: child_session_id.clone(),
            snapshot: session_snapshot(&child_state),
        });
        Ok(child_session_id)
    }

    async fn continue_active_turn_from_compaction(
        &mut self,
        session_id: SessionId,
        turn_id: TurnId,
        trigger: CompactTrigger,
        compaction: CompactResult,
    ) -> Result<SessionId, String> {
        let Some(mut active_turn) = self.active_turns.remove(&session_id) else {
            return Err("stale auto compact transition".into());
        };
        if active_turn.turn_id != turn_id {
            self.active_turns.insert(session_id, active_turn);
            return Err("stale auto compact transition".into());
        }

        let input = PendingCompactContinuation {
            parent_session_id: session_id.clone(),
            working_dir: active_turn.working_dir.clone(),
            model_id: active_turn.model_id.clone(),
            system_prompt: active_turn.system_prompt.clone(),
            tool_registry: Arc::clone(&active_turn.tool_registry),
            trigger,
            compaction,
            switch_active: active_turn.switch_active_on_continuation,
        };

        match self.create_compact_continuation_child(input).await {
            Ok(child_session_id) => {
                active_turn.session_id = child_session_id.clone();
                self.active_turns
                    .insert(child_session_id.clone(), active_turn);
                Ok(child_session_id)
            },
            Err(error) => {
                self.active_turns.insert(session_id, active_turn);
                Err(error)
            },
        }
    }

    fn active_turn_matches(&self, session_id: &SessionId, turn_id: &TurnId) -> bool {
        self.active_turns
            .get(session_id)
            .is_some_and(|active_turn| &active_turn.turn_id == turn_id)
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

/// 将事件持久化到存储（如果是持久化事件）并广播给所有订阅者。
///
/// 只有 `is_durable()` 返回 true 的事件才会写入磁盘，
/// 非持久化事件（如流式 delta）仅广播不存储。
async fn record_and_broadcast(
    runtime: &ServerRuntime,
    event_tx: &broadcast::Sender<ClientNotification>,
    session_id: &SessionId,
    turn_id: Option<&TurnId>,
    payload: EventPayload,
) -> Result<Event, String> {
    let event = Event::new(session_id.clone(), turn_id.cloned(), payload);
    let event = if event.payload.is_durable() {
        runtime
            .session_manager
            .append_event(event)
            .await
            .map_err(|e| e.to_string())?
    } else {
        event
    };

    let _ = event_tx.send(ClientNotification::Event(event.clone()));
    Ok(event)
}

fn session_snapshot(state: &astrcode_core::storage::SessionReadModel) -> SessionSnapshot {
    SessionSnapshot {
        session_id: state.session_id.clone(),
        cursor: state.cursor(),
        messages: state.messages.iter().map(message_to_dto).collect(),
        model_id: state.model_id.clone(),
        working_dir: state.working_dir.clone(),
    }
}

/// 将 LLM 消息转换为传输层 DTO，用于会话快照。
fn message_to_dto(message: &LlmMessage) -> MessageDto {
    MessageDto {
        role: message.role.as_str().to_string(),
        content: message
            .content
            .iter()
            .map(content_to_text)
            .collect::<Vec<_>>()
            .join(""),
    }
}

/// 将 LLM 内容块转换为纯文本表示，用于客户端展示。
fn content_to_text(content: &LlmContent) -> String {
    match content {
        LlmContent::Text { text } => text.clone(),
        LlmContent::Image { .. } => "[image]".into(),
        LlmContent::ToolCall {
            name, arguments, ..
        } => format!("tool call: {name}({arguments})"),
        LlmContent::ToolResult { content, .. } => content.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::{future, sync::Arc, time::Duration};

    use astrcode_context::{compaction::CompactResult, manager::LlmContextAssembler};
    use astrcode_core::{
        config::{EffectiveConfig, LlmSettings, OpenAiApiMode},
        event::EventPayload,
        llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
        tool::ToolDefinition,
    };
    use astrcode_protocol::events::ClientNotification;
    use astrcode_storage::in_memory::InMemoryEventStore;
    use tokio::sync::mpsc;

    use super::*;
    use crate::session::{
        SessionManager, compact_boundary_payload, session_continued_from_compaction_payload,
    };

    struct MockLlm;

    #[async_trait::async_trait]
    impl LlmProvider for MockLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            let (tx, rx) = mpsc::unbounded_channel();
            let _ = tx.send(LlmEvent::ContentDelta {
                delta: r#"<summary>
1. Primary Request and Intent:
   Compacted conversation summary

2. Key Technical Concepts:
   - compact

3. Files and Code Sections:
   - (none)

4. Errors and fixes:
   - (none)

5. Problem Solving:
   compacted

6. All user messages:
   - (none)

7. Pending Tasks:
   - (none)

8. Current Work:
   compact command

9. Optional Next Step:
   - (none)
</summary>"#
                    .into(),
            });
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
            Ok(rx)
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 200000,
                max_output_tokens: 1024,
            }
        }
    }

    struct PendingLlm;

    struct InvalidSummaryLlm;

    #[async_trait::async_trait]
    impl LlmProvider for PendingLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            future::pending().await
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 1024,
                max_output_tokens: 1024,
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for InvalidSummaryLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            let (tx, rx) = mpsc::unbounded_channel();
            let _ = tx.send(LlmEvent::ContentDelta {
                delta: "not a compact summary".into(),
            });
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
            Ok(rx)
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 200000,
                max_output_tokens: 1024,
            }
        }
    }

    fn test_runtime_with_settings(
        llm_provider: Arc<dyn LlmProvider>,
        context_settings: astrcode_context::settings::ContextWindowSettings,
    ) -> Arc<ServerRuntime> {
        Arc::new(ServerRuntime {
            session_manager: Arc::new(SessionManager::new(Arc::new(InMemoryEventStore::new()))),
            llm_provider,
            context_assembler: Arc::new(LlmContextAssembler::new(context_settings.clone())),
            extension_runner: Arc::new(astrcode_extensions::runner::ExtensionRunner::new(
                Duration::from_secs(1),
                Arc::new(astrcode_extensions::runtime::ExtensionRuntime::new()),
            )),
            effective: EffectiveConfig {
                llm: LlmSettings {
                    provider_kind: "mock".into(),
                    base_url: String::new(),
                    api_key: String::new(),
                    api_mode: OpenAiApiMode::ChatCompletions,
                    model_id: "mock-model".into(),
                    max_tokens: 1024,
                    context_limit: 1024,
                    connect_timeout_secs: 1,
                    read_timeout_secs: 1,
                    max_retries: 0,
                    retry_base_delay_ms: 0,
                    temperature: None,
                    supports_prompt_cache_key: false,
                    prompt_cache_retention: None,
                },
            },
        })
    }

    fn test_runtime_with_llm(llm_provider: Arc<dyn LlmProvider>) -> Arc<ServerRuntime> {
        test_runtime_with_settings(
            llm_provider,
            astrcode_context::settings::ContextWindowSettings::default(),
        )
    }

    fn test_runtime() -> Arc<ServerRuntime> {
        test_runtime_with_llm(Arc::new(MockLlm))
    }

    async fn recv_event(
        event_rx: &mut broadcast::Receiver<ClientNotification>,
    ) -> ClientNotification {
        tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .expect("event should arrive")
            .expect("event channel should stay open")
    }

    async fn wait_for_turn_completed(
        event_rx: &mut broadcast::Receiver<ClientNotification>,
    ) -> String {
        loop {
            let notification = recv_event(event_rx).await;
            let ClientNotification::Event(event) = notification else {
                continue;
            };
            if let EventPayload::TurnCompleted { finish_reason } = event.payload {
                return finish_reason;
            }
        }
    }

    async fn drain_until_compact_boundary(
        event_rx: &mut broadcast::Receiver<ClientNotification>,
    ) -> SessionId {
        loop {
            let notification = recv_event(event_rx).await;
            let ClientNotification::Event(event) = notification else {
                continue;
            };
            if let EventPayload::CompactBoundaryCreated {
                continued_session_id,
                ..
            } = event.payload
            {
                return continued_session_id;
            }
        }
    }

    async fn collect_turn_ids_until_completed(
        event_rx: &mut broadcast::Receiver<ClientNotification>,
    ) -> (String, Vec<Option<TurnId>>) {
        let mut turn_ids = Vec::new();
        loop {
            let notification = recv_event(event_rx).await;
            let ClientNotification::Event(event) = notification else {
                continue;
            };
            match event.payload {
                EventPayload::TurnStarted
                | EventPayload::UserMessage { .. }
                | EventPayload::AssistantMessageCompleted { .. } => {
                    turn_ids.push(event.turn_id);
                },
                EventPayload::TurnCompleted { finish_reason } => {
                    turn_ids.push(event.turn_id);
                    return (finish_reason, turn_ids);
                },
                _ => {},
            }
        }
    }

    #[test]
    fn compact_payload_helpers_split_projection_and_audit_fields() {
        let compaction = CompactResult {
            pre_tokens: 100,
            post_tokens: 20,
            summary: "summary".into(),
            messages_removed: 2,
            context_messages: vec![LlmMessage::system("hidden context")],
            retained_messages: vec![LlmMessage::user("retained")],
            transcript_path: Some("compact.jsonl".into()),
        };

        let boundary = compact_boundary_payload("manual_command", &compaction, "child".into());
        let continued =
            session_continued_from_compaction_payload("parent".into(), "7".into(), &compaction);

        assert!(matches!(
            boundary,
            EventPayload::CompactBoundaryCreated {
                continued_session_id,
                transcript_path: Some(path),
                ..
            } if continued_session_id == "child" && path == "compact.jsonl"
        ));
        assert!(matches!(
            continued,
            EventPayload::SessionContinuedFromCompaction {
                parent_session_id,
                parent_cursor,
                context_messages,
                retained_messages,
                ..
            } if parent_session_id == "parent"
                && parent_cursor == "7"
                && context_messages.len() == 1
                && retained_messages.len() == 1
        ));
    }

    #[tokio::test]
    async fn record_and_broadcast_updates_projection_before_broadcast() {
        let runtime = test_runtime();
        let start_event = runtime
            .session_manager
            .create(".", "mock-model", 2048, None)
            .await
            .unwrap();
        let sid = start_event.session_id.clone();
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);

        record_and_broadcast(
            &runtime,
            &event_tx,
            &sid,
            None,
            EventPayload::SystemPromptConfigured {
                text: "ordered prompt".into(),
                fingerprint: "fingerprint".into(),
            },
        )
        .await
        .unwrap();

        let ClientNotification::Event(event) = recv_event(&mut event_rx).await else {
            panic!("expected event notification");
        };
        assert!(event.seq.is_some());

        let model = runtime.session_manager.read_model(&sid).await.unwrap();
        assert_eq!(model.system_prompt.as_deref(), Some("ordered prompt"));
    }

    #[tokio::test]
    async fn create_session_configures_system_prompt() {
        let runtime = test_runtime();
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);
        let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

        let sid = handler.create_session(".".into()).await.unwrap();

        let mut saw_configured = false;
        for _ in 0..2 {
            if let ClientNotification::Event(event) = recv_event(&mut event_rx).await {
                if let EventPayload::SystemPromptConfigured { text, fingerprint } = event.payload {
                    saw_configured = true;
                    assert!(text.contains("# Identity"));
                    assert!(!fingerprint.is_empty());
                }
            }
        }
        assert!(saw_configured);

        let state = runtime.session_manager.read_model(&sid).await.unwrap();
        assert!(
            state
                .system_prompt
                .as_deref()
                .is_some_and(|prompt| prompt.contains("# Identity"))
        );
        assert!(state.messages.is_empty());
    }

    #[tokio::test]
    async fn submit_prompt_reuses_session_system_prompt() {
        let runtime = test_runtime();
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(128);
        let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

        let sid = handler.create_session(".".into()).await.unwrap();
        let initial_prompt = {
            let state = runtime.session_manager.read_model(&sid).await.unwrap();
            state.system_prompt.clone()
        };

        handler
            .submit_prompt_for_session(sid.clone(), "one".into())
            .await
            .unwrap();
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");

        handler
            .submit_prompt_for_session(sid.clone(), "two".into())
            .await
            .unwrap();
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");

        let state = runtime.session_manager.read_model(&sid).await.unwrap();
        assert_eq!(state.system_prompt, initial_prompt);
    }

    #[tokio::test]
    async fn submit_prompt_configures_missing_session_system_prompt() {
        let runtime = test_runtime();
        let start_event = runtime
            .session_manager
            .create(".", "mock-model", 2048, None)
            .await
            .unwrap();
        let sid = start_event.session_id.clone();
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(128);
        let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

        handler
            .submit_prompt_for_session(sid.clone(), "hello".into())
            .await
            .unwrap();
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");

        let state = runtime.session_manager.read_model(&sid).await.unwrap();
        assert!(
            state
                .system_prompt
                .as_deref()
                .is_some_and(|prompt| prompt.contains("# Identity"))
        );
    }

    #[tokio::test]
    async fn submit_prompt_uses_one_turn_id_for_turn_events() {
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);
        let handler = CommandHandler::spawn_actor(test_runtime(), event_tx);

        let sid = handler.create_session(".".into()).await.unwrap();
        handler
            .submit_prompt_for_session(sid, "hi".into())
            .await
            .unwrap();
        let (finish_reason, turn_ids) = collect_turn_ids_until_completed(&mut event_rx).await;
        assert_eq!(finish_reason, "stop");

        assert!(
            turn_ids.len() >= 4,
            "expected turn lifecycle, user and assistant events"
        );
        let first = turn_ids[0].clone();
        assert!(first.is_some(), "turn events should carry a turn_id");
        assert!(
            turn_ids.iter().all(|turn_id| *turn_id == first),
            "all events in one prompt should share the same turn_id"
        );
    }

    #[tokio::test]
    async fn submit_prompt_rejects_second_running_turn() {
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);
        let handler =
            CommandHandler::spawn_actor(test_runtime_with_llm(Arc::new(PendingLlm)), event_tx);

        let sid = handler.create_session(".".into()).await.unwrap();
        handler
            .submit_prompt_for_session(sid.clone(), "first".into())
            .await
            .unwrap();
        let error = handler
            .submit_prompt_for_session(sid.clone(), "second".into())
            .await
            .unwrap_err();
        assert!(error.contains("already running"));

        let mut saw_busy = false;
        while let Ok(notification) = event_rx.try_recv() {
            if let ClientNotification::Error { code: 40900, .. } = notification {
                saw_busy = true;
                break;
            }
        }
        assert!(saw_busy, "second prompt should be rejected while turn runs");

        handler.abort_session(sid).await.unwrap();
    }

    #[tokio::test]
    async fn abort_stops_active_turn_and_records_completion() {
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);
        let handler =
            CommandHandler::spawn_actor(test_runtime_with_llm(Arc::new(PendingLlm)), event_tx);

        let sid = handler.create_session(".".into()).await.unwrap();
        handler
            .submit_prompt_for_session(sid.clone(), "keep running".into())
            .await
            .unwrap();

        handler.abort_session(sid).await.unwrap();

        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "aborted");
    }

    #[tokio::test]
    async fn compact_session_rejects_running_turn_without_compaction_started() {
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(128);
        let handler =
            CommandHandler::spawn_actor(test_runtime_with_llm(Arc::new(PendingLlm)), event_tx);

        let sid = handler.create_session(".".into()).await.unwrap();
        handler
            .submit_prompt_for_session(sid.clone(), "keep running".into())
            .await
            .unwrap();
        while event_rx.try_recv().is_ok() {}

        let error = handler.compact_session(sid.clone()).await.unwrap_err();
        assert_eq!(error, "Cannot compact while a turn is running");

        let mut saw_conflict = false;
        while let Ok(notification) = event_rx.try_recv() {
            match notification {
                ClientNotification::Error { code, .. } => {
                    saw_conflict |= code == 40900;
                },
                ClientNotification::Event(event) => {
                    assert!(
                        !matches!(event.payload, EventPayload::CompactionStarted),
                        "rejected compact must not leave clients in compacting state"
                    );
                },
                _ => {},
            }
        }
        assert!(saw_conflict);

        handler.abort_session(sid).await.unwrap();
    }

    #[tokio::test]
    async fn stale_agent_finish_after_abort_is_ignored() {
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);
        let handler =
            CommandHandler::spawn_actor(test_runtime_with_llm(Arc::new(PendingLlm)), event_tx);

        let sid = handler.create_session(".".into()).await.unwrap();
        let turn_id = handler
            .submit_prompt_for_session(sid.clone(), "keep running".into())
            .await
            .unwrap();
        handler.abort_session(sid.clone()).await.unwrap();
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "aborted");

        handler
            .tx
            .send(CommandMessage::AgentTurnFinished {
                session_id: sid,
                turn_id,
                output: AgentTurnOutput {
                    text: "late".into(),
                    finish_reason: "stop".into(),
                    tool_results: vec![],
                    auto_compaction: None,
                },
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;

        while let Ok(notification) = event_rx.try_recv() {
            if let ClientNotification::Event(event) = notification {
                if matches!(event.payload, EventPayload::TurnCompleted { .. }) {
                    panic!("stale AgentTurnFinished should not emit a second completion");
                }
            }
        }
    }

    #[tokio::test]
    async fn compact_command_rewrites_provider_history_without_exposing_summary() {
        let settings = astrcode_context::settings::ContextWindowSettings::default();
        let runtime = test_runtime_with_settings(Arc::new(MockLlm), settings);
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(256);
        let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

        let parent_id = handler.create_session(".".into()).await.unwrap();
        for text in ["one", "two", "three"] {
            handler
                .submit_prompt_for_session(parent_id.clone(), text.into())
                .await
                .unwrap();
            assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
        }

        let child_id = handler
            .compact_session(parent_id.clone())
            .await
            .unwrap()
            .unwrap();
        let continued_session_id = drain_until_compact_boundary(&mut event_rx).await;
        assert_eq!(child_id, continued_session_id);

        let parent_state = runtime
            .session_manager
            .read_model(&parent_id)
            .await
            .unwrap();
        assert!(parent_state.context_messages.is_empty());
        assert!(!parent_state.messages.is_empty());

        let state = runtime.session_manager.read_model(&child_id).await.unwrap();
        assert_eq!(state.parent_session_id.as_deref(), Some(parent_id.as_str()));
        assert!(!state.context_messages.is_empty());
        assert!(state.provider_messages().iter().any(|message| {
            message_to_dto(message)
                .content
                .contains("<compact_summary>")
        }));
        assert!(state.messages.iter().all(|message| {
            !message_to_dto(message)
                .content
                .contains("<compact_summary>")
        }));
    }

    #[tokio::test]
    async fn compact_command_compacts_existing_hidden_context_again() {
        let settings = astrcode_context::settings::ContextWindowSettings::default();
        let runtime = test_runtime_with_settings(Arc::new(MockLlm), settings);
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(512);
        let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

        let first_session_id = handler.create_session(".".into()).await.unwrap();
        for text in ["one", "two", "three", "four"] {
            handler
                .submit_prompt_for_session(first_session_id.clone(), text.into())
                .await
                .unwrap();
            assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
        }

        let first_child_id = handler
            .compact_session(first_session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            first_child_id,
            drain_until_compact_boundary(&mut event_rx).await
        );
        let first_summary = {
            let state = runtime
                .session_manager
                .read_model(&first_child_id)
                .await
                .unwrap();
            message_to_dto(&state.context_messages[0]).content
        };

        handler
            .submit_prompt_for_session(first_child_id.clone(), "five".into())
            .await
            .unwrap();
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
        let second_child_id = handler
            .compact_session(first_child_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            second_child_id,
            drain_until_compact_boundary(&mut event_rx).await
        );

        let state = runtime
            .session_manager
            .read_model(&second_child_id)
            .await
            .unwrap();
        let second_summary = message_to_dto(&state.context_messages[0]).content;
        assert!(
            second_summary.contains("Compacted conversation summary"),
            "second compact should preserve a provider summary"
        );
        assert!(
            first_summary.contains("Compacted conversation summary"),
            "first compact should preserve a provider summary"
        );
    }

    #[tokio::test]
    async fn auto_compact_switches_active_session_to_continuation_child() {
        let settings = astrcode_context::settings::ContextWindowSettings {
            compact_threshold_percent: 0.0,
            ..Default::default()
        };
        let runtime = test_runtime_with_settings(Arc::new(MockLlm), settings);
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(512);
        let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

        let parent_id = handler.create_session(".".into()).await.unwrap();
        for index in 0..3 {
            runtime
                .session_manager
                .append_event(Event::new(
                    parent_id.clone(),
                    None,
                    EventPayload::UserMessage {
                        message_id: new_message_id(),
                        text: format!("old user {index} {}", "x ".repeat(20)),
                    },
                ))
                .await
                .unwrap();
            runtime
                .session_manager
                .append_event(Event::new(
                    parent_id.clone(),
                    None,
                    EventPayload::AssistantMessageCompleted {
                        message_id: new_message_id(),
                        text: format!("old answer {index} {}", "y ".repeat(20)),
                    },
                ))
                .await
                .unwrap();
        }

        handler
            .submit_prompt_for_session(parent_id.clone(), "current".into())
            .await
            .unwrap();
        let mut compaction_started_count = 0;
        let mut child_id = None;
        let mut turn_completed_session = None;
        loop {
            let notification = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .expect("event should arrive")
                .expect("event channel should remain open");
            let ClientNotification::Event(event) = notification else {
                continue;
            };
            match event.payload {
                EventPayload::CompactionStarted => {
                    compaction_started_count += 1;
                    assert_eq!(event.session_id, parent_id);
                },
                EventPayload::CompactBoundaryCreated {
                    continued_session_id,
                    ..
                } => {
                    assert!(
                        turn_completed_session.is_none(),
                        "compact boundary should be created before turn completion"
                    );
                    assert_eq!(event.session_id, parent_id);
                    child_id = Some(continued_session_id);
                },
                EventPayload::TurnCompleted { finish_reason } => {
                    assert_eq!(finish_reason, "stop");
                    turn_completed_session = Some(event.session_id);
                    if child_id.is_some() {
                        break;
                    }
                },
                _ => {},
            }
        }
        assert_eq!(compaction_started_count, 1);
        let child_id = child_id.expect("compact boundary should create a child session");
        assert_eq!(turn_completed_session.as_deref(), Some(child_id.as_str()));

        let parent = runtime
            .session_manager
            .read_model(&parent_id)
            .await
            .unwrap();
        assert!(parent.context_messages.is_empty());
        let child = runtime.session_manager.read_model(&child_id).await.unwrap();
        assert_eq!(child.parent_session_id.as_deref(), Some(parent_id.as_str()));
        assert!(!child.context_messages.is_empty());
        assert!(child.messages.iter().any(|message| {
            message_to_dto(message)
                .content
                .contains("Compacted conversation summary")
        }));
    }

    #[tokio::test]
    async fn compact_command_does_not_fallback_when_summary_is_invalid() {
        let runtime = test_runtime_with_llm(Arc::new(InvalidSummaryLlm));
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(256);
        let handler = CommandHandler::spawn_actor(Arc::clone(&runtime), event_tx);

        let sid = handler.create_session(".".into()).await.unwrap();
        for text in ["one", "two", "three"] {
            handler
                .submit_prompt_for_session(sid.clone(), text.into())
                .await
                .unwrap();
            assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
        }
        while event_rx.try_recv().is_ok() {}

        assert!(
            handler
                .compact_session(sid.clone())
                .await
                .unwrap()
                .is_none()
        );

        let state = runtime.session_manager.read_model(&sid).await.unwrap();
        assert!(state.context_messages.is_empty());

        while let Ok(notification) = event_rx.try_recv() {
            if let ClientNotification::Event(event) = notification {
                assert!(
                    !matches!(event.payload, EventPayload::CompactionStarted),
                    "failed compact must not leave clients in compacting state"
                );
            }
        }
    }
}
