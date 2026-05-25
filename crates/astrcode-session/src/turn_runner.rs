//! TurnRunner — 临时回合处理器与回合驱动。
//!
//! 负责处理一轮完整的对话：调用 LLM、执行工具调用、
//! 分发扩展钩子事件，并将事件流式传输给客户端。
//! Agent 是无状态的短暂对象，处理完一个回合后即被丢弃。
//! `drive_agent` 负责在回合执行时转发事件流并等待最终输出。

use std::{sync::Arc, time::Duration};

use astrcode_context::{
    compaction::CompactResult, context_assembler::ContextPrepareInput,
    prompt_engine::system_messages_from_prompt,
};
use astrcode_core::{
    event::{Event, EventPayload},
    extension::{CompactStrategy, CompactTrigger, ExtensionEvent, ProviderEvent, ProviderResult},
    llm::{LlmContent, LlmError, LlmEvent, LlmMessage, LlmRole},
    tool::ToolDefinition,
    types::*,
};
use astrcode_support::hash::hex_fingerprint;
use tokio::sync::mpsc;

use crate::{
    compact::{
        CompactHookContext, collect_compact_instructions, compact_trigger_name,
        dispatch_post_compact, persist_compact_result,
    },
    compaction_coordinator::{CompactionRequest, PreparedContextMessages},
    llm_stream::{
        StreamOutcome, assistant_message_with_thinking, consume_llm_stream,
        non_empty_reasoning_content, provider_visible_messages,
    },
    session::Session,
    tool_pipeline::ToolPipeline,
    tool_types::ExecuteToolCalls,
    turn_context::{
        SharedTurnContext, TurnError, TurnEventTx, end_turn_with_error_typed,
        on_step_end_best_effort, send_event,
    },
    turn_stages::{PreparedProviderRequest, TurnState},
};

/// 运行 agent 的一次 process_prompt，通过 select! + drain 实时处理事件。
///
/// 每个事件经 `Session::emit` 写 store + fanout 到 runtime 广播。
/// 返回 `(output, emitted_error)`。
pub(crate) async fn drive_agent(
    agent: &mut TurnRunner,
    user_text: &str,
    turn_id: &TurnId,
) -> (Result<TurnOutput, TurnError>, bool) {
    let session = Arc::clone(agent.session());
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let agent_future = agent.process_prompt(user_text, turn_id, Some(event_tx));
    tokio::pin!(agent_future);

    let mut emitted_error = false;
    let mut events_closed = false;

    let output = loop {
        tokio::select! {
            result = &mut agent_future => break result,
            payload = event_rx.recv(), if !events_closed => {
                match payload {
                    Some(payload) => {
                        if matches!(payload, EventPayload::ErrorOccurred { .. }) {
                            emitted_error = true;
                        }
                        dispatch_turn_event(&session, turn_id, payload).await;
                    },
                    None => events_closed = true,
                }
            },
        }
    };

    while let Some(payload) = event_rx.recv().await {
        if matches!(payload, EventPayload::ErrorOccurred { .. }) {
            emitted_error = true;
        }
        dispatch_turn_event(&session, turn_id, payload).await;
    }

    (output, emitted_error)
}

/// 把一个 turn 内事件写入 session（持久化 + fanout）。
async fn dispatch_turn_event(session: &Session, turn_id: &TurnId, payload: EventPayload) {
    if payload.is_durable() {
        if let Err(error) = session.emit_durable(Some(turn_id), payload).await {
            tracing::warn!(
                session_id = %session.id(),
                turn_id = %turn_id,
                error = %error,
                "durable turn event emit failed"
            );
        }
    } else {
        session.emit_live(Some(turn_id), payload).await;
    }
}

/// AgentTurn — 一个临时的回合处理器。
pub(crate) struct TurnRunner {
    session: Arc<Session>,
    shared: SharedTurnContext,
    llm: Arc<dyn astrcode_core::llm::LlmProvider>,
    system_prompt: String,
    extra_system_prompt: Option<String>,
    initial_history: Vec<LlmMessage>,
    tools: ToolPipeline,
    event_rx: mpsc::Receiver<Event>,
}

#[derive(Clone)]
pub(crate) struct CompactionStageMeta {
    pub(crate) base_event_seq: u64,
    pub(crate) trigger: CompactTrigger,
    pub(crate) strategy: CompactStrategy,
    pub(crate) llm_api_failed: bool,
}

impl TurnRunner {
    pub(crate) fn session(&self) -> &Arc<Session> {
        &self.session
    }

    pub(crate) fn llm(&self) -> &Arc<dyn astrcode_core::llm::LlmProvider> {
        &self.llm
    }

    pub(crate) fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    pub(crate) fn new_with_llm(
        session: Arc<Session>,
        session_state: &astrcode_core::storage::SessionReadModel,
        background_result_tx: Option<
            mpsc::UnboundedSender<crate::background::BackgroundTaskCompletion>,
        >,
        session_store_dir: Option<std::path::PathBuf>,
        llm: Arc<dyn astrcode_core::llm::LlmProvider>,
    ) -> Result<Self, TurnError> {
        let shared = SharedTurnContext {
            session_id: session.id().clone(),
            working_dir: session_state.working_dir.clone(),
            model_id: session_state.model_id.clone(),
            session_store_dir: session_store_dir.clone(),
        };
        let system_prompt = session_state.system_prompt.clone().unwrap_or_default();
        let initial_history = session_state.provider_messages();
        let runtime = Arc::clone(session.runtime());
        let caps = Arc::clone(session.caps());

        let capabilities = crate::tool_exec::ToolRuntimeCapabilities::for_turn(
            &session,
            &shared,
            background_result_tx,
            session_store_dir,
        );
        let tools = ToolPipeline::new(
            shared.clone(),
            runtime.tool_registry(),
            Arc::clone(caps.extension_runner()),
            Arc::clone(&session),
            capabilities,
        );
        let context_settings = caps.context_assembler().settings().clone();
        runtime.configure_compact_circuit_breaker(
            context_settings.compact_circuit_breaker_threshold,
            Duration::from_secs(context_settings.compact_circuit_breaker_cooldown_secs),
        );
        let event_rx = session.subscribe();
        Ok(Self {
            session,
            shared,
            llm,
            system_prompt,
            extra_system_prompt: session_state.extra_system_prompt.clone(),
            initial_history,
            tools,
            event_rx,
        })
    }

    pub(crate) async fn process_prompt(
        &mut self,
        user_text: &str,
        turn_id: &TurnId,
        event_tx: Option<TurnEventTx>,
    ) -> Result<TurnOutput, TurnError> {
        let all_tools = self.tools.list_definitions_with_prompt_metadata();
        let extension_runner = Arc::clone(self.session().caps().extension_runner());

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

        let mut state = TurnState::new(
            std::mem::take(&mut self.initial_history),
            self.system_prompt(),
            user_text,
            all_tools,
        );

        loop {
            self.drain_mid_turn_messages(&mut state);

            extension_runner
                .emit_lifecycle(ExtensionEvent::StepStart, lifecycle_ctx.clone())
                .await?;

            let prepared = self
                .prepare_stage(&extension_runner, &mut state, turn_id)
                .await?;
            let visible_tools = state.visible_tools();
            let outcome = match self
                .llm_stage(&extension_runner, prepared, &visible_tools, &event_tx)
                .await
            {
                Ok(outcome) => outcome,
                Err(TurnError::Llm(LlmError::PromptTooLong(_))) if !state.reactive_compact_used() => {
                    state.mark_reactive_compact_used();
                    if !self
                        .run_reactive_compaction(&extension_runner, &mut state, turn_id)
                        .await?
                    {
                        return end_turn_with_error_typed(
                            &extension_runner,
                            &self.shared,
                            TurnError::CompactExhausted,
                        )
                        .await;
                    }
                    continue;
                },
                Err(TurnError::Llm(LlmError::PromptTooLong(_))) => {
                    return end_turn_with_error_typed(
                        &extension_runner,
                        &self.shared,
                        TurnError::CompactExhausted,
                    )
                    .await;
                },
                Err(error) => return Err(error),
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
                        state.push_message(assistant_message_with_thinking(
                            &text,
                            reasoning_content.clone(),
                        ));
                        state.append_final_text(&text);
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
                    on_step_end_best_effort(&extension_runner, &lifecycle_ctx).await;
                    return self
                        .postprocess_complete_stage(
                            &extension_runner,
                            user_text.to_string(),
                            state,
                            finish_reason,
                        )
                        .await;
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
                        state.append_final_text(visible_text);
                    }
                    if message_started && (!visible_text.is_empty() || reasoning_content.is_some())
                    {
                        send_event(
                            event_tx.as_ref(),
                            EventPayload::AssistantMessageCompleted {
                                message_id,
                                text: visible_text.to_string(),
                                reasoning_content: reasoning_content.clone(),
                            },
                        );
                    }

                    self.tools_stage(
                        &extension_runner,
                        &mut state,
                        &tool_calls,
                        visible_text,
                        reasoning_content,
                        &event_tx,
                    )
                    .await?;

                    on_step_end_best_effort(&extension_runner, &lifecycle_ctx).await;
                },
            }
        }
    }

    fn drain_mid_turn_messages(&mut self, state: &mut TurnState) {
        loop {
            match self.event_rx.try_recv() {
                Ok(event) => {
                    if let EventPayload::UserMessage { text, .. } = event.payload {
                        state.push_message(LlmMessage::user(&text));
                    }
                },
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
    }

    async fn prepare_stage(
        &mut self,
        extension_runner: &astrcode_extensions::runner::ExtensionRunner,
        state: &mut TurnState,
        turn_id: &TurnId,
    ) -> Result<PreparedProviderRequest, TurnError> {
        self.refresh_system_prompt(state).await?;

        let llm = Arc::clone(self.llm());
        let context_assembler = Arc::clone(self.session().caps().context_assembler());
        let custom_instructions = self
            .compact_instructions(extension_runner, state, CompactTrigger::AutoThreshold)
            .await;
        let (_, visible_messages) = crate::compaction_coordinator::split_system_messages(state);
        let probe_input = ContextPrepareInput {
            messages: visible_messages,
            system_prompt: Some(self.system_prompt()),
            model_limits: llm.model_limits(),
            custom_instructions,
        };
        let should_auto_compact = context_assembler.should_auto_compact(&probe_input);
        let base_event_seq = if should_auto_compact {
            self.read_base_event_seq().await?
        } else {
            0
        };

        let PreparedContextMessages {
            system_messages,
            context_messages,
            compaction_applied: _,
        } = self
            .prepare_context_messages(
                extension_runner,
                state,
                turn_id,
                CompactionRequest {
                    trigger: CompactTrigger::AutoThreshold,
                    strategy: CompactStrategy::Auto,
                    allow_auto_compact: should_auto_compact
                        && self.should_attempt_llm_compact(CompactTrigger::AutoThreshold),
                    force_compact: false,
                    base_event_seq,
                },
            )
            .await?;

        let messages = self
            .apply_before_provider_request_hook(extension_runner, system_messages, context_messages)
            .await?;
        Ok(PreparedProviderRequest { llm, messages })
    }

    pub(crate) async fn handle_compaction_stage(
        &self,
        extension_runner: &astrcode_extensions::runner::ExtensionRunner,
        state: &TurnState,
        compaction: &mut CompactResult,
        settings: &astrcode_context::ContextSettings,
        turn_id: &TurnId,
        meta: CompactionStageMeta,
    ) -> bool {
        self.session()
            .emit_live(Some(turn_id), EventPayload::CompactionStarted)
            .await;
        let visible_tools = state.visible_tools();
        crate::post_compact::enrich_post_compact_context(
            compaction,
            self.shared.session_id.as_str(),
            state.messages(),
            &self.shared.working_dir,
            Some(self.system_prompt()),
            &visible_tools,
            settings,
            self.shared.session_store_dir.clone(),
        )
        .await;
        let hook_ctx = self.compact_hook_context(state, meta.trigger);
        if let Err(e) = dispatch_post_compact(extension_runner, hook_ctx, compaction).await {
            tracing::warn!(error = %e, "PostCompact extension dispatch failed");
        }

        if meta.trigger == CompactTrigger::AutoThreshold && meta.llm_api_failed {
            self.session()
                .runtime()
                .compact_circuit_breaker()
                .lock()
                .record_llm_failure();
        }

        let fp = hex_fingerprint(self.system_prompt().as_bytes());
        let trigger_name = compact_trigger_name(meta.trigger);
        match persist_compact_result(
            self.session(),
            compaction,
            trigger_name,
            &self.system_prompt,
            &fp,
            self.extra_system_prompt.as_deref(),
            meta.base_event_seq,
            meta.strategy,
        )
        .await
        {
            Ok(persisted) => {
                if meta.trigger == CompactTrigger::AutoThreshold && !meta.llm_api_failed {
                    self.session()
                        .runtime()
                        .compact_circuit_breaker()
                        .lock()
                        .record_compact_success();
                }
                self.session()
                    .emit_live(
                        Some(turn_id),
                        EventPayload::CompactionCompleted {
                            messages_removed: persisted.messages_removed,
                        },
                    )
                    .await;
                true
            },
            Err(e) => {
                tracing::warn!(error = %e, "auto-compact persist skipped");
                self.session()
                    .emit_live(
                        Some(turn_id),
                        EventPayload::CompactionSkipped {
                            reason: e.to_string(),
                        },
                    )
                    .await;
                false
            },
        }
    }

    async fn run_reactive_compaction(
        &mut self,
        extension_runner: &astrcode_extensions::runner::ExtensionRunner,
        state: &mut TurnState,
        turn_id: &TurnId,
    ) -> Result<bool, TurnError> {
        self.refresh_system_prompt(state).await?;
        let base_event_seq = self.read_base_event_seq().await?;
        let prepared = self
            .prepare_context_messages(
                extension_runner,
                state,
                turn_id,
                CompactionRequest {
                    trigger: CompactTrigger::ReactivePromptTooLong,
                    strategy: CompactStrategy::ReactivePromptTooLong,
                    allow_auto_compact: false,
                    force_compact: true,
                    base_event_seq,
                },
            )
            .await?;
        Ok(prepared.compaction_applied)
    }

    fn compact_hook_context<'a>(
        &'a self,
        state: &TurnState,
        trigger: CompactTrigger,
    ) -> CompactHookContext<'a> {
        CompactHookContext {
            session_id: self.shared.session_id.as_str(),
            working_dir: &self.shared.working_dir,
            model_id: &self.shared.model_id,
            trigger,
            message_count: state.message_count(),
        }
    }

    pub(crate) async fn compact_instructions(
        &self,
        extension_runner: &astrcode_extensions::runner::ExtensionRunner,
        state: &TurnState,
        trigger: CompactTrigger,
    ) -> Vec<String> {
        collect_compact_instructions(extension_runner, self.compact_hook_context(state, trigger))
            .await
            .unwrap_or_default()
    }

    async fn read_base_event_seq(&self) -> Result<u64, TurnError> {
        Ok(self
            .session()
            .latest_cursor()
            .await
            .map_err(|e| TurnError::SessionReadFailed(e.to_string()))?
            .and_then(|c| c.parse::<u64>().ok())
            .unwrap_or(0))
    }

    fn should_attempt_llm_compact(&self, trigger: CompactTrigger) -> bool {
        match trigger {
            CompactTrigger::AutoThreshold => self
                .session()
                .runtime()
                .compact_circuit_breaker()
                .lock()
                .should_attempt(),
            CompactTrigger::ManualCommand | CompactTrigger::ReactivePromptTooLong => true,
        }
    }

    async fn refresh_system_prompt(&mut self, state: &mut TurnState) -> Result<(), TurnError> {
        let Some(prompt) = self
            .session()
            .current_system_prompt()
            .await
            .map_err(|e| TurnError::SessionReadFailed(e.to_string()))?
        else {
            return Ok(());
        };
        if prompt == self.system_prompt() {
            return Ok(());
        }

        tracing::info!(session_id = %self.shared.session_id, "system_prompt changed mid-turn, refreshing");
        self.system_prompt = prompt;

        let new_system_messages = system_messages_from_prompt(self.system_prompt());
        let non_system: Vec<LlmMessage> = state
            .messages()
            .iter()
            .filter(|message| message.role != LlmRole::System)
            .cloned()
            .collect();

        let mut messages = Vec::with_capacity(new_system_messages.len() + non_system.len());
        messages.extend(new_system_messages);
        messages.extend(non_system);
        state.replace_messages(messages);

        Ok(())
    }

    async fn llm_stage(
        &self,
        extension_runner: &astrcode_extensions::runner::ExtensionRunner,
        prepared: PreparedProviderRequest,
        tools: &[ToolDefinition],
        event_tx: &Option<TurnEventTx>,
    ) -> Result<StreamOutcome, TurnError> {
        let rx = self
            .start_provider_stream(
                &prepared.llm,
                extension_runner,
                prepared.messages,
                tools,
                event_tx,
            )
            .await?;
        let message_id = new_message_id();
        match consume_llm_stream(rx, event_tx, message_id).await {
            Ok(outcome) => Ok(outcome),
            Err(TurnError::Llm(LlmError::PromptTooLong(message))) => {
                Err(TurnError::Llm(LlmError::PromptTooLong(message)))
            },
            Err(error) => end_turn_with_error_typed(extension_runner, &self.shared, error).await,
        }
    }

    async fn tools_stage(
        &self,
        extension_runner: &astrcode_extensions::runner::ExtensionRunner,
        state: &mut TurnState,
        tool_calls: &[crate::tool_types::PendingToolCall],
        visible_text: &str,
        reasoning_content: Option<String>,
        event_tx: &Option<TurnEventTx>,
    ) -> Result<(), TurnError> {
        self.dispatch_after_provider_response(extension_runner)
            .await?;

        let visible_tools = state.visible_tools();
        let prepared_tool_calls = match self
            .tools
            .prepare_tool_calls(tool_calls, &visible_tools, event_tx)
            .await
        {
            Ok(prepared_tool_calls) => prepared_tool_calls,
            Err(error) => {
                return end_turn_with_error_typed(extension_runner, &self.shared, error).await;
            },
        };
        state.push_message(assistant_tool_call_message(
            &prepared_tool_calls,
            visible_text,
            reasoning_content,
        ));

        let (messages, all_tool_results) = state.messages_and_tool_results_mut();
        let discovered_tools = match self
            .tools
            .execute_and_commit(ExecuteToolCalls {
                prepared: &prepared_tool_calls,
                tools: &visible_tools,
                messages,
                all_tool_results,
                event_tx,
            })
            .await
        {
            Ok(discovered_tools) => discovered_tools,
            Err(error) => {
                return end_turn_with_error_typed(extension_runner, &self.shared, error).await;
            },
        };
        state.activate_deferred_tools(discovered_tools);
        Ok(())
    }

    async fn postprocess_complete_stage(
        &self,
        extension_runner: &astrcode_extensions::runner::ExtensionRunner,
        user_text: String,
        state: TurnState,
        finish_reason: String,
    ) -> Result<TurnOutput, TurnError> {
        self.dispatch_after_provider_response(extension_runner)
            .await?;
        let end_ctx = self
            .shared
            .lifecycle_ctx_with_exchange(user_text, state.final_text().to_string());
        extension_runner
            .emit_lifecycle(ExtensionEvent::TurnEnd, end_ctx)
            .await?;
        let (text, tool_results) = state.take_output_parts();
        Ok(TurnOutput {
            text,
            finish_reason,
            tool_results,
        })
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
                Err(TurnError::ProviderBlocked { reason })
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
        event_tx: &Option<TurnEventTx>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, TurnError> {
        match llm.generate(send_messages, tools.to_vec()).await {
            Ok(rx) => Ok(rx),
            Err(LlmError::PromptTooLong(message)) => {
                Err(TurnError::Llm(LlmError::PromptTooLong(message)))
            },
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

#[derive(Debug)]
pub struct TurnOutput {
    pub text: String,
    pub finish_reason: String,
    pub tool_results: Vec<astrcode_core::tool::ToolResult>,
}

pub struct RunTurnResult {
    pub output: Result<TurnOutput, TurnError>,
    pub emitted_error: bool,
}

pub(crate) async fn run_turn(
    agent: &mut TurnRunner,
    user_text: &str,
    turn_id: &TurnId,
) -> RunTurnResult {
    let (output, emitted_error) = drive_agent(agent, user_text, turn_id).await;

    RunTurnResult {
        output,
        emitted_error,
    }
}

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
