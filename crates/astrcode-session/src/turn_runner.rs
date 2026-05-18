//! Agent loop — 临时回合处理器与回合驱动。
//!
//! 负责处理一轮完整的对话：调用 LLM、执行工具调用、
//! 分发扩展钩子事件，并将事件流式传输给客户端。
//! Agent 是无状态的短暂对象，处理完一个回合后即被丢弃。
//! `drive_agent` 负责在回合执行时转发事件流并等待最终输出。

use std::sync::Arc;

use astrcode_context::context_assembler::ContextPrepareInput;
use astrcode_core::{
    event::{Event, EventPayload},
    extension::{CompactTrigger, ExtensionEvent, ProviderEvent, ProviderResult},
    llm::{LlmContent, LlmEvent, LlmMessage, LlmRole},
    tool::ToolDefinition,
    types::*,
};
use tokio::sync::mpsc;

use crate::{
    compact::{CompactHookContext, collect_compact_instructions, dispatch_post_compact},
    llm_stream::{
        StreamOutcome, assistant_message_with_thinking, consume_llm_stream,
        non_empty_reasoning_content, provider_visible_messages,
    },
    mcp_visibility::{
        activate_discovered_mcp_tools, append_deferred_mcp_tools_reminder, clone_tools_by_index,
        provider_visible_tool_indexes,
    },
    session::Session,
    tool_pipeline::ToolPipeline,
    tool_types::ExecuteToolCalls,
    turn_context::{
        AgentSignal, EventSink, SharedTurnContext, TurnError, end_turn_with_error_typed, send_event,
    },
};

/// 运行 agent 的一次 process_prompt，通过 select! + drain 实时处理事件。
///
/// 每个事件先经 `Session::emit` 写 store + fanout 到 runtime 广播，再可选地
/// 调用 `sink.on_event(&event)` 做副作用（lossless，例如子 agent 的进度桥）。
/// 返回 `(output, emitted_error)`。
pub async fn drive_agent(
    agent: &mut TurnRunner,
    user_text: &str,
    turn_id: &TurnId,
    sink: Option<&dyn EventSink>,
) -> (Result<TurnOutput, TurnError>, bool) {
    let session = Arc::clone(&agent.session);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let agent_future = agent.process_prompt(user_text, Some(event_tx));
    tokio::pin!(agent_future);

    let mut emitted_error = false;
    let mut events_closed = false;

    let output = loop {
        tokio::select! {
            result = &mut agent_future => break result,
            payload = event_rx.recv(), if !events_closed => {
                match payload {
                    Some(AgentSignal::Event(payload)) => {
                        if matches!(payload, EventPayload::ErrorOccurred { .. }) {
                            emitted_error = true;
                        }
                        dispatch_agent_event(&session, turn_id, sink, payload).await;
                    },
                    None => events_closed = true,
                }
            },
        }
    };

    while let Some(AgentSignal::Event(payload)) = event_rx.recv().await {
        if matches!(payload, EventPayload::ErrorOccurred { .. }) {
            emitted_error = true;
        }
        dispatch_agent_event(&session, turn_id, sink, payload).await;
    }

    (output, emitted_error)
}

/// 把一个 turn 内事件写入 session（持久化 + fanout），并通知可选 sink。
async fn dispatch_agent_event(
    session: &Session,
    turn_id: &TurnId,
    sink: Option<&dyn EventSink>,
    payload: EventPayload,
) {
    if let Some(sink) = sink {
        // 给 sink 一份事件副本——它不关心 seq，只需 payload 用于翻译进度。
        let preview = Event::new(session.id().clone(), Some(turn_id.clone()), payload.clone());
        sink.on_event(&preview).await;
    }
    session.emit(Some(turn_id), payload).await;
}

/// AgentTurn — 一个临时的回合处理器。
///
/// 字段语义：
/// - `session`: 持有运行 turn 所需的全部依赖（store / runtime / caps / event_tx）。 `caps()`
///   提供按需拉取的 LLM provider / extension runner / context assembler。
/// - `system_prompt`: 当前 turn 起始时的 system prompt。turn 内若 session 的 prompt
///   被外部刷新（例如扩展注册新 skill），下一轮 LLM 调用前会从 `session.current_system_prompt`
///   重新读取。
/// - `initial_history`: 启动时 session_state 的 `provider_messages()`
///   快照；首轮循环消费一次后置空。
/// - `shared`: 缓存 turn 期间不变的标识三元组，避免反复 clone。
/// - `tools`: 工具调度管线。
pub struct TurnRunner {
    session: Arc<Session>,
    shared: SharedTurnContext,
    system_prompt: String,
    initial_history: Vec<LlmMessage>,
    tools: ToolPipeline,
}

impl TurnRunner {
    /// 创建一个新的 TurnRunner 实例。
    ///
    /// `session_state` 由调用方提前读取并传入，避免重复 I/O。
    pub fn new(
        session: Arc<Session>,
        session_state: &astrcode_core::storage::SessionReadModel,
        background_result_tx: Option<
            mpsc::UnboundedSender<crate::background::BackgroundTaskCompletion>,
        >,
    ) -> Result<Self, TurnError> {
        let shared = SharedTurnContext {
            session_id: session.id().clone(),
            working_dir: session_state.working_dir.clone(),
            model_id: session_state.model_id.clone(),
        };
        let system_prompt = session_state.system_prompt.clone().unwrap_or_default();
        let initial_history = session_state.provider_messages();
        let runtime = Arc::clone(session.runtime());
        let caps = Arc::clone(session.caps());

        let background_task_reader: Option<Arc<dyn astrcode_core::tool::BackgroundTaskReader>> =
            Some(Arc::new(crate::background::BackgroundTaskReaderImpl::new(
                runtime.background_tasks(),
            )));
        let capabilities = crate::tool_exec::ToolRuntimeCapabilities {
            background_result_tx,
            background_tasks: runtime.background_tasks(),
            background_task_reader,
            file_observation_store: Some(runtime.file_observation_store()),
        };
        let tools = ToolPipeline::new(
            shared.clone(),
            runtime.tool_registry(),
            Arc::clone(caps.extension_runner()),
            Arc::clone(&session),
            capabilities,
        );
        Ok(Self {
            session,
            shared,
            system_prompt,
            initial_history,
            tools,
        })
    }

    /// 处理用户输入的完整 Agent 循环。
    pub(crate) async fn process_prompt(
        &mut self,
        user_text: &str,
        event_tx: Option<mpsc::UnboundedSender<AgentSignal>>,
    ) -> Result<TurnOutput, TurnError> {
        let all_tools = self.tools.list_definitions();
        let mut active_mcp_tools = std::collections::HashSet::new();
        let mut tool_indexes = provider_visible_tool_indexes(&all_tools, &active_mcp_tools);
        let mut tools = clone_tools_by_index(&all_tools, &tool_indexes);

        let extension_runner = Arc::clone(self.session.caps().extension_runner());
        let context_assembler = Arc::clone(self.session.caps().context_assembler());

        let lifecycle_ctx = self.shared.lifecycle_ctx();
        let (turn_start_res, prompt_submit_res) = tokio::join!(
            extension_runner.emit_lifecycle(ExtensionEvent::TurnStart, lifecycle_ctx.clone()),
            extension_runner
                .emit_lifecycle(ExtensionEvent::UserPromptSubmit, lifecycle_ctx.clone()),
        );
        turn_start_res?;
        if let Err(e) = prompt_submit_res {
            return end_turn_with_error_typed(&extension_runner, &self.shared, e).await;
        }

        // 用启动时缓存的初始历史构建 messages，避免在 process_prompt 入口又读一次 session。
        let initial_history = std::mem::take(&mut self.initial_history);
        let mut messages = Vec::with_capacity(initial_history.len() + 2);
        if !self.system_prompt.trim().is_empty() {
            messages.push(LlmMessage::system(&self.system_prompt));
        }
        messages.extend(
            initial_history
                .into_iter()
                .filter(|message| message.role != LlmRole::System),
        );
        messages.push(LlmMessage::user(user_text));

        let mut final_text = String::new();
        let mut all_tool_results: Vec<astrcode_core::tool::ToolResult> = Vec::new();

        loop {
            // 动态刷新 system_prompt（扩展可能注册新 skill/tool）
            if let Some(prompt) = self
                .session
                .current_system_prompt()
                .await
                .map_err(|e| TurnError::Internal(e.to_string()))?
            {
                if prompt != self.system_prompt {
                    tracing::info!(session_id = %self.shared.session_id, "system_prompt changed mid-turn, refreshing");
                    self.system_prompt = prompt;
                    if let Some(msg) = messages.iter_mut().find(|m| m.role == LlmRole::System) {
                        msg.content = vec![LlmContent::Text {
                            text: self.system_prompt.clone(),
                        }];
                    }
                }
            }

            // 每轮重新拉 llm 快照，跟随 ConfigManager 热更新
            let llm = self.session.caps().llm();

            // 收集插件 compact 指令
            let custom_instructions = collect_compact_instructions(
                &extension_runner,
                CompactHookContext {
                    session_id: self.shared.session_id.as_str(),
                    working_dir: &self.shared.working_dir,
                    model_id: &self.shared.model_id,
                    trigger: CompactTrigger::AutoThreshold,
                    message_count: messages.len(),
                },
            )
            .await
            .unwrap_or_default();

            // 上下文准备：context assembler 内部处理阈值检查、LLM compact 和 deterministic
            // fallback。
            let (system_messages, visible_messages): (Vec<_>, Vec<_>) = messages
                .iter()
                .cloned()
                .partition(|message| message.role == LlmRole::System);
            let input = ContextPrepareInput {
                messages: visible_messages,
                system_prompt: Some(&self.system_prompt),
                model_limits: llm.model_limits(),
                custom_instructions,
            };
            let request_fn = crate::compact::make_compact_request_fn(Arc::clone(&llm));
            let mut prepared = context_assembler
                .prepare_messages_with_llm(input, request_fn)
                .await;

            if let Some(ref mut compaction) = prepared.compaction {
                send_event(event_tx.as_ref(), EventPayload::CompactionStarted);
                crate::post_compact::enrich_post_compact_context(
                    compaction,
                    self.shared.session_id.as_str(),
                    &messages,
                    &self.shared.working_dir,
                    Some(&self.system_prompt),
                    &tools,
                    context_assembler.settings(),
                )
                .await;
                let hook_ctx = CompactHookContext {
                    session_id: self.shared.session_id.as_str(),
                    working_dir: &self.shared.working_dir,
                    model_id: &self.shared.model_id,
                    trigger: CompactTrigger::AutoThreshold,
                    message_count: messages.len(),
                };
                if let Err(e) = dispatch_post_compact(&extension_runner, hook_ctx, compaction).await
                {
                    tracing::warn!(error = %e, "PostCompact extension dispatch failed");
                }
            }

            let mut context_messages = prepared.messages;
            append_deferred_mcp_tools_reminder(
                &mut context_messages,
                &all_tools,
                &active_mcp_tools,
            );

            let send_messages = self
                .apply_before_provider_request_hook(
                    &extension_runner,
                    system_messages,
                    context_messages,
                )
                .await?;

            let rx = self
                .start_provider_stream(&llm, &extension_runner, send_messages, &tools, &event_tx)
                .await?;
            let message_id = new_message_id();

            let outcome = match consume_llm_stream(rx, &event_tx, message_id).await {
                Ok(outcome) => outcome,
                Err(error) => {
                    return end_turn_with_error_typed(&extension_runner, &self.shared, error).await;
                },
            };

            match outcome {
                StreamOutcome::Complete {
                    text,
                    reasoning_content,
                    finish_reason,
                    message_id,
                    message_started,
                } => {
                    let reasoning_content = non_empty_reasoning_content(reasoning_content);
                    if !text.is_empty() || reasoning_content.is_some() {
                        messages.push(assistant_message_with_thinking(
                            &text,
                            reasoning_content.clone(),
                        ));
                        final_text.push_str(&text);
                        if message_started {
                            send_event(
                                event_tx.as_ref(),
                                EventPayload::AssistantMessageCompleted {
                                    message_id,
                                    text,
                                    reasoning_content,
                                },
                            );
                        }
                    }
                    self.dispatch_after_provider_response(&extension_runner)
                        .await?;
                    extension_runner
                        .emit_lifecycle(ExtensionEvent::TurnEnd, lifecycle_ctx.clone())
                        .await?;
                    return Ok(TurnOutput {
                        text: final_text,
                        finish_reason,
                        tool_results: all_tool_results,
                    });
                },
                StreamOutcome::ToolCalls {
                    text,
                    reasoning_content,
                    tool_calls,
                    message_id,
                    message_started,
                } => {
                    let reasoning_content = non_empty_reasoning_content(reasoning_content);
                    let visible_text = text.as_deref().unwrap_or_default();
                    if !visible_text.is_empty() {
                        final_text.push_str(visible_text);
                    }
                    if message_started {
                        send_event(
                            event_tx.as_ref(),
                            EventPayload::AssistantMessageCompleted {
                                message_id,
                                text: visible_text.to_string(),
                                reasoning_content: reasoning_content.clone(),
                            },
                        );
                    }

                    self.dispatch_after_provider_response(&extension_runner)
                        .await?;

                    let prepared_tool_calls = match self
                        .tools
                        .prepare_tool_calls(&tool_calls, &tools, &event_tx)
                        .await
                    {
                        Ok(prepared_tool_calls) => prepared_tool_calls,
                        Err(error) => {
                            return end_turn_with_error_typed(
                                &extension_runner,
                                &self.shared,
                                error,
                            )
                            .await;
                        },
                    };
                    messages.push(assistant_tool_call_message(
                        &prepared_tool_calls,
                        visible_text,
                        reasoning_content,
                    ));
                    let discovered_tools = match self
                        .tools
                        .execute_and_commit(ExecuteToolCalls {
                            prepared: &prepared_tool_calls,
                            tools: &tools,
                            messages: &mut messages,
                            all_tool_results: &mut all_tool_results,
                            event_tx: &event_tx,
                        })
                        .await
                    {
                        Ok(discovered_tools) => discovered_tools,
                        Err(error) => {
                            return end_turn_with_error_typed(
                                &extension_runner,
                                &self.shared,
                                error,
                            )
                            .await;
                        },
                    };
                    if activate_discovered_mcp_tools(
                        &mut active_mcp_tools,
                        &all_tools,
                        discovered_tools,
                    ) {
                        tool_indexes = provider_visible_tool_indexes(&all_tools, &active_mcp_tools);
                        tools = clone_tools_by_index(&all_tools, &tool_indexes);
                    }
                },
            }
        }
    }

    async fn apply_before_provider_request_hook(
        &self,
        extension_runner: &astrcode_extensions::runner::ExtensionRunner,
        system_messages: Vec<LlmMessage>,
        context_messages: Vec<LlmMessage>,
    ) -> Result<Vec<LlmMessage>, TurnError> {
        let send_messages = provider_visible_messages([system_messages, context_messages].concat());
        match extension_runner
            .emit_provider(
                ProviderEvent::BeforeRequest,
                self.shared.provider_ctx(send_messages.clone()),
            )
            .await?
        {
            ProviderResult::Block { reason } => {
                extension_runner
                    .emit_lifecycle(ExtensionEvent::TurnEnd, self.shared.lifecycle_ctx())
                    .await?;
                Err(TurnError::Internal(reason))
            },
            ProviderResult::ReplaceMessages { messages } => Ok(provider_visible_messages(messages)),
            ProviderResult::AppendMessages { messages } => {
                let mut combined = send_messages;
                combined.extend(messages);
                Ok(provider_visible_messages(combined))
            },
            ProviderResult::Allow => Ok(send_messages),
        }
    }

    async fn start_provider_stream(
        &self,
        llm: &Arc<dyn astrcode_core::llm::LlmProvider>,
        extension_runner: &astrcode_extensions::runner::ExtensionRunner,
        send_messages: Vec<LlmMessage>,
        tools: &[ToolDefinition],
        event_tx: &Option<mpsc::UnboundedSender<AgentSignal>>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, TurnError> {
        match llm.generate(send_messages, tools.to_vec()).await {
            Ok(rx) => Ok(rx),
            Err(e) => {
                send_event(
                    event_tx.as_ref(),
                    EventPayload::ErrorOccurred {
                        code: -32603,
                        message: e.to_string(),
                        recoverable: false,
                    },
                );
                end_turn_with_error_typed(extension_runner, &self.shared, e).await
            },
        }
    }

    async fn dispatch_after_provider_response(
        &self,
        extension_runner: &astrcode_extensions::runner::ExtensionRunner,
    ) -> Result<(), TurnError> {
        if let Err(e) = extension_runner
            .emit_lifecycle(
                ExtensionEvent::AfterProviderResponse,
                self.shared.lifecycle_ctx(),
            )
            .await
        {
            return end_turn_with_error_typed(extension_runner, &self.shared, e).await;
        }
        Ok(())
    }
}

/// Agent 回合的输出结果。
#[derive(Debug)]
pub struct TurnOutput {
    pub text: String,
    pub finish_reason: String,
    pub tool_results: Vec<astrcode_core::tool::ToolResult>,
}

// ─── run_turn: 统一的回合执行入口 ──────────────────────────────────────

/// `run_turn` 的返回结果。
pub struct RunTurnResult {
    pub output: Result<TurnOutput, TurnError>,
    pub emitted_error: bool,
}

/// 执行一轮完整的 agent turn。
///
/// 封装 `drive_agent` 调用。所有事件通过 `Session::emit` 持久化 + fanout，
/// 可选 `sink` 收到副本（lossless）。
pub async fn run_turn(
    agent: &mut TurnRunner,
    user_text: &str,
    turn_id: &TurnId,
    sink: Option<&dyn EventSink>,
) -> RunTurnResult {
    let (output, emitted_error) = drive_agent(agent, user_text, turn_id, sink).await;

    RunTurnResult {
        output,
        emitted_error,
    }
}

// ─── Message construction helpers ────────────────────────────────────────

fn assistant_tool_call_message(
    prepared: &[crate::tool_types::PreparedToolCall],
    text: &str,
    reasoning_content: Option<String>,
) -> LlmMessage {
    let mut content = Vec::with_capacity(prepared.len() + usize::from(!text.is_empty()));
    if !text.is_empty() {
        content.push(LlmContent::Text {
            text: text.to_string(),
        });
    }
    content.extend(prepared.iter().map(|call| LlmContent::ToolCall {
        call_id: call.call_id.clone(),
        name: call.name.clone(),
        arguments: call.tool_input.clone(),
    }));

    LlmMessage {
        role: LlmRole::Assistant,
        content,
        name: None,
        reasoning_content,
    }
}
