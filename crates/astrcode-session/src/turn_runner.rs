//! TurnLoop — 临时回合处理器与回合驱动。
//!
//! 负责处理一轮完整的对话：调用 LLM、执行工具调用、
//! 分发扩展钩子事件，并将事件流式传输给客户端。
//! Agent 是无状态的短暂对象，处理完一个回合后即被丢弃。

use std::{sync::Arc, time::Duration};

use astrcode_core::{
    event::EventPayload,
    extension::{
        AfterToolResultsContext, AfterToolResultsResult, ContinueAfterStopContext,
        ContinueAfterStopResult, ExtensionEvent, ProviderEvent, ProviderResult,
    },
    llm::{
        LlmContent, LlmError, LlmEvent, LlmMessage, LlmRole, LlmTokenUsage, LlmTokenUsageSource,
        provider_visible_messages, token_estimate,
    },
    storage::SessionReadModel,
    tool::ToolDefinition,
    types::*,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    compaction_coordinator::{Compaction, CompactionHost, PreparedContextMessages},
    llm_request_history::build_llm_request_messages,
    llm_stream::{StreamOutcome, consume_llm_stream, non_empty_reasoning_content},
    session::Session,
    steer::{count_visible_user_messages, has_pending_mid_turn_user_messages},
    tool_deduplicator::ToolCallDeduplicator,
    tool_exec::TurnToolContext,
    tool_pipeline::ToolCalls,
    tool_types::ExecuteDeclaredToolBatch,
    turn_context::{
        SharedTurnContext, TurnError, end_turn_with_error_typed, on_step_end_best_effort,
    },
    turn_publish::{ExtensionEvents, TurnEvents},
    turn_stages::{PreparedProviderRequest, TurnState},
};

/// 运行 agent 的一次 process_prompt；durable 在 turn 内同步写入，live 经 TurnEvents 直发。
pub(crate) async fn drive_agent(
    agent: &mut TurnLoop,
    user_text: &str,
    turn_id: &TurnId,
) -> (Result<TurnOutput, TurnError>, bool) {
    let publisher = Arc::new(TurnEvents::new(
        agent.session().clone(),
        turn_id.clone(),
        None,
    ));
    let output = agent.process_prompt(user_text, turn_id, &publisher).await;
    (output, publisher.emitted_error())
}

/// AgentTurn — 一个临时的回合处理器。
///
/// **演进注记**：当前结构体同时持有 `session` / `llm` / `tools` / `compaction`
/// 五个领域，"每一项 `process_prompt_inner` 都会用到"。如果**以下需求同时出现**，
/// 需要拆分为独立阶段（`PrepareStage` / `LlmStage` / `ToolStage` / `CompactStage`）
/// 并由 `TurnLoop` 以 **Trait 对象** 或 **泛型参数** 组合：
///
/// 1. 同一 session 并行多回合（sub-agent / tree-of-thought）；
/// 2. 中途回滚到上一 checkpoint（需要动 `compaction` 与 `llm_stage` 边界）；
/// 3. 多 provider 轮换（需要替换 `Arc<dyn LlmProvider>` 以外的依赖）。
///
/// TODO: 更好的做法是**先拆 `TurnState`**（当前 `TurnLoop` 内部状态载体）为独立阶段的状态载体，
/// **当前不拆**：`process_prompt_inner` 三个阶段之间的状态转移
/// （`state.tool_deduplicator_mut()` / `state.append_final_text`）
/// 与 `compaction` 强耦合，拆为独立阶段需先拆 `TurnState`。
/// YAGNI：在明确提出以上需求时再动。参考 issue #TBD。
pub(crate) struct TurnLoop {
    session: Session,
    llm: Arc<dyn astrcode_core::llm::LlmProvider>,
    cancellation_token: CancellationToken,
    tools: ToolCalls,
    compaction: Compaction,
}

impl TurnLoop {
    pub(crate) fn session(&self) -> &Session {
        &self.session
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }

    fn max_parallel_tool_calls(&self) -> usize {
        self.session
            .caps()
            .read_effective()
            .agent
            .tool_max_parallel_calls
            .max(1)
    }

    pub(crate) fn system_prompt(&self) -> &str {
        self.compaction.system_prompt()
    }

    fn shared(&self) -> &SharedTurnContext {
        self.tools.shared()
    }

    pub(crate) fn new_with_llm(
        session: Session,
        session_state: &SessionReadModel,
        session_store_dir: Option<std::path::PathBuf>,
        llm: Arc<dyn astrcode_core::llm::LlmProvider>,
        cancellation_token: CancellationToken,
    ) -> Result<Self, TurnError> {
        let system_prompt = session_state.system_prompt.clone().unwrap_or_default();
        let runtime = session.runtime();
        let caps = session.caps();
        let turn = TurnToolContext::for_turn(&session, session_state, session_store_dir);
        let tools = ToolCalls::new(
            turn,
            runtime.loaded_tool_registry(),
            caps.extension_runner_arc(),
            session.clone(),
            cancellation_token.clone(),
        );
        let context_settings = caps.context_assembler().settings().clone();
        runtime.configure_compact_circuit_breaker(
            context_settings.compact_circuit_breaker_threshold,
            Duration::from_secs(context_settings.compact_circuit_breaker_cooldown_secs),
        );
        Ok(Self {
            session,
            llm,
            cancellation_token,
            tools,
            compaction: Compaction::new(system_prompt, session_state.extra_system_prompt.clone()),
        })
    }

    pub(crate) async fn process_prompt(
        &mut self,
        user_text: &str,
        turn_id: &TurnId,
        publisher: &Arc<TurnEvents>,
    ) -> Result<TurnOutput, TurnError> {
        let extension_runner = self.session().caps().extension_runner_arc();
        let event_bridge = ExtensionEvents::start(Arc::clone(publisher), self.tools.shared_mut());
        let result = self
            .process_prompt_inner(user_text, turn_id, publisher)
            .await;
        if result.is_err() {
            self.finalize_turn_on_error(extension_runner.as_ref()).await;
        }
        event_bridge.shutdown(self.tools.shared_mut()).await;
        result
    }

    /// Turn 失败时统一补发 `TurnEnd`，避免 `?` 旁路错误漏掉扩展生命周期钩子。
    async fn finalize_turn_on_error(
        &self,
        extension_runner: &dyn astrcode_kernel::ExtensionRuntime,
    ) {
        if let Err(hook_error) = extension_runner
            .emit_lifecycle(ExtensionEvent::TurnEnd, self.shared().lifecycle_ctx())
            .await
        {
            tracing::warn!(error = %hook_error, "TurnEnd lifecycle hook failed after turn error");
        }
    }

    async fn process_prompt_inner(
        &mut self,
        _user_text: &str,
        turn_id: &TurnId,
        publisher: &Arc<TurnEvents>,
    ) -> Result<TurnOutput, TurnError> {
        let all_tools = self.tools.list_definitions_with_prompt_metadata();
        let extension_runner = self.session().caps().extension_runner_arc();

        let lifecycle_ctx = self.shared().lifecycle_ctx();
        let (turn_start_res, prompt_submit_res) = tokio::join!(
            extension_runner.emit_lifecycle(ExtensionEvent::TurnStart, lifecycle_ctx.clone()),
            extension_runner
                .emit_lifecycle(ExtensionEvent::UserPromptSubmit, lifecycle_ctx.clone()),
        );
        turn_start_res?;
        if let Err(e) = prompt_submit_res {
            return end_turn_with_error_typed(e);
        }

        let mut state = TurnState::new(all_tools);
        if let Ok(count) = publisher.visible_user_message_count().await {
            state.set_tracked_user_message_count(count);
        }

        // Step
        loop {
            self.check_aborted()?;
            state.tool_deduplicator_mut().begin_step();
            let mid_turn_synced = self
                .sync_mid_turn_user_messages_at_step_start(publisher, &mut state)
                .await?;
            let step_ctx = lifecycle_ctx.clone().for_step_start(mid_turn_synced);

            extension_runner
                .emit_lifecycle(ExtensionEvent::StepStart, step_ctx)
                .await?;

            let prepared = self
                .prepare_stage(extension_runner.as_ref(), &state, turn_id, publisher)
                .await?;
            let request_messages = prepared.messages.clone();
            let model_context_window = prepared.llm.model_limits().max_input_tokens;
            let visible_tools = state.visible_tools();
            // 提取 deduplicator 用于流式工具执行；llm_stage 返回后归还。
            // visible_tools 传给 early exec context 供 prepare 使用。
            let dedup_for_early = state.tool_deduplicator_mut();
            let outcome = match self
                .llm_stage(
                    prepared,
                    &visible_tools,
                    publisher,
                    Some(dedup_for_early),
                    visible_tools.clone(),
                )
                .await
            {
                Ok(outcome) => outcome,
                Err(TurnError::Llm(LlmError::PromptTooLong(_)))
                    if !state.reactive_compact_used() =>
                {
                    state.mark_reactive_compact_used();
                    let shared = self.shared().clone();
                    let host = CompactionHost {
                        session: &self.session,
                        llm: &self.llm,
                        shared: &shared,
                        extension_runner: extension_runner.as_ref(),
                    };
                    if !self
                        .compaction
                        .run_reactive_compaction(&host, &state, turn_id, publisher)
                        .await?
                    {
                        return end_turn_with_error_typed(TurnError::CompactExhausted);
                    }
                    continue;
                },
                Err(TurnError::Llm(LlmError::PromptTooLong(_))) => {
                    return end_turn_with_error_typed(TurnError::CompactExhausted);
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
                    usage,
                } => {
                    let reasoning_content = non_empty_reasoning_content(reasoning_content);
                    let assistant_text_for_continue = text.clone();
                    state.record_assistant_text(&text, reasoning_content.clone());
                    if (!text.is_empty() || reasoning_content.is_some()) && message_started {
                        publisher
                            .durable(EventPayload::AssistantMessageCompleted {
                                message_id,
                                text,
                                reasoning_content,
                            })
                            .await?;
                    }
                    if let Some(usage) = usage {
                        publisher
                            .durable(EventPayload::TokenUsageRecorded {
                                usage,
                                model_context_window,
                            })
                            .await?;
                    }
                    on_step_end_best_effort(extension_runner.as_ref(), &lifecycle_ctx).await;

                    if self
                        .should_continue_after_stop(
                            extension_runner.as_ref(),
                            &assistant_text_for_continue,
                            &finish_reason,
                            &mut state,
                        )
                        .await?
                    {
                        continue;
                    }

                    if self
                        .has_pending_mid_turn_user_messages(publisher, &state)
                        .await?
                    {
                        tracing::debug!(
                            "pending mid-turn user messages; running one more agent step"
                        );
                        continue;
                    }

                    let hook_messages = state.provider_response_messages(request_messages);
                    return self
                        .postprocess_complete_stage(
                            extension_runner.as_ref(),
                            _user_text.to_string(),
                            &mut state,
                            finish_reason,
                            hook_messages,
                        )
                        .await;
                },
                StreamOutcome::ToolCalls {
                    text,
                    reasoning_content,
                    tool_calls,
                    early_results,
                    message_id,
                    message_started,
                    usage,
                } => {
                    let reasoning_content = non_empty_reasoning_content(reasoning_content);
                    let visible_text = text.as_deref().unwrap_or_default();
                    state.record_assistant_tool_calls(
                        visible_text,
                        reasoning_content.clone(),
                        &tool_calls,
                    );
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
                                reasoning_content,
                            })
                            .await?;
                    }
                    if let Some(usage) = usage {
                        publisher
                            .durable(EventPayload::TokenUsageRecorded {
                                usage,
                                model_context_window,
                            })
                            .await?;
                    }

                    let hook_messages = state.provider_response_messages(request_messages);
                    let tool_decision = self
                        .tools_stage(
                            extension_runner.as_ref(),
                            &mut state,
                            &tool_calls,
                            early_results,
                            publisher,
                            hook_messages,
                        )
                        .await?;

                    state.tool_deduplicator_mut().end_step();
                    on_step_end_best_effort(extension_runner.as_ref(), &lifecycle_ctx).await;
                    if let ToolStageDecision::EndTurn { reason } = tool_decision {
                        extension_runner
                            .emit_lifecycle(
                                ExtensionEvent::TurnEnd,
                                self.shared().lifecycle_ctx_with_exchange(
                                    _user_text.to_string(),
                                    state.final_text().to_string(),
                                ),
                            )
                            .await?;
                        let (text, tool_results) = state.take_output_parts();
                        return Ok(TurnOutput {
                            text,
                            finish_reason: reason,
                            tool_results,
                        });
                    }
                },
            }
        }
    }

    async fn prepare_stage(
        &mut self,
        extension_runner: &dyn astrcode_kernel::ExtensionRuntime,
        state: &TurnState,
        turn_id: &TurnId,
        publisher: &Arc<TurnEvents>,
    ) -> Result<PreparedProviderRequest, TurnError> {
        let shared = self.shared().clone();
        let host = CompactionHost {
            session: &self.session,
            llm: &self.llm,
            shared: &shared,
            extension_runner,
        };
        self.compaction
            .refresh_system_prompt(host.session, host.shared)
            .await?;

        let model = publisher.snapshot_model().await?;
        let llm = Arc::clone(host.llm);
        let visible_tools = state.visible_tools();
        let compaction_request = self
            .compaction
            .build_auto_compaction_request(&host, &model, &visible_tools)
            .await?;

        let PreparedContextMessages {
            context_messages,
            compaction_applied: _,
        } = self
            .compaction
            .prepare_context_messages(&host, state, &model, turn_id, compaction_request, publisher)
            .await?;

        let messages = build_llm_request_messages(self.system_prompt(), context_messages);
        let mut messages = messages;
        if let Some(reminder) = state.tool_deduplicator().check_reminder() {
            tracing::debug!("injecting tool deduplication system-reminder");
            messages.push(LlmMessage::user(reminder));
        }
        let messages = self
            .apply_before_provider_request_hook(extension_runner, messages)
            .await?;
        Ok(PreparedProviderRequest { llm, messages })
    }

    async fn llm_stage(
        &self,
        prepared: PreparedProviderRequest,
        tools: &[ToolDefinition],
        publisher: &TurnEvents,
        deduplicator: Option<&mut ToolCallDeduplicator>,
        visible_tools: Vec<ToolDefinition>,
    ) -> Result<StreamOutcome, TurnError> {
        let request_messages = prepared.messages.clone();
        let rx = self
            .start_provider_stream(&prepared.llm, prepared.messages, tools, publisher)
            .await?;
        let message_id = new_message_id();

        // 构建 early exec context（需要 deduplicator 和 visible_tools）
        let early_exec = deduplicator.map(|dedup| {
            let max_parallel = self.max_parallel_tool_calls();
            crate::llm_stream::EarlyExecContext {
                pipeline: &self.tools,
                visible_tools,
                deduplicator: dedup,
                max_parallel,
            }
        });

        match consume_llm_stream(
            rx,
            publisher,
            message_id,
            &self.cancellation_token,
            early_exec,
        )
        .await
        {
            Ok(outcome) => Ok(self
                .with_usage_fallback(outcome, &prepared.llm, request_messages, tools)
                .await),
            Err(e @ TurnError::Llm(LlmError::PromptTooLong(_))) => Err(e),
            Err(error) => end_turn_with_error_typed(error),
        }
    }

    async fn with_usage_fallback(
        &self,
        outcome: StreamOutcome,
        llm: &Arc<dyn astrcode_core::llm::LlmProvider>,
        request_messages: Vec<LlmMessage>,
        tools: &[ToolDefinition],
    ) -> StreamOutcome {
        let needs_usage = match &outcome {
            StreamOutcome::Complete { usage, .. } | StreamOutcome::ToolCalls { usage, .. } => {
                usage.is_none()
            },
        };
        if !needs_usage {
            return outcome;
        }

        let usage = self
            .fallback_token_usage(llm, request_messages, tools)
            .await;
        match outcome {
            StreamOutcome::Complete {
                text,
                reasoning_content,
                finish_reason,
                message_id,
                message_started,
                usage: _,
            } => StreamOutcome::Complete {
                text,
                reasoning_content,
                finish_reason,
                message_id,
                message_started,
                usage,
            },
            StreamOutcome::ToolCalls {
                text,
                reasoning_content,
                tool_calls,
                early_results,
                message_id,
                message_started,
                usage: _,
            } => StreamOutcome::ToolCalls {
                text,
                reasoning_content,
                tool_calls,
                early_results,
                message_id,
                message_started,
                usage,
            },
        }
    }

    async fn fallback_token_usage(
        &self,
        llm: &Arc<dyn astrcode_core::llm::LlmProvider>,
        request_messages: Vec<LlmMessage>,
        tools: &[ToolDefinition],
    ) -> Option<LlmTokenUsage> {
        let effective = self.session.caps().read_effective();
        match llm
            .count_input_tokens(request_messages.clone(), tools.to_vec())
            .await
        {
            Ok(count) => {
                tracing::warn!(
                    provider = %effective.llm.provider_kind,
                    model = %effective.llm.model_id,
                    stage = "turn_usage",
                    "provider stream did not include usage; recording provider count fallback"
                );
                Some(LlmTokenUsage {
                    input_tokens: Some(count.input_tokens),
                    cached_input_tokens: None,
                    cache_creation_input_tokens: None,
                    output_tokens: None,
                    reasoning_output_tokens: None,
                    total_tokens: None,
                    source: Some(LlmTokenUsageSource::ProviderCountFallback),
                })
            },
            Err(error) => {
                tracing::warn!(
                    provider = %effective.llm.provider_kind,
                    model = %effective.llm.model_id,
                    stage = "turn_usage",
                    error = %error,
                    "provider stream did not include usage and provider count failed; recording \
                     local estimate fallback"
                );
                Some(LlmTokenUsage {
                    input_tokens: Some(token_estimate::estimate_request_tokens(
                        &request_messages,
                        None,
                    ) as u64),
                    cached_input_tokens: None,
                    cache_creation_input_tokens: None,
                    output_tokens: None,
                    reasoning_output_tokens: None,
                    total_tokens: None,
                    source: Some(LlmTokenUsageSource::LocalEstimateFallback),
                })
            },
        }
    }

    async fn tools_stage(
        &self,
        extension_runner: &dyn astrcode_kernel::ExtensionRuntime,
        state: &mut TurnState,
        tool_calls: &[crate::tool_types::StreamedToolCall],
        early_results: Vec<crate::early_tool_scheduler::EarlyExecutionEntry>,
        publisher: &Arc<TurnEvents>,
        hook_messages: Vec<LlmMessage>,
    ) -> Result<ToolStageDecision, TurnError> {
        self.dispatch_after_provider_response(extension_runner, hook_messages, state)
            .await?;

        let visible_tools = state.visible_tools();

        let plan = self
            .tools
            .prepare_tool_batch(tool_calls, early_results, &visible_tools, state)
            .await?;
        let declared = self.tools.declare_tool_batch(plan, publisher).await?;

        let committed = match self
            .tools
            .execute_and_commit(ExecuteDeclaredToolBatch {
                declared,
                tools: &visible_tools,
                state,
                publisher: Arc::clone(publisher),
            })
            .await
        {
            Ok(discovered_tools) => discovered_tools,
            Err(error) => {
                return end_turn_with_error_typed(error);
            },
        };
        state.activate_deferred_tools(committed.discovered_tools);
        if committed.tool_results.is_empty() {
            return Ok(ToolStageDecision::Continue);
        }
        let decision = extension_runner
            .emit_after_tool_results(AfterToolResultsContext {
                session_id: self.shared().session_id.to_string(),
                working_dir: self.shared().working_dir.clone(),
                model: self.shared().model_selection(),
                tool_results: committed.tool_results,
                session_store_dir: self.shared().session_store_dir.clone(),
            })
            .await?;
        Ok(match decision {
            AfterToolResultsResult::Continue => ToolStageDecision::Continue,
            AfterToolResultsResult::EndTurn { reason } => ToolStageDecision::EndTurn { reason },
        })
    }

    async fn postprocess_complete_stage(
        &self,
        extension_runner: &dyn astrcode_kernel::ExtensionRuntime,
        user_text: String,
        state: &mut TurnState,
        finish_reason: String,
        hook_messages: Vec<LlmMessage>,
    ) -> Result<TurnOutput, TurnError> {
        self.dispatch_after_provider_response(extension_runner, hook_messages, state)
            .await?;
        let end_ctx = self
            .shared()
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

    /// 运行 `BeforeRequest` 扩展钩子。
    ///
    /// 返回值覆盖 LLM 请求的 messages。Append 时**不**动会话语义、
    /// 不入事件日志；仅对本次 LLM 调用生效。
    ///
    /// **性能注记**：`send_messages.clone()` 不可消除。
    /// `ProviderContext` 在 `astrcode-core::extension::ProviderContext` 上
    /// 定义为持有 `Vec<LlmMessage>` **所有权**，`emit_provider` 又需
    /// `&self` 借用；caller 必须 clone 才能让 hook 看到消息。`AppendMessages`
    /// 分支看似可避免 clone（`send_messages` 走 move），但为了在同一
    /// match 里能服侍 `Allow` / `ReplaceMessages` 两个分支不重复构造 ctx，
    /// 只能在进入前就持有独立副本。
    ///
    /// 消除 clone 需要扩展点演进：
    /// 1. `ProviderContext.messages: Arc<Vec<LlmMessage>>`，caller 共享 所有权 `Arc::clone` 代替
    ///    `Vec::clone`；
    /// 2. `ExtensionRuntime::emit_provider` 内部可用 copy-on-write，hook 未改就零拷贝；
    /// 3. `ProviderEvent::BeforeRequest` hook 保持现状（接受只读快照）。
    ///
    /// 是**API 演进**而非 bug——参考 issue #TBD。不优先动。
    async fn apply_before_provider_request_hook(
        &self,
        extension_runner: &dyn astrcode_kernel::ExtensionRuntime,
        send_messages: Vec<LlmMessage>,
    ) -> Result<Vec<LlmMessage>, TurnError> {
        match extension_runner
            .emit_provider(
                ProviderEvent::BeforeRequest,
                self.shared().provider_ctx(send_messages.clone()),
            )
            .await?
        {
            ProviderResult::Block { reason } => Err(TurnError::ProviderBlocked { reason }),
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
        send_messages: Vec<LlmMessage>,
        tools: &[ToolDefinition],
        publisher: &TurnEvents,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, TurnError> {
        let result = tokio::select! {
            _ = self.cancellation_token.cancelled() => return Err(TurnError::Aborted),
            result = llm.generate(send_messages, tools.to_vec()) => result,
        };
        match result {
            Ok(rx) => Ok(rx),
            Err(LlmError::PromptTooLong(message)) => {
                Err(TurnError::Llm(LlmError::PromptTooLong(message)))
            },
            Err(e) => {
                publisher
                    .durable_error(-32603, e.to_string(), false)
                    .await?;
                end_turn_with_error_typed(e)
            },
        }
    }

    async fn dispatch_after_provider_response(
        &self,
        extension_runner: &dyn astrcode_kernel::ExtensionRuntime,
        messages: Vec<LlmMessage>,
        state: &mut TurnState,
    ) -> Result<(), TurnError> {
        let ctx = self.shared().provider_ctx(messages);
        match extension_runner
            .emit_provider(ProviderEvent::AfterResponse, ctx)
            .await?
        {
            ProviderResult::Block { reason } => {
                return Err(TurnError::ProviderBlocked { reason });
            },
            ProviderResult::ReplaceMessages { messages } => {
                if let Some(text) = extract_last_assistant_text(&messages) {
                    state.set_final_text(text);
                }
            },
            ProviderResult::AppendMessages { messages } => {
                let extra = extract_text_from_messages(&messages);
                if !extra.is_empty() {
                    state.append_final_text(&extra);
                }
            },
            ProviderResult::Allow => {},
        }
        extension_runner
            .emit_lifecycle(
                ExtensionEvent::AfterProviderResponse,
                self.shared().lifecycle_ctx(),
            )
            .await?;
        Ok(())
    }

    fn check_aborted(&self) -> Result<(), TurnError> {
        if self.cancellation_token.is_cancelled() {
            Err(TurnError::Aborted)
        } else {
            Ok(())
        }
    }

    /// 每个 agent step 开始前：重载读模型，返回自上次 step 以来新增的 durable user 消息条数。
    async fn sync_mid_turn_user_messages_at_step_start(
        &self,
        publisher: &Arc<TurnEvents>,
        state: &mut TurnState,
    ) -> Result<u32, TurnError> {
        publisher.invalidate_model_cache().await;
        let model = publisher.snapshot_model().await?;
        let current = count_visible_user_messages(&model);
        let previous = state.tracked_user_message_count();
        let synced = current.saturating_sub(previous) as u32;
        if synced > 0 {
            tracing::debug!(
                synced,
                previous,
                current,
                "mid-turn user messages synced into context for next step"
            );
        }
        state.set_tracked_user_message_count(current);
        Ok(synced)
    }

    async fn has_pending_mid_turn_user_messages(
        &self,
        publisher: &Arc<TurnEvents>,
        state: &TurnState,
    ) -> Result<bool, TurnError> {
        publisher.invalidate_model_cache().await;
        let model = publisher.snapshot_model().await?;
        Ok(has_pending_mid_turn_user_messages(
            &model,
            state.tracked_user_message_count(),
        ))
    }

    async fn should_continue_after_stop(
        &self,
        extension_runner: &dyn astrcode_kernel::ExtensionRuntime,
        assistant_text: &str,
        finish_reason: &str,
        state: &mut TurnState,
    ) -> Result<bool, TurnError> {
        let ctx = ContinueAfterStopContext {
            session_id: self.shared().session_id.to_string(),
            working_dir: self.shared().working_dir.clone(),
            model: self.shared().model_selection(),
            assistant_text: assistant_text.to_string(),
            finish_reason: finish_reason.to_string(),
            continuations_this_turn: state.continue_after_stop_count(),
        };
        let decision = extension_runner.emit_continue_after_stop(ctx).await?;
        if decision == ContinueAfterStopResult::ContinueOneStep {
            state.record_continue_after_stop();
            tracing::debug!("ContinueAfterStop: running one more agent step");
            return Ok(true);
        }
        Ok(false)
    }
}

fn extract_last_assistant_text(messages: &[LlmMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|m| m.role == LlmRole::Assistant)
        .map(|message| message.joined_text(""))
}

fn extract_text_from_messages(messages: &[LlmMessage]) -> String {
    LlmContent::join_text(messages.iter().flat_map(|message| &message.content), "")
}

enum ToolStageDecision {
    Continue,
    EndTurn { reason: String },
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
    agent: &mut TurnLoop,
    user_text: &str,
    turn_id: &TurnId,
) -> RunTurnResult {
    let (output, emitted_error) = drive_agent(agent, user_text, turn_id).await;

    RunTurnResult {
        output,
        emitted_error,
    }
}
