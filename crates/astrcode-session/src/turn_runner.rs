//! TurnRunner — 临时回合处理器与回合驱动。
//!
//! 负责处理一轮完整的对话：调用 LLM、执行工具调用、
//! 分发扩展钩子事件，并将事件流式传输给客户端。
//! Agent 是无状态的短暂对象，处理完一个回合后即被丢弃。

use std::{sync::Arc, time::Duration};

use astrcode_context::{compaction::CompactResult, context_assembler::ContextPrepareInput};
use astrcode_core::{
    event::EventPayload,
    extension::{CompactStrategy, CompactTrigger, ExtensionEvent, ProviderEvent, ProviderResult},
    llm::{LlmError, LlmEvent, LlmMessage},
    storage::SessionReadModel,
    tool::ToolDefinition,
    types::*,
};
use astrcode_support::{hash::hex_fingerprint, sync::lock_parking};
use tokio::sync::mpsc;

use crate::{
    compact::{
        CompactHookContext, collect_compact_instructions, compact_trigger_name,
        dispatch_post_compact, persist_compact_result,
    },
    compaction_coordinator::{CompactionRequest, PreparedContextMessages},
    llm_request_history::{build_llm_request_messages, visible_messages_for_assembler},
    llm_stream::{
        StreamOutcome, consume_llm_stream, non_empty_reasoning_content, provider_visible_messages,
    },
    session::Session,
    tool_pipeline::ToolPipeline,
    tool_types::ExecuteToolCalls,
    turn_context::{
        SharedTurnContext, TurnError, end_turn_with_error_typed, on_step_end_best_effort,
        turn_error_emits_turn_end,
    },
    turn_publish::{ExtensionEventBridge, TurnPublisher},
    turn_stages::{PreparedProviderRequest, TurnState},
};

/// 运行 agent 的一次 process_prompt；durable 在 turn 内同步写入，live 经 TurnPublisher 直发。
pub(crate) async fn drive_agent(
    agent: &mut TurnRunner,
    user_text: &str,
    turn_id: &TurnId,
) -> (Result<TurnOutput, TurnError>, bool) {
    let publisher = Arc::new(TurnPublisher::new(
        Arc::clone(agent.session()),
        turn_id.clone(),
        None,
    ));
    let output = agent.process_prompt(user_text, turn_id, &publisher).await;
    (output, publisher.emitted_error())
}

/// AgentTurn — 一个临时的回合处理器。
pub(crate) struct TurnRunner {
    session: Arc<Session>,
    shared: SharedTurnContext,
    llm: Arc<dyn astrcode_core::llm::LlmProvider>,
    system_prompt: String,
    extra_system_prompt: Option<String>,
    tools: ToolPipeline,
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
        session_state: &SessionReadModel,
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
            turn_event_tx: None,
        };
        let system_prompt = session_state.system_prompt.clone().unwrap_or_default();
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
        Ok(Self {
            session,
            shared,
            llm,
            system_prompt,
            extra_system_prompt: session_state.extra_system_prompt.clone(),
            tools,
        })
    }

    pub(crate) async fn process_prompt(
        &mut self,
        user_text: &str,
        turn_id: &TurnId,
        publisher: &Arc<TurnPublisher>,
    ) -> Result<TurnOutput, TurnError> {
        let extension_runner = Arc::clone(self.session().caps().extension_runner());
        let event_bridge = ExtensionEventBridge::start(Arc::clone(publisher), &mut self.shared);
        let result = self
            .process_prompt_inner(user_text, turn_id, publisher)
            .await;
        event_bridge.shutdown(&mut self.shared).await;
        if let Err(error) = &result {
            self.finalize_turn_on_error(&extension_runner, error).await;
        }
        result
    }

    /// 未走 `end_turn_with_error_typed` 的失败路径补发 `TurnEnd`（如 durable 写入失败）。
    async fn finalize_turn_on_error(
        &self,
        extension_runner: &astrcode_extensions::runner::ExtensionRunner,
        error: &TurnError,
    ) {
        if !turn_error_emits_turn_end(error) {
            return;
        }
        if let Err(hook_error) = extension_runner
            .emit_lifecycle(ExtensionEvent::TurnEnd, self.shared.lifecycle_ctx())
            .await
        {
            tracing::warn!(error = %hook_error, "TurnEnd lifecycle hook failed after turn error");
        }
    }

    async fn process_prompt_inner(
        &mut self,
        _user_text: &str,
        turn_id: &TurnId,
        publisher: &Arc<TurnPublisher>,
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

        let mut state = TurnState::new(all_tools);

        loop {
            publisher.invalidate_model_cache().await;

            extension_runner
                .emit_lifecycle(ExtensionEvent::StepStart, lifecycle_ctx.clone())
                .await?;

            let prepared = self
                .prepare_stage(&extension_runner, &state, turn_id, publisher)
                .await?;
            let visible_tools = state.visible_tools();
            let outcome = match self
                .llm_stage(&extension_runner, prepared, &visible_tools, publisher)
                .await
            {
                Ok(outcome) => outcome,
                Err(TurnError::Llm(LlmError::PromptTooLong(_)))
                    if !state.reactive_compact_used() =>
                {
                    state.mark_reactive_compact_used();
                    if !self
                        .run_reactive_compaction(&extension_runner, &state, turn_id, publisher)
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
                        state.append_final_text(&text);
                        if message_started {
                            publisher
                                .durable(EventPayload::AssistantMessageCompleted {
                                    message_id,
                                    text,
                                    reasoning_content,
                                })
                                .await?;
                        }
                    }
                    on_step_end_best_effort(&extension_runner, &lifecycle_ctx).await;
                    return self
                        .postprocess_complete_stage(
                            &extension_runner,
                            _user_text.to_string(),
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
                    if !tool_calls.is_empty() || message_started {
                        if !message_started {
                            publisher
                                .live(EventPayload::AssistantMessageStarted {
                                    message_id: message_id.clone(),
                                })
                                .await;
                        }
                        publisher
                            .durable(EventPayload::AssistantMessageCompleted {
                                message_id,
                                text: visible_text.to_string(),
                                reasoning_content: reasoning_content.clone(),
                            })
                            .await?;
                    }

                    self.tools_stage(&extension_runner, &mut state, &tool_calls, publisher)
                        .await?;

                    on_step_end_best_effort(&extension_runner, &lifecycle_ctx).await;
                },
            }
        }
    }

    async fn prepare_stage(
        &mut self,
        extension_runner: &astrcode_extensions::runner::ExtensionRunner,
        state: &TurnState,
        turn_id: &TurnId,
        publisher: &Arc<TurnPublisher>,
    ) -> Result<PreparedProviderRequest, TurnError> {
        self.refresh_system_prompt().await?;

        let model = publisher.snapshot_model().await?;

        let llm = Arc::clone(self.llm());
        let context_assembler = Arc::clone(self.session().caps().context_assembler());
        let custom_instructions = self
            .compact_instructions(extension_runner, &model, CompactTrigger::AutoThreshold)
            .await;
        let visible_messages = visible_messages_for_assembler(&model);
        let probe_input = ContextPrepareInput {
            messages: visible_messages,
            system_prompt: Some(self.system_prompt()),
            model_limits: llm.model_limits(),
            custom_instructions,
        };
        let threshold_met = context_assembler.should_auto_compact(&probe_input);
        let run_compact = context_assembler.auto_compact_enabled() && threshold_met;
        let use_llm_for_compact =
            run_compact && self.should_attempt_llm_compact(CompactTrigger::AutoThreshold);
        let base_event_seq = if run_compact {
            self.read_base_event_seq().await?
        } else {
            0
        };

        let PreparedContextMessages {
            context_messages,
            compaction_applied,
        } = self
            .prepare_context_messages(
                extension_runner,
                state,
                &model,
                turn_id,
                CompactionRequest {
                    trigger: CompactTrigger::AutoThreshold,
                    strategy: CompactStrategy::Auto,
                    run_compact,
                    use_llm_for_compact,
                    force_compact: false,
                    base_event_seq,
                    keep_recent_turns: None,
                },
            )
            .await?;

        if compaction_applied {
            publisher.invalidate_model_cache().await;
        }
        let messages = build_llm_request_messages(self.system_prompt(), context_messages);
        let messages = self
            .apply_before_provider_request_hook(extension_runner, messages)
            .await?;
        Ok(PreparedProviderRequest { llm, messages })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn handle_compaction_stage(
        &self,
        extension_runner: &astrcode_extensions::runner::ExtensionRunner,
        state: &TurnState,
        model: &SessionReadModel,
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
            &model.provider_messages(),
            &self.shared.working_dir,
            Some(self.system_prompt()),
            &visible_tools,
            settings,
            self.shared.session_store_dir.clone(),
        )
        .await;
        let hook_ctx = self.compact_hook_context(model, meta.trigger);
        if let Err(e) = dispatch_post_compact(extension_runner, hook_ctx, compaction).await {
            tracing::warn!(error = %e, "PostCompact extension dispatch failed");
        }

        if meta.trigger == CompactTrigger::AutoThreshold && meta.llm_api_failed {
            lock_parking(self.session().runtime().compact_circuit_breaker()).record_llm_failure();
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
                    lock_parking(self.session().runtime().compact_circuit_breaker())
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
        state: &TurnState,
        turn_id: &TurnId,
        publisher: &Arc<TurnPublisher>,
    ) -> Result<bool, TurnError> {
        self.refresh_system_prompt().await?;
        let model = publisher.snapshot_model().await?;
        let base_event_seq = self.read_base_event_seq().await?;
        let prepared = self
            .prepare_context_messages(
                extension_runner,
                state,
                &model,
                turn_id,
                CompactionRequest {
                    trigger: CompactTrigger::ReactivePromptTooLong,
                    strategy: CompactStrategy::ReactivePromptTooLong,
                    run_compact: true,
                    use_llm_for_compact: true,
                    force_compact: true,
                    base_event_seq,
                    keep_recent_turns: None,
                },
            )
            .await?;
        if prepared.compaction_applied {
            publisher.invalidate_model_cache().await;
        }
        Ok(prepared.compaction_applied)
    }

    fn compact_hook_context<'a>(
        &'a self,
        model: &'a SessionReadModel,
        trigger: CompactTrigger,
    ) -> CompactHookContext<'a> {
        CompactHookContext {
            session_id: self.shared.session_id.as_str(),
            working_dir: &self.shared.working_dir,
            model_id: &self.shared.model_id,
            trigger,
            message_count: model
                .context_messages
                .len()
                .saturating_add(model.messages.len()),
        }
    }

    pub(crate) async fn compact_instructions(
        &self,
        extension_runner: &astrcode_extensions::runner::ExtensionRunner,
        model: &SessionReadModel,
        trigger: CompactTrigger,
    ) -> Vec<String> {
        collect_compact_instructions(extension_runner, self.compact_hook_context(model, trigger))
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
            CompactTrigger::AutoThreshold => {
                lock_parking(self.session().runtime().compact_circuit_breaker()).should_attempt()
            },
            CompactTrigger::ManualCommand | CompactTrigger::ReactivePromptTooLong => true,
        }
    }

    async fn refresh_system_prompt(&mut self) -> Result<(), TurnError> {
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
        Ok(())
    }

    async fn llm_stage(
        &self,
        extension_runner: &astrcode_extensions::runner::ExtensionRunner,
        prepared: PreparedProviderRequest,
        tools: &[ToolDefinition],
        publisher: &TurnPublisher,
    ) -> Result<StreamOutcome, TurnError> {
        let rx = self
            .start_provider_stream(
                &prepared.llm,
                extension_runner,
                prepared.messages,
                tools,
                publisher,
            )
            .await?;
        let message_id = new_message_id();
        match consume_llm_stream(rx, publisher, message_id).await {
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
        publisher: &Arc<TurnPublisher>,
    ) -> Result<(), TurnError> {
        self.dispatch_after_provider_response(extension_runner)
            .await?;

        let visible_tools = state.visible_tools();
        let prepared_tool_calls = match self
            .tools
            .prepare_tool_calls(tool_calls, &visible_tools, publisher)
            .await
        {
            Ok(prepared_tool_calls) => prepared_tool_calls,
            Err(error) => {
                return end_turn_with_error_typed(extension_runner, &self.shared, error).await;
            },
        };

        let discovered_tools = match self
            .tools
            .execute_and_commit(ExecuteToolCalls {
                prepared: &prepared_tool_calls,
                tools: &visible_tools,
                state,
                publisher: Arc::clone(publisher),
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
        send_messages: Vec<LlmMessage>,
    ) -> Result<Vec<LlmMessage>, TurnError> {
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
            ProviderResult::ReplaceMessages { messages } => {
                tracing::debug!(
                    message_count = messages.len(),
                    "BeforeProviderRequest ReplaceMessages applies only to this LLM request (not \
                     durable)"
                );
                Ok(provider_visible_messages(messages))
            },
            ProviderResult::AppendMessages { messages } => {
                tracing::debug!(
                    message_count = messages.len(),
                    "BeforeProviderRequest AppendMessages applies only to this LLM request (not \
                     durable)"
                );
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
        publisher: &TurnPublisher,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, TurnError> {
        match llm.generate(send_messages, tools.to_vec()).await {
            Ok(rx) => Ok(rx),
            Err(LlmError::PromptTooLong(message)) => {
                Err(TurnError::Llm(LlmError::PromptTooLong(message)))
            },
            Err(e) => {
                publisher
                    .live(EventPayload::ErrorOccurred {
                        code: -32603,
                        message: e.to_string(),
                        recoverable: false,
                    })
                    .await;
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
