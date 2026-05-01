//! 命令处理器 — 使用 ServerRuntime 处理客户端命令。
//!
//! 传输层无关：同时被 stdio 二进制和进程内 CLI 使用。
//! 负责将 `ClientCommand` 路由到对应的服务方法，并通过广播通道发送通知。

use std::{collections::HashMap, sync::Arc};

use astrcode_context::compaction::{CompactError, CompactPromptStyle, CompactSkipReason};
use astrcode_core::{
    config::ModelSelection,
    event::{Event, EventPayload},
    extension::ExtensionEvent,
    llm::{LlmContent, LlmMessage},
    types::{SessionId, TurnId, new_message_id, new_turn_id},
};
use astrcode_extensions::context::ServerExtensionContext;
use astrcode_protocol::{
    commands::ClientCommand,
    events::{ClientNotification, MessageDto, SessionListItem, SessionSnapshot},
};
use astrcode_tools::registry::ToolRegistry;
use tokio::{sync::broadcast, task::JoinHandle};

use crate::{
    agent_loop::{Agent, AgentServices, drive_agent},
    bootstrap::{ServerRuntime, build_system_prompt_snapshot, build_tool_registry_snapshot},
    forked_provider::CompactForkRunner,
    session::compaction_applied_payload,
};

struct AgentTurnInput {
    sid: SessionId,
    turn_id: TurnId,
    working_dir: String,
    tool_registry: Arc<ToolRegistry>,
    system_prompt: String,
    history: Vec<LlmMessage>,
    text: String,
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
    /// 当前正在执行的回合（如有）
    active_turn: Option<ActiveTurn>,
}

/// 正在执行的回合信息，持有对应的 tokio 任务句柄。
struct ActiveTurn {
    session_id: SessionId,
    turn_id: TurnId,
    /// 后台任务的 JoinHandle，可用于取消（abort）回合
    handle: JoinHandle<()>,
}

impl CommandHandler {
    /// 创建新的命令处理器。
    ///
    /// # 参数
    /// - `runtime`: 服务器运行时服务集合
    /// - `event_tx`: 事件广播发送端
    pub fn new(
        runtime: Arc<ServerRuntime>,
        event_tx: broadcast::Sender<ClientNotification>,
    ) -> Self {
        Self {
            runtime,
            event_tx,
            active_session_id: None,
            session_tool_registries: HashMap::new(),
            active_turn: None,
        }
    }

    /// 处理一个客户端命令，将其路由到对应的处理方法。
    ///
    /// 支持的命令包括：创建会话、提交提示词、列出会话、中止回合、
    /// 恢复/切换会话、删除会话等。
    pub async fn handle(&mut self, cmd: ClientCommand) -> Result<(), String> {
        self.clear_finished_turn();

        match cmd {
            ClientCommand::CreateSession { working_dir } => {
                self.create_session(working_dir).await;
            },

            ClientCommand::SubmitPrompt { text, .. } => {
                self.submit_prompt(text).await?;
            },

            ClientCommand::ListSessions => {
                let sids = self
                    .runtime
                    .session_manager
                    .list()
                    .await
                    .unwrap_or_default();
                let active = self.runtime.session_manager.active().await;
                let items: Vec<_> = sids
                    .into_iter()
                    .map(|sid| {
                        // 尝试从内存中获取已加载 session 的真实状态
                        if let Some(session) = active.get(&sid) {
                            // try_read 避免在 list 操作中长时间持锁
                            if let Ok(st) = session.state.try_read() {
                                SessionListItem {
                                    session_id: sid,
                                    created_at: st.created_at.clone(),
                                    last_active_at: String::new(),
                                    working_dir: st.working_dir.clone(),
                                    parent_session_id: None,
                                }
                            } else {
                                SessionListItem {
                                    session_id: sid,
                                    created_at: String::new(),
                                    last_active_at: String::new(),
                                    working_dir: String::new(),
                                    parent_session_id: None,
                                }
                            }
                        } else {
                            SessionListItem {
                                session_id: sid,
                                created_at: String::new(),
                                last_active_at: String::new(),
                                working_dir: String::new(),
                                parent_session_id: None,
                            }
                        }
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
    async fn create_session(&mut self, working_dir: String) {
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
                    return;
                }

                match self
                    .initialize_session_prompt(&event.session_id, &working_dir)
                    .await
                {
                    Ok(_) => {},
                    Err(e) => self.send_error(-32603, &e),
                }
            },
            Err(e) => self.send_error(-32603, &e.to_string()),
        }
    }

    /// 提交用户提示词，创建回合并在后台启动 Agent 处理。
    ///
    /// 如果已有回合在运行则拒绝（返回 40900 错误）。
    /// 成功提交后，回合在独立的 tokio 任务中异步执行。
    async fn submit_prompt(&mut self, text: String) -> Result<(), String> {
        if self.active_turn.is_some() {
            self.send_error(40900, "A turn is already running");
            return Ok(());
        }

        let sid = self.ensure_session().await?;

        let session = self
            .runtime
            .session_manager
            .get(&sid)
            .await
            .ok_or_else(|| format!("Session {sid} vanished"))?;

        let state = session.state.read().await.clone();
        let history = state.provider_messages();
        let working_dir = state.working_dir;
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

        let handle = self.spawn_agent_turn(AgentTurnInput {
            sid: sid.clone(),
            turn_id: turn_id.clone(),
            working_dir,
            tool_registry,
            system_prompt,
            history,
            text,
        });
        self.active_turn = Some(ActiveTurn {
            session_id: sid,
            turn_id,
            handle,
        });
        Ok(())
    }

    /// 中止当前活跃的回合，取消后台任务并记录完成事件。
    async fn abort_active_turn(&mut self) -> Result<(), String> {
        let Some(active_turn) = self.active_turn.take() else {
            self.send_error(40400, "No active turn");
            return Ok(());
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
        if self.active_turn.is_some() {
            self.send_error(40900, "Cannot compact while a turn is running");
            return Ok(());
        }

        let Some(sid) = self.active_session_id.clone() else {
            self.send_error(40400, "No active session");
            return Ok(());
        };
        let session = self
            .runtime
            .session_manager
            .get(&sid)
            .await
            .ok_or_else(|| format!("Session {sid} vanished"))?;
        let state = session.state.read().await.clone();
        let tool_registry = self.ensure_tool_registry(&sid, &state.working_dir).await;
        let compact_runner = CompactForkRunner::new(
            Arc::clone(&self.runtime.llm_provider),
            tool_registry.list_definitions(),
        );
        let compaction = match self
            .runtime
            .context_assembler
            .compact_provider_messages_with_compact_runner(
                &compact_runner,
                state.provider_messages(),
                state.system_prompt.as_deref(),
                CompactPromptStyle::ForkedConversationPrompt,
            )
            .await
        {
            Ok(prepared) => prepared
                .compaction
                .expect("compact_provider_messages should include compaction"),
            Err(CompactError::Skip(
                CompactSkipReason::Empty | CompactSkipReason::NothingToCompact,
            )) => {
                self.send_error(40000, "Nothing to compact");
                return Ok(());
            },
            Err(error) => {
                self.send_error(-32603, &format!("Compaction failed: {error}"));
                return Ok(());
            },
        };

        self.record_and_broadcast(&sid, None, EventPayload::CompactionStarted)
            .await?;
        self.record_and_broadcast(&sid, None, compaction_applied_payload(&compaction))
            .await?;
        self.record_and_broadcast(
            &sid,
            None,
            EventPayload::CompactionCompleted {
                pre_tokens: compaction.pre_tokens,
                post_tokens: compaction.post_tokens,
                summary: compaction.summary,
            },
        )
        .await?;
        Ok(())
    }

    /// 清理已完成的活跃回合引用，在每次处理新命令前调用。
    fn clear_finished_turn(&mut self) {
        if self
            .active_turn
            .as_ref()
            .is_some_and(|active_turn| active_turn.handle.is_finished())
        {
            self.active_turn = None;
        }
    }

    /// 恢复或切换到指定会话，从磁盘重放事件并构建快照发送给客户端。
    async fn resume_session(&mut self, session_id: SessionId) {
        match self.runtime.session_manager.resume(&session_id).await {
            Ok(session) => {
                let state = session.state.read().await;
                let working_dir = state.working_dir.clone();
                let needs_prompt = state.system_prompt.is_none();
                let snapshot = SessionSnapshot {
                    session_id: session_id.clone(),
                    cursor: String::new(),
                    messages: state.messages.iter().map(message_to_dto).collect(),
                    model_id: state.model_id.clone(),
                    working_dir: working_dir.clone(),
                };
                drop(state);

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
            self.runtime.context_assembler.as_ref(),
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
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            let AgentTurnInput {
                sid,
                turn_id,
                working_dir,
                tool_registry,
                system_prompt,
                history,
                text,
            } = input;

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
                },
            );

            let (output, emitted_error) = drive_agent(&agent, &text, history, |payload| {
                let runtime = runtime.clone();
                let event_tx = event_tx.clone();
                let sid = sid.clone();
                let turn_id = turn_id.clone();
                async move {
                    let _ =
                        record_and_broadcast(&runtime, &event_tx, &sid, Some(&turn_id), payload)
                            .await;
                }
            })
            .await;

            match output {
                Ok(output) => {
                    let _ = record_and_broadcast(
                        &runtime,
                        &event_tx,
                        &sid,
                        Some(&turn_id),
                        EventPayload::TurnCompleted {
                            finish_reason: output.finish_reason.clone(),
                        },
                    )
                    .await;
                    let _ = record_and_broadcast(
                        &runtime,
                        &event_tx,
                        &sid,
                        Some(&turn_id),
                        EventPayload::AgentRunCompleted {
                            reason: output.finish_reason,
                        },
                    )
                    .await;
                },
                Err(e) => {
                    if !emitted_error {
                        let _ = record_and_broadcast(
                            &runtime,
                            &event_tx,
                            &sid,
                            Some(&turn_id),
                            EventPayload::ErrorOccurred {
                                code: -32603,
                                message: e.to_string(),
                                recoverable: false,
                            },
                        )
                        .await;
                    }
                    let _ = record_and_broadcast(
                        &runtime,
                        &event_tx,
                        &sid,
                        Some(&turn_id),
                        EventPayload::TurnCompleted {
                            finish_reason: "error".into(),
                        },
                    )
                    .await;
                    let _ = record_and_broadcast(
                        &runtime,
                        &event_tx,
                        &sid,
                        Some(&turn_id),
                        EventPayload::AgentRunCompleted {
                            reason: "error".into(),
                        },
                    )
                    .await;
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

    use astrcode_context::manager::LlmContextAssembler;
    use astrcode_core::{
        config::{EffectiveConfig, LlmSettings, OpenAiApiMode},
        event::EventPayload,
        llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
        tool::ToolDefinition,
    };
    use astrcode_protocol::{commands::ClientCommand, events::ClientNotification};
    use astrcode_storage::noop::NoopEventStore;
    use tokio::sync::mpsc;

    use super::*;
    use crate::session::{COMPACTION_APPLIED_EVENT, SessionManager};

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
            session_manager: Arc::new(SessionManager::new(Arc::new(NoopEventStore::new()))),
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

    async fn drain_until_compaction_completed(
        event_rx: &mut broadcast::Receiver<ClientNotification>,
    ) {
        loop {
            let notification = recv_event(event_rx).await;
            let ClientNotification::Event(event) = notification else {
                continue;
            };
            if matches!(event.payload, EventPayload::CompactionCompleted { .. }) {
                return;
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

    #[tokio::test]
    async fn create_session_configures_system_prompt() {
        let runtime = test_runtime();
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);
        let mut handler = CommandHandler::new(Arc::clone(&runtime), event_tx);

        handler
            .handle(ClientCommand::CreateSession {
                working_dir: ".".into(),
            })
            .await
            .unwrap();

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

        let sid = handler.active_session_id.clone().unwrap();
        let session = runtime.session_manager.get(&sid).await.unwrap();
        let state = session.state.read().await;
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
        let mut handler = CommandHandler::new(Arc::clone(&runtime), event_tx);

        handler
            .handle(ClientCommand::CreateSession {
                working_dir: ".".into(),
            })
            .await
            .unwrap();
        let sid = handler.active_session_id.clone().unwrap();
        let initial_prompt = {
            let session = runtime.session_manager.get(&sid).await.unwrap();
            let state = session.state.read().await;
            state.system_prompt.clone()
        };

        handler
            .handle(ClientCommand::SubmitPrompt {
                text: "one".into(),
                attachments: vec![],
            })
            .await
            .unwrap();
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");

        handler
            .handle(ClientCommand::SubmitPrompt {
                text: "two".into(),
                attachments: vec![],
            })
            .await
            .unwrap();
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");

        let session = runtime.session_manager.get(&sid).await.unwrap();
        let state = session.state.read().await;
        assert_eq!(state.system_prompt, initial_prompt);
    }

    #[tokio::test]
    async fn submit_prompt_backfills_legacy_session_system_prompt() {
        let runtime = test_runtime();
        let start_event = runtime
            .session_manager
            .create(".", "mock-model", 2048, None)
            .await
            .unwrap();
        let sid = start_event.session_id.clone();
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(128);
        let mut handler = CommandHandler::new(Arc::clone(&runtime), event_tx);
        handler.active_session_id = Some(sid.clone());

        handler
            .handle(ClientCommand::SubmitPrompt {
                text: "hello".into(),
                attachments: vec![],
            })
            .await
            .unwrap();
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");

        let session = runtime.session_manager.get(&sid).await.unwrap();
        let state = session.state.read().await;
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
        let mut handler = CommandHandler::new(test_runtime(), event_tx);

        handler
            .handle(ClientCommand::CreateSession {
                working_dir: ".".into(),
            })
            .await
            .unwrap();
        handler
            .handle(ClientCommand::SubmitPrompt {
                text: "hi".into(),
                attachments: vec![],
            })
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
        let mut handler =
            CommandHandler::new(test_runtime_with_llm(Arc::new(PendingLlm)), event_tx);

        handler
            .handle(ClientCommand::CreateSession {
                working_dir: ".".into(),
            })
            .await
            .unwrap();
        handler
            .handle(ClientCommand::SubmitPrompt {
                text: "first".into(),
                attachments: vec![],
            })
            .await
            .unwrap();
        handler
            .handle(ClientCommand::SubmitPrompt {
                text: "second".into(),
                attachments: vec![],
            })
            .await
            .unwrap();

        let mut saw_busy = false;
        while let Ok(notification) = event_rx.try_recv() {
            if let ClientNotification::Error { code: 40900, .. } = notification {
                saw_busy = true;
                break;
            }
        }
        assert!(saw_busy, "second prompt should be rejected while turn runs");

        handler.handle(ClientCommand::Abort).await.unwrap();
    }

    #[tokio::test]
    async fn abort_stops_active_turn_and_records_completion() {
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(64);
        let mut handler =
            CommandHandler::new(test_runtime_with_llm(Arc::new(PendingLlm)), event_tx);

        handler
            .handle(ClientCommand::CreateSession {
                working_dir: ".".into(),
            })
            .await
            .unwrap();
        handler
            .handle(ClientCommand::SubmitPrompt {
                text: "keep running".into(),
                attachments: vec![],
            })
            .await
            .unwrap();
        assert!(handler.active_turn.is_some());

        handler.handle(ClientCommand::Abort).await.unwrap();

        assert!(handler.active_turn.is_none());
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "aborted");
    }

    #[tokio::test]
    async fn compact_command_rewrites_provider_history_without_exposing_summary() {
        let settings = astrcode_context::settings::ContextWindowSettings::default();
        let runtime = test_runtime_with_settings(Arc::new(MockLlm), settings);
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(256);
        let mut handler = CommandHandler::new(Arc::clone(&runtime), event_tx);

        handler
            .handle(ClientCommand::CreateSession {
                working_dir: ".".into(),
            })
            .await
            .unwrap();
        for text in ["one", "two", "three"] {
            handler
                .handle(ClientCommand::SubmitPrompt {
                    text: text.into(),
                    attachments: vec![],
                })
                .await
                .unwrap();
            assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
        }

        handler.handle(ClientCommand::Compact).await.unwrap();

        let mut saw_applied = false;
        let mut saw_completed = false;
        while !saw_completed {
            let notification = recv_event(&mut event_rx).await;
            let ClientNotification::Event(event) = notification else {
                continue;
            };
            match event.payload {
                EventPayload::Custom { name, .. } if name == COMPACTION_APPLIED_EVENT => {
                    saw_applied = true;
                },
                EventPayload::CompactionCompleted { .. } => saw_completed = true,
                _ => {},
            }
        }
        assert!(saw_applied);

        let sid = handler.active_session_id.clone().unwrap();
        let session = runtime.session_manager.get(&sid).await.unwrap();
        let state = session.state.read().await;
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
        let mut handler = CommandHandler::new(Arc::clone(&runtime), event_tx);

        handler
            .handle(ClientCommand::CreateSession {
                working_dir: ".".into(),
            })
            .await
            .unwrap();
        for text in ["one", "two", "three", "four"] {
            handler
                .handle(ClientCommand::SubmitPrompt {
                    text: text.into(),
                    attachments: vec![],
                })
                .await
                .unwrap();
            assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
        }

        handler.handle(ClientCommand::Compact).await.unwrap();
        drain_until_compaction_completed(&mut event_rx).await;
        let sid = handler.active_session_id.clone().unwrap();
        let session = runtime.session_manager.get(&sid).await.unwrap();
        let first_summary = {
            let state = session.state.read().await;
            message_to_dto(&state.context_messages[0]).content
        };

        handler
            .handle(ClientCommand::SubmitPrompt {
                text: "five".into(),
                attachments: vec![],
            })
            .await
            .unwrap();
        assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
        handler.handle(ClientCommand::Compact).await.unwrap();
        drain_until_compaction_completed(&mut event_rx).await;

        let state = session.state.read().await;
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
    async fn compact_command_does_not_fallback_when_summary_is_invalid() {
        let runtime = test_runtime_with_llm(Arc::new(InvalidSummaryLlm));
        let (event_tx, mut event_rx) = tokio::sync::broadcast::channel(256);
        let mut handler = CommandHandler::new(Arc::clone(&runtime), event_tx);

        handler
            .handle(ClientCommand::CreateSession {
                working_dir: ".".into(),
            })
            .await
            .unwrap();
        for text in ["one", "two", "three"] {
            handler
                .handle(ClientCommand::SubmitPrompt {
                    text: text.into(),
                    attachments: vec![],
                })
                .await
                .unwrap();
            assert_eq!(wait_for_turn_completed(&mut event_rx).await, "stop");
        }

        handler.handle(ClientCommand::Compact).await.unwrap();

        let sid = handler.active_session_id.clone().unwrap();
        let session = runtime.session_manager.get(&sid).await.unwrap();
        let state = session.state.read().await;
        assert!(state.context_messages.is_empty());
    }
}
