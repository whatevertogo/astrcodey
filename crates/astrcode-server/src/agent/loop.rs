//! Agent loop — 临时回合处理器与回合驱动。
//!
//! 负责处理一轮完整的对话：组装提示词、调用 LLM、执行工具调用、
//! 分发扩展钩子事件，并将事件流式传输给客户端。
//! Agent 是无状态的短暂对象，处理完一个回合后即被丢弃。
//! `drive_agent` 负责在回合执行时转发事件流并等待最终输出。

use std::{future::Future, sync::Arc};

use astrcode_context::{
    compaction::{
        CompactResult, CompactSkipReason, CompactSummaryRenderOptions,
        compact_messages_with_render_options, is_prompt_too_long_message,
    },
    manager::{ContextPrepareInput, LlmContextAssembler, PreparedContext},
    token_usage::should_compact as token_should_compact,
};
use astrcode_core::{
    config::ModelSelection,
    event::EventPayload,
    extension::{
        CompactTrigger, ExtensionEvent, LifecycleContext, ProviderContext, ProviderEvent,
        ProviderResult,
    },
    llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, LlmRole},
    storage::CompactSnapshotInput,
    tool::{BackgroundTaskReader, FileObservationStore, ToolDefinition},
    types::*,
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_tools::registry::ToolRegistry;
use tokio::sync::{mpsc, oneshot};

use super::{
    compact::{
        CompactHookContext, MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES, collect_compact_instructions,
        compact_trigger_name, compact_with_forked_provider,
        counts_as_auto_compact_provider_failure, dispatch_post_compact,
        prepared_context_from_compaction,
    },
    post_compact::enrich_post_compact_context,
    shared_context::{
        AgentError, AgentSignal, SharedTurnContext, end_turn_with_error_typed,
        retained_messages_after_compaction, send_event,
    },
    tool_pipeline::ToolPipeline,
    tool_types::{
        BackgroundTaskCompletion, ExecuteToolCalls, InMemoryFileObservationStore, PendingToolCall,
        assistant_tool_call_message,
    },
    util::{
        activate_discovered_mcp_tools, append_deferred_mcp_tools_reminder, clone_tools_by_index,
        provider_visible_tool_indexes,
    },
};

// ─── StreamOutcome ───────────────────────────────────────────────────────

enum StreamOutcome {
    Complete {
        text: String,
        reasoning_content: String,
        finish_reason: String,
        message_id: MessageId,
        message_started: bool,
    },
    ToolCalls {
        text: Option<String>,
        reasoning_content: String,
        tool_calls: Vec<PendingToolCall>,
        message_id: MessageId,
        message_started: bool,
    },
}
use crate::{agent::AutoCompactFailureTracker, session::SessionManager};

/// 运行 agent 的一次 process_prompt，通过 select! + drain 实时处理事件。
///
/// `on_signal` 在每个事件或控制信号到达时被调用（包含 select 阶段和 drain 阶段）。
/// 返回 `(output, emitted_error)`。
pub(crate) async fn drive_agent<F, Fut>(
    agent: &AgentLoop,
    user_text: &str,
    history: Vec<LlmMessage>,
    mut on_signal: F,
) -> (Result<AgentTurnOutput, AgentError>, bool)
where
    F: FnMut(AgentSignal) -> Fut,
    Fut: Future<Output = ()>,
{
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let agent_future = agent.process_prompt(user_text, history, Some(event_tx));
    tokio::pin!(agent_future);

    let mut emitted_error = false;
    let mut events_closed = false;
    let output = loop {
        tokio::select! {
            result = &mut agent_future => break result,
            payload = event_rx.recv(), if !events_closed => {
                match payload {
                    Some(signal) => {
                        if matches!(signal, AgentSignal::Event(EventPayload::ErrorOccurred { .. })) {
                            emitted_error = true;
                        }
                        on_signal(signal).await;
                    },
                    None => events_closed = true,
                }
            },
        }
    };

    while let Some(signal) = event_rx.recv().await {
        if matches!(
            signal,
            AgentSignal::Event(EventPayload::ErrorOccurred { .. })
        ) {
            emitted_error = true;
        }
        on_signal(signal).await;
    }

    (output, emitted_error)
}

/// Agent — a transient turn processor.
///
/// Created from a session projection, processes one turn, emits event payloads,
/// and is discarded. Durable event persistence stays in the handler; compact
/// transcript snapshots are written through the injected session manager.
pub struct AgentLoop {
    system_prompt: String,
    shared: SharedTurnContext,
    llm: Arc<dyn LlmProvider>,
    extension_runner: Arc<ExtensionRunner>,
    tools: ToolPipeline,
    context_assembler: Arc<LlmContextAssembler>,
    session_manager: Arc<SessionManager>,
    auto_compact_failures: Arc<AutoCompactFailureTracker>,
}

#[derive(Clone)]
pub struct AgentServices {
    pub llm: Arc<dyn LlmProvider>,
    pub tool_registry: Arc<ToolRegistry>,
    pub extension_runner: Arc<ExtensionRunner>,
    pub context_assembler: Arc<LlmContextAssembler>,
    pub session_manager: Arc<SessionManager>,
    pub auto_compact_failures: Arc<AutoCompactFailureTracker>,
    pub background_result_tx: Option<mpsc::UnboundedSender<BackgroundTaskCompletion>>,
    pub background_tasks: Arc<parking_lot::Mutex<super::background::BackgroundTaskManager>>,
    pub agent_session_control: Option<Arc<dyn astrcode_core::tool::AgentSessionControl>>,
}

/// 消费 LLM 事件流直到完成或积累工具调用。
///
/// 返回 `StreamOutcome::Complete` 表示回复完成（无工具调用），
/// 返回 `StreamOutcome::ToolCalls` 表示需要执行工具后继续循环。
async fn consume_llm_stream(
    mut rx: mpsc::UnboundedReceiver<LlmEvent>,
    event_tx: &Option<mpsc::UnboundedSender<AgentSignal>>,
    message_id: MessageId,
) -> Result<StreamOutcome, AgentError> {
    let mut current_text = String::new();
    let mut reasoning_content = String::new();
    let mut tool_calls: Vec<PendingToolCall> = Vec::new();
    let mut message_started = false;

    while let Some(event) = rx.recv().await {
        match event {
            LlmEvent::ContentDelta { delta } => {
                ensure_assistant_message_started(event_tx, &message_id, &mut message_started);
                send_event(
                    event_tx,
                    EventPayload::AssistantTextDelta {
                        message_id: message_id.clone(),
                        delta: delta.clone(),
                    },
                );
                current_text.push_str(&delta);
            },
            LlmEvent::ThinkingDelta { delta } => {
                ensure_assistant_message_started(event_tx, &message_id, &mut message_started);
                send_event(
                    event_tx,
                    EventPayload::ThinkingDelta {
                        message_id: message_id.clone(),
                        delta: delta.clone(),
                    },
                );
                reasoning_content.push_str(&delta);
            },
            LlmEvent::ToolCallStart {
                call_id,
                name,
                arguments,
            } => {
                // Replace duplicate call_id entries from buggy providers
                // instead of silently merging arguments.
                if let Some(existing) = tool_calls.iter_mut().find(|t| t.call_id == call_id) {
                    tracing::warn!(
                        call_id,
                        name,
                        "duplicate ToolCallStart with same call_id, replacing previous entry"
                    );
                    existing.name = name.clone();
                    existing.arguments = arguments.clone();
                } else {
                    send_event(
                        event_tx,
                        EventPayload::ToolCallStarted {
                            call_id: call_id.clone().into(),
                            tool_name: name.clone(),
                        },
                    );
                    if !arguments.is_empty() {
                        send_event(
                            event_tx,
                            EventPayload::ToolCallArgumentsDelta {
                                call_id: call_id.clone().into(),
                                delta: arguments.clone(),
                            },
                        );
                    }
                    tool_calls.push(PendingToolCall {
                        call_id,
                        name,
                        arguments,
                    });
                }
            },
            LlmEvent::ToolCallDelta { call_id, delta } => {
                if let Some(tc) = tool_calls.iter_mut().find(|t| t.call_id == call_id) {
                    tc.arguments.push_str(&delta);
                }
                send_event(
                    event_tx,
                    EventPayload::ToolCallArgumentsDelta {
                        call_id: call_id.into(),
                        delta,
                    },
                );
            },
            LlmEvent::Done { finish_reason } => {
                if tool_calls.is_empty() {
                    return Ok(StreamOutcome::Complete {
                        text: current_text,
                        reasoning_content: std::mem::take(&mut reasoning_content),
                        finish_reason,
                        message_id,
                        message_started,
                    });
                }
                let text = if current_text.is_empty() {
                    None
                } else {
                    Some(current_text)
                };
                return Ok(StreamOutcome::ToolCalls {
                    text,
                    reasoning_content: std::mem::take(&mut reasoning_content),
                    tool_calls,
                    message_id,
                    message_started,
                });
            },
            LlmEvent::Error { message } => {
                let recoverable = is_prompt_too_long_message(&message);
                send_event(
                    event_tx,
                    EventPayload::ErrorOccurred {
                        code: -32603,
                        message: message.clone(),
                        recoverable,
                    },
                );
                return Err(AgentError::Llm(LlmError::StreamParse(message)));
            },
        }
    }

    Err(AgentError::Internal("LLM stream ended unexpectedly".into()))
}

fn ensure_assistant_message_started(
    event_tx: &Option<mpsc::UnboundedSender<AgentSignal>>,
    message_id: &MessageId,
    message_started: &mut bool,
) {
    if *message_started {
        return;
    }
    send_event(
        event_tx,
        EventPayload::AssistantMessageStarted {
            message_id: message_id.clone(),
        },
    );
    *message_started = true;
}

fn non_empty_reasoning_content(reasoning_content: String) -> Option<String> {
    if reasoning_content.is_empty() {
        None
    } else {
        Some(reasoning_content)
    }
}

fn assistant_message_with_thinking(text: &str, reasoning_content: Option<String>) -> LlmMessage {
    let mut message = LlmMessage::assistant(text);
    message.reasoning_content = reasoning_content;
    message
}

fn provider_visible_messages(messages: Vec<LlmMessage>) -> Vec<LlmMessage> {
    messages
        .into_iter()
        .map(LlmMessage::provider_visible)
        .filter(LlmMessage::has_provider_visible_content)
        .collect()
}

impl AgentLoop {
    /// 创建一个新的 AgentLoop 实例。
    ///
    /// `AgentServices` 中的依赖被分配给相应的子对象；
    /// `AgentLoop` 本身只保留编排职责。
    pub fn new(
        session_id: SessionId,
        working_dir: String,
        system_prompt: String,
        model_id: String,
        services: AgentServices,
    ) -> Self {
        let shared = SharedTurnContext::new(session_id, working_dir, model_id);
        let background_task_reader: Option<Arc<dyn BackgroundTaskReader>> = Some(Arc::new(
            super::background::BackgroundTaskReaderImpl::new(services.background_tasks.clone()),
        ));
        let file_observation_store: Option<Arc<dyn FileObservationStore>> =
            Some(Arc::new(InMemoryFileObservationStore::default()));
        let capabilities = super::tool_types::ToolRuntimeCapabilities {
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
            services.session_manager.clone(),
            capabilities,
        );
        Self {
            system_prompt,
            shared,
            llm: services.llm,
            extension_runner: services.extension_runner,
            tools,
            context_assembler: services.context_assembler,
            session_manager: services.session_manager,
            auto_compact_failures: services.auto_compact_failures,
        }
    }

    /// 处理用户输入的完整 Agent 循环。
    pub(crate) async fn process_prompt(
        &self,
        user_text: &str,
        history: Vec<LlmMessage>,
        event_tx: Option<mpsc::UnboundedSender<AgentSignal>>,
    ) -> Result<AgentTurnOutput, AgentError> {
        let _session_history = history.clone();
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

        let mut messages = Vec::with_capacity(history.len() + 2);
        if !self.system_prompt.trim().is_empty() {
            messages.push(LlmMessage::system(self.system_prompt.clone()));
        }
        messages.extend(
            history
                .into_iter()
                .filter(|message| message.role != LlmRole::System),
        );
        messages.push(LlmMessage::user(user_text));

        let mut final_text = String::new();
        let mut all_tool_results: Vec<astrcode_core::tool::ToolResult> = Vec::new();
        let return_auto_compaction = event_tx.is_none();
        let mut auto_compaction: Option<AgentCompactContinuation> = None;

        loop {
            let (system_messages, prepared_context, compacted) = self
                .prepare_provider_context(&mut messages, &tools, &event_tx)
                .await?;
            if return_auto_compaction {
                if let Some(compaction) = compacted {
                    auto_compaction = Some(AgentCompactContinuation {
                        trigger: CompactTrigger::AutoThreshold,
                        compaction,
                    });
                }
            }

            let mut context_messages = prepared_context.messages;
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
                    return Ok(AgentTurnOutput {
                        text: final_text,
                        finish_reason,
                        tool_results: all_tool_results,
                        auto_compaction: auto_compaction
                            .map(|continuation| continuation.with_retained_messages(&messages)),
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

    /// 准备本次 provider request 的上下文窗口。
    async fn prepare_provider_context(
        &self,
        messages: &mut Vec<LlmMessage>,
        tools: &[ToolDefinition],
        event_tx: &Option<mpsc::UnboundedSender<AgentSignal>>,
    ) -> Result<(Vec<LlmMessage>, PreparedContext, Option<CompactResult>), AgentError> {
        let (system_messages, visible_messages): (Vec<_>, Vec<_>) = messages
            .iter()
            .cloned()
            .partition(|message| message.role == LlmRole::System);
        let prepare_input = ContextPrepareInput {
            messages: visible_messages,
            system_prompt: Some(&self.system_prompt),
            model_limits: self.llm.model_limits(),
        };
        let compact_message_count = prepare_input.messages.len();
        let should_auto_compact = self.context_assembler.auto_compact_enabled()
            && token_should_compact(self.context_assembler.prompt_snapshot(&prepare_input));
        let prepared_context = if should_auto_compact {
            send_event(event_tx, EventPayload::CompactionStarted);
            let compact_instructions = match self
                .compact_instructions(CompactTrigger::AutoThreshold, compact_message_count)
                .await
            {
                Ok(instructions) => instructions,
                Err(error) => {
                    return end_turn_with_error_typed(&self.extension_runner, &self.shared, error)
                        .await;
                },
            };
            let transcript_path = self
                .write_compact_snapshot(
                    CompactTrigger::AutoThreshold,
                    prepare_input.messages.clone(),
                    Some(&self.system_prompt),
                )
                .await;
            let render_options = CompactSummaryRenderOptions { transcript_path };
            let provider_compaction = if self
                .auto_compact_failures
                .should_skip_provider(&self.shared.session_id)
            {
                None
            } else {
                Some(
                    match compact_with_forked_provider(
                        Arc::clone(&self.llm),
                        tools.to_vec(),
                        &prepare_input.messages,
                        prepare_input.system_prompt,
                        self.context_assembler.settings(),
                        &compact_instructions,
                        &render_options,
                    )
                    .await
                    {
                        Ok(compaction) => {
                            self.auto_compact_failures
                                .record_provider_success(&self.shared.session_id);
                            Ok(compaction)
                        },
                        Err(error) => {
                            if counts_as_auto_compact_provider_failure(&error) {
                                let failures = self
                                    .auto_compact_failures
                                    .record_provider_failure(&self.shared.session_id);
                                tracing::warn!(
                                    session_id = %self.shared.session_id,
                                    failures,
                                    max_failures = MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES,
                                    error = %error,
                                    "provider-backed auto compact failed; using deterministic fallback"
                                );
                            }
                            Err(error)
                        },
                    },
                )
            };
            match provider_compaction {
                Some(Ok(mut compaction)) => {
                    enrich_post_compact_context(
                        &mut compaction,
                        self.shared.session_id.as_str(),
                        &prepare_input.messages,
                        &self.shared.working_dir,
                        Some(&self.system_prompt),
                        tools,
                        self.context_assembler.settings(),
                    )
                    .await;
                    prepared_context_from_compaction(compaction)
                },
                Some(Err(_)) | None => match compact_messages_with_render_options(
                    &prepare_input.messages,
                    prepare_input.system_prompt,
                    &render_options,
                ) {
                    Ok(mut compaction) => {
                        enrich_post_compact_context(
                            &mut compaction,
                            self.shared.session_id.as_str(),
                            &prepare_input.messages,
                            &self.shared.working_dir,
                            Some(&self.system_prompt),
                            tools,
                            self.context_assembler.settings(),
                        )
                        .await;
                        prepared_context_from_compaction(compaction)
                    },
                    Err(CompactSkipReason::Empty | CompactSkipReason::NothingToCompact) => {
                        self.context_assembler.prepare_messages(prepare_input)
                    },
                },
            }
        } else {
            self.context_assembler.prepare_messages(prepare_input)
        };
        if let Some(compaction) = prepared_context.compaction.as_ref() {
            if let Err(error) = self
                .notify_post_compact(
                    CompactTrigger::AutoThreshold,
                    compact_message_count,
                    compaction,
                )
                .await
            {
                return end_turn_with_error_typed(&self.extension_runner, &self.shared, error)
                    .await;
            }
            let compacted = compaction.clone();
            if event_tx.is_some() {
                self.request_auto_compact_transition(event_tx, compacted.clone())
                    .await?;
            }
            *messages = [system_messages.clone(), prepared_context.messages.clone()].concat();
            return Ok((system_messages, prepared_context, Some(compacted)));
        }

        Ok((system_messages, prepared_context, None))
    }

    async fn request_auto_compact_transition(
        &self,
        event_tx: &Option<mpsc::UnboundedSender<AgentSignal>>,
        compaction: CompactResult,
    ) -> Result<(), AgentError> {
        let Some(tx) = event_tx else {
            return Ok(());
        };
        let (reply, rx) = oneshot::channel();
        tx.send(AgentSignal::AutoCompact {
            trigger: CompactTrigger::AutoThreshold,
            compaction,
            reply,
        })
        .map_err(|_| AgentError::Internal("auto compact transition channel closed".into()))?;
        rx.await
            .map_err(|_| AgentError::Internal("auto compact transition was dropped".into()))?
            .map(|_| ())
            .map_err(AgentError::Internal)
    }

    async fn apply_before_provider_request_hook(
        &self,
        system_messages: Vec<LlmMessage>,
        context_messages: Vec<LlmMessage>,
    ) -> Result<Vec<LlmMessage>, AgentError> {
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
                Err(AgentError::Internal(reason))
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
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, AgentError> {
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
    ) -> Result<(), AgentError> {
        if let Err(e) = self
            .extension_runner
            .emit_lifecycle(ExtensionEvent::AfterProviderResponse, lifecycle_ctx.clone())
            .await
        {
            return end_turn_with_error_typed(&self.extension_runner, &self.shared, e).await;
        }
        Ok(())
    }

    async fn write_compact_snapshot(
        &self,
        trigger: CompactTrigger,
        provider_messages: Vec<LlmMessage>,
        system_prompt: Option<&str>,
    ) -> Option<String> {
        let snapshot = CompactSnapshotInput {
            trigger: compact_trigger_name(trigger).into(),
            model_id: self.shared.model_id.clone(),
            working_dir: self.shared.working_dir.clone(),
            system_prompt: system_prompt.map(str::to_string),
            provider_messages,
        };
        match self
            .session_manager
            .write_compact_snapshot(&self.shared.session_id, snapshot)
            .await
        {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(
                    session_id = %self.shared.session_id,
                    trigger = compact_trigger_name(trigger),
                    error = %error,
                    "Failed to write compact transcript snapshot"
                );
                None
            },
        }
    }

    async fn compact_instructions(
        &self,
        trigger: CompactTrigger,
        message_count: usize,
    ) -> Result<Vec<String>, AgentError> {
        collect_compact_instructions(
            &self.extension_runner,
            CompactHookContext {
                session_id: self.shared.session_id.as_str(),
                working_dir: &self.shared.working_dir,
                model_id: &self.shared.model_id,
                trigger,
                message_count,
            },
        )
        .await
        .map_err(AgentError::Extension)
    }

    async fn notify_post_compact(
        &self,
        trigger: CompactTrigger,
        message_count: usize,
        compaction: &CompactResult,
    ) -> Result<(), AgentError> {
        dispatch_post_compact(
            &self.extension_runner,
            CompactHookContext {
                session_id: self.shared.session_id.as_str(),
                working_dir: &self.shared.working_dir,
                model_id: &self.shared.model_id,
                trigger,
                message_count,
            },
            compaction,
        )
        .await
        .map_err(AgentError::Extension)
    }
}

/// Agent 回合的输出结果。
#[derive(Debug)]
pub struct AgentTurnOutput {
    pub text: String,
    pub finish_reason: String,
    pub tool_results: Vec<astrcode_core::tool::ToolResult>,
    pub auto_compaction: Option<AgentCompactContinuation>,
}

/// Agent loop 发现 auto compact 后交给 command owner 执行的 continuation 计划。
#[derive(Debug)]
pub struct AgentCompactContinuation {
    pub trigger: CompactTrigger,
    pub compaction: CompactResult,
}

impl AgentCompactContinuation {
    fn with_retained_messages(mut self, messages: &[LlmMessage]) -> Self {
        self.compaction.retained_messages =
            retained_messages_after_compaction(messages, &self.compaction.context_messages);
        self
    }
}

// Re-export constants for the test module.
#[cfg(test)]
use super::shared_context::{TOOL_SEARCH_METADATA_KEY, TOOL_SEARCH_TOOL_NAME};

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "loop_tests.rs"]
mod tests;
