//! Agent loop — 临时回合处理器与回合驱动。
//!
//! 负责处理一轮完整的对话：调用 LLM、执行工具调用、
//! 分发扩展钩子事件，并将事件流式传输给客户端。
//! Agent 是无状态的短暂对象，处理完一个回合后即被丢弃。
//! `drive_agent` 负责在回合执行时转发事件流并等待最终输出。

use std::{future::Future, sync::Arc};

use astrcode_context::context_engine::{ContextPrepareInput, LlmContextAssembler};
use astrcode_core::{
    config::ModelSelection,
    event::EventPayload,
    extension::{ExtensionEvent, LifecycleContext, ProviderContext, ProviderEvent, ProviderResult},
    llm::{LlmEvent, LlmMessage, LlmProvider, LlmRole},
    tool::{BackgroundTaskReader, FileObservationStore, ToolDefinition},
    types::*,
};
use astrcode_extensions::runner::ExtensionRunner;
use tokio::sync::mpsc;

use crate::{
    llm_stream::{
        StreamOutcome, assistant_message_with_thinking, consume_llm_stream,
        non_empty_reasoning_content, provider_visible_messages,
    },
    tool_exec::InMemoryFileObservationStore,
    tool_pipeline::ToolPipeline,
    tool_types::{ExecuteToolCalls, assistant_tool_call_message},
    turn_context::{
        AgentSignal, EventBus, SharedTurnContext, TurnError, end_turn_with_error_typed, send_event,
    },
    util::{
        activate_discovered_mcp_tools, append_deferred_mcp_tools_reminder, clone_tools_by_index,
        provider_visible_tool_indexes,
    },
};

/// 运行 agent 的一次 process_prompt，通过 select! + drain 实时处理事件。
///
/// `on_signal` 在每个事件或控制信号到达时被调用（包含 select 阶段和 drain 阶段）。
/// 返回 `(output, emitted_error)`。
pub async fn drive_agent<F, Fut>(
    agent: &TurnRunner,
    user_text: &str,
    transient_instructions: Option<String>,
    event_bus: &dyn EventBus,
    mut on_signal: F,
) -> (Result<TurnOutput, TurnError>, bool)
where
    F: FnMut(AgentSignal) -> Fut,
    Fut: Future<Output = ()>,
{
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let agent_future = agent.process_prompt(user_text, transient_instructions, Some(event_tx));
    tokio::pin!(agent_future);

    let mut emitted_error = false;
    let mut events_closed = false;
    let output = loop {
        tokio::select! {
            result = &mut agent_future => break result,
            payload = event_rx.recv(), if !events_closed => {
                match payload {
                    Some(signal) => {
                        let AgentSignal::Event(ref payload) = signal;
                        if matches!(payload, EventPayload::ErrorOccurred { .. }) {
                            emitted_error = true;
                        }
                        event_bus.emit(&agent.shared.session_id, payload.clone()).await;
                        on_signal(signal).await;
                    },
                    None => events_closed = true,
                }
            },
        }
    };

    while let Some(signal) = event_rx.recv().await {
        let AgentSignal::Event(ref payload) = signal;
        if matches!(payload, EventPayload::ErrorOccurred { .. }) {
            emitted_error = true;
        }
        event_bus
            .emit(&agent.shared.session_id, payload.clone())
            .await;
        on_signal(signal).await;
    }

    (output, emitted_error)
}

/// Agent — a transient turn processor.
///
/// Created from a session projection, processes one turn, emits event payloads,
/// and is discarded. Durable event persistence stays in the handler; compact
/// transcript snapshots are written through the injected session manager.
pub struct TurnRunner {
    session: Arc<crate::session::Session>,
    system_prompt: String,
    shared: SharedTurnContext,
    llm: Arc<dyn LlmProvider>,
    extension_runner: Arc<ExtensionRunner>,
    tools: ToolPipeline,
    context_assembler: Arc<LlmContextAssembler>,
}

impl TurnRunner {
    /// 创建一个新的 TurnRunner 实例。
    ///
    /// 从 `services.session` 读取所有事实：`working_dir`、`model_id`、`system_prompt`。
    pub async fn new(
        services: crate::session_services::SessionServices,
    ) -> Result<Self, TurnError> {
        let state = services
            .session
            .read_model()
            .await
            .map_err(|e| TurnError::Internal(e.to_string()))?;
        let shared = SharedTurnContext {
            session_id: services.session.id().clone(),
            working_dir: state.working_dir,
            model_id: state.model_id,
        };
        let system_prompt = state.system_prompt.unwrap_or_default();
        let background_task_reader: Option<Arc<dyn BackgroundTaskReader>> = Some(Arc::new(
            crate::background::BackgroundTaskReaderImpl::new(services.background_tasks.clone()),
        ));
        let file_observation_store: Option<Arc<dyn FileObservationStore>> =
            Some(Arc::new(InMemoryFileObservationStore::default()));
        let capabilities = crate::tool_types::ToolRuntimeCapabilities {
            background_result_tx: services.background_result_tx,
            background_tasks: services.background_tasks,
            background_task_reader,
            file_observation_store,
            agent_session_control: services.agent_session_control,
        };
        let tools = ToolPipeline::new(
            shared.clone(),
            services.tool_registry,
            services.extension_runner.clone(),
            services.session.clone(),
            capabilities,
        );
        Ok(Self {
            session: services.session,
            system_prompt,
            shared,
            llm: services.llm,
            extension_runner: services.extension_runner,
            tools,
            context_assembler: services.context_assembler,
        })
    }

    /// 处理用户输入的完整 Agent 循环。
    pub(crate) async fn process_prompt(
        &self,
        user_text: &str,
        transient_instructions: Option<String>,
        event_tx: Option<mpsc::UnboundedSender<AgentSignal>>,
    ) -> Result<TurnOutput, TurnError> {
        let all_tools = self.tools.list_definitions();
        let mut active_mcp_tools = std::collections::HashSet::new();
        let mut tool_indexes = provider_visible_tool_indexes(&all_tools, &active_mcp_tools);
        let mut tools = clone_tools_by_index(&all_tools, &tool_indexes);

        let lifecycle_ctx = LifecycleContext {
            session_id: self.shared.session_id.to_string(),
            working_dir: self.shared.working_dir.clone(),
            model: ModelSelection::simple(self.shared.model_id.clone()),
        };
        self.extension_runner
            .emit_lifecycle(ExtensionEvent::TurnStart, lifecycle_ctx.clone())
            .await?;

        if let Err(e) = self
            .extension_runner
            .emit_lifecycle(ExtensionEvent::UserPromptSubmit, lifecycle_ctx.clone())
            .await
        {
            return end_turn_with_error_typed(&self.extension_runner, &self.shared, e).await;
        }

        // 从 session 读取 history
        let state = self
            .session
            .read_model()
            .await
            .map_err(|e| TurnError::Internal(e.to_string()))?;
        let history = state.provider_messages();

        // 合并 transient_instructions（斜杠命令注入，turn 级别）
        let effective_prompt = transient_instructions
            .filter(|i| !i.trim().is_empty())
            .map(|i| {
                format!(
                    "{}\n\n[Slash Command Instructions]\n{}",
                    self.system_prompt,
                    i.trim()
                )
            })
            .unwrap_or_else(|| self.system_prompt.clone());

        let mut messages = Vec::with_capacity(history.len() + 2);
        if !effective_prompt.trim().is_empty() {
            messages.push(LlmMessage::system(effective_prompt));
        }
        messages.extend(
            history
                .into_iter()
                .filter(|message| message.role != LlmRole::System),
        );
        messages.push(LlmMessage::user(user_text));

        let mut final_text = String::new();
        let mut all_tool_results: Vec<astrcode_core::tool::ToolResult> = Vec::new();

        loop {
            // 上下文准备：context assembler 内部处理阈值检查和 deterministic compact。
            let (system_messages, visible_messages): (Vec<_>, Vec<_>) = messages
                .iter()
                .cloned()
                .partition(|message| message.role == LlmRole::System);
            let input = ContextPrepareInput {
                messages: visible_messages,
                system_prompt: Some(&self.system_prompt),
                model_limits: self.llm.model_limits(),
            };
            let mut prepared = self.context_assembler.prepare_messages(input);

            if let Some(ref mut compaction) = prepared.compaction {
                send_event(&event_tx, EventPayload::CompactionStarted);
                crate::post_compact::enrich_post_compact_context(
                    compaction,
                    self.shared.session_id.as_str(),
                    &messages,
                    &self.shared.working_dir,
                    Some(&self.system_prompt),
                    &tools,
                    self.context_assembler.settings(),
                )
                .await;
            }

            let mut context_messages = prepared.messages;
            append_deferred_mcp_tools_reminder(
                &mut context_messages,
                &all_tools,
                &active_mcp_tools,
            );

            let send_messages = self
                .apply_before_provider_request_hook(system_messages, context_messages)
                .await?;

            let rx = self
                .start_provider_stream(send_messages, &tools, &event_tx)
                .await?;
            let message_id = new_message_id();

            let outcome = consume_llm_stream(rx, &event_tx, message_id).await?;

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
                                &event_tx,
                                EventPayload::AssistantMessageCompleted {
                                    message_id,
                                    text,
                                    reasoning_content,
                                },
                            );
                        }
                    }
                    self.extension_runner
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
                            &event_tx,
                            EventPayload::AssistantMessageCompleted {
                                message_id,
                                text: visible_text.to_string(),
                                reasoning_content: reasoning_content.clone(),
                            },
                        );
                    }

                    self.dispatch_after_provider_response(&lifecycle_ctx)
                        .await?;

                    let prepared_tool_calls = self
                        .tools
                        .prepare_tool_calls(&tool_calls, &tools, &event_tx)
                        .await?;
                    messages.push(assistant_tool_call_message(
                        &prepared_tool_calls,
                        visible_text,
                        reasoning_content,
                    ));
                    let discovered_tools = self
                        .tools
                        .execute_and_commit(ExecuteToolCalls {
                            prepared: &prepared_tool_calls,
                            tools: &tools,
                            messages: &mut messages,
                            all_tool_results: &mut all_tool_results,
                            event_tx: &event_tx,
                        })
                        .await?;
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
        system_messages: Vec<LlmMessage>,
        context_messages: Vec<LlmMessage>,
    ) -> Result<Vec<LlmMessage>, TurnError> {
        let send_messages = provider_visible_messages([system_messages, context_messages].concat());
        let provider_ctx = ProviderContext {
            session_id: self.shared.session_id.to_string(),
            working_dir: self.shared.working_dir.clone(),
            model: ModelSelection::simple(self.shared.model_id.clone()),
            messages: send_messages.clone(),
        };
        match self
            .extension_runner
            .emit_provider(ProviderEvent::BeforeRequest, provider_ctx)
            .await?
        {
            ProviderResult::Block { reason } => {
                let lifecycle_ctx = LifecycleContext {
                    session_id: self.shared.session_id.to_string(),
                    working_dir: self.shared.working_dir.clone(),
                    model: ModelSelection::simple(self.shared.model_id.clone()),
                };
                self.extension_runner
                    .emit_lifecycle(ExtensionEvent::TurnEnd, lifecycle_ctx)
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
        send_messages: Vec<LlmMessage>,
        tools: &[ToolDefinition],
        event_tx: &Option<mpsc::UnboundedSender<AgentSignal>>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, TurnError> {
        match self.llm.generate(send_messages, tools.to_vec()).await {
            Ok(rx) => Ok(rx),
            Err(e) => {
                send_event(
                    event_tx,
                    EventPayload::ErrorOccurred {
                        code: -32603,
                        message: e.to_string(),
                        recoverable: false,
                    },
                );
                end_turn_with_error_typed(&self.extension_runner, &self.shared, e).await
            },
        }
    }

    async fn dispatch_after_provider_response(
        &self,
        lifecycle_ctx: &LifecycleContext,
    ) -> Result<(), TurnError> {
        if let Err(e) = self
            .extension_runner
            .emit_lifecycle(ExtensionEvent::AfterProviderResponse, lifecycle_ctx.clone())
            .await
        {
            return end_turn_with_error_typed(&self.extension_runner, &self.shared, e).await;
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
/// 封装 `drive_agent` 调用。
/// handler 和 spawner 共用此函数，通过 `on_signal` 闭包处理各自的差异。
///
/// `on_signal` 接收 `AgentSignal`：
/// - Event 信号已由 EventBus 处理，闭包可做额外副作用（如 progress 转发）。
pub async fn run_turn<F, Fut>(
    agent: &TurnRunner,
    user_text: &str,
    transient_instructions: Option<String>,
    event_bus: &dyn EventBus,
    on_signal: F,
) -> RunTurnResult
where
    F: FnMut(AgentSignal) -> Fut,
    Fut: Future<Output = ()>,
{
    let (output, emitted_error) = drive_agent(
        agent,
        user_text,
        transient_instructions,
        event_bus,
        on_signal,
    )
    .await;

    RunTurnResult {
        output,
        emitted_error,
    }
}
