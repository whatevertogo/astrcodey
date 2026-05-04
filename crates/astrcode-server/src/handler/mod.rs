//! 命令处理器 — 使用 ServerRuntime 处理客户端命令。
//!
//! 传输层无关：同时被 stdio 二进制和进程内 CLI 使用。
//! 负责将 `ClientCommand` 路由到对应的服务方法，并通过广播通道发送通知。

use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    config::ModelSelection,
    event::{Event, EventPayload},
    extension::ExtensionEvent,
    llm::LlmMessage,
    types::{SessionId, TurnId, new_message_id, new_turn_id},
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
    agent::{AgentError, AgentLoop, AgentServices, AgentSignal, AgentTurnOutput, drive_agent},
    bootstrap::{ServerRuntime, build_system_prompt_snapshot, build_tool_registry_snapshot},
};

mod actor;
mod compact;
mod events;
mod snapshot;

pub use actor::CommandHandle;
use actor::CommandMessage;
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
    actor_tx: mpsc::UnboundedSender<CommandMessage>,
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

            ClientCommand::GetState => {
                self.send_current_state().await;
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
                    session_id,
                    snapshot,
                });
            },
            Err(e) => self.send_error(40401, &format!("Session not found: {e}")),
        }
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

            let agent = AgentLoop::new(
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
                    auto_compact_failures: runtime.auto_compact_failures.clone(),
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

#[cfg(test)]
mod tests;
