//! TurnLoop — 临时回合处理器与回合驱动。
//!
//! 负责处理一轮完整的对话：调用 LLM、执行工具调用、
//! 分发扩展钩子事件，并将事件流式传输给客户端。
//! Agent 是无状态的短暂对象，处理完一个回合后即被丢弃。

use std::{sync::Arc, time::Duration};

use astrcode_context::token_budget::{
    PromptTokenSnapshot, compact_threshold_tokens, request_max_output_tokens,
};
use astrcode_core::{
    event::{DurableEventPayload, LiveEventPayload},
    llm::{
        LlmContent, LlmError, LlmEvent, LlmMessage, LlmRequest, LlmRole, LlmTokenUsage,
        LlmTokenUsageSource, provider_visible_messages, token_estimate,
    },
    tool::ToolDefinition,
    types::*,
};
use astrcode_extension_sdk::extension::{
    ContinueAfterStopContext, ContinueAfterStopResult, ExtensionEvent, ProviderEvent,
    ProviderResult,
};
use astrcode_session_projection::SessionReadModel;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    compaction::{
        CompactCircuitBreaker, CompactionHost, PreparedProviderHistory, plan_auto_compaction,
        prepare_provider_history, run_reactive_compaction,
    },
    llm_stream::{StreamOutcome, consume_llm_stream, non_empty_reasoning_content},
    projection_context::context_snapshot,
    session::Session,
    steer::{count_visible_user_messages, has_pending_mid_turn_user_messages},
    tool_deduplicator::ToolCallDeduplicator,
    tool_exec::TurnToolContext,
    tool_pipeline::ToolCalls,
    tool_types::ExecuteToolBatch,
    turn_context::{
        SharedTurnContext, TurnError, end_turn_with_error_typed, on_step_end_best_effort,
    },
    turn_publish::{ExtensionEvents, TurnEvents},
    turn_stages::{PreparedProviderRequest, TurnState},
};

/// 运行 agent 的一次 process_prompt；durable 在 turn 内同步写入，live 经 TurnEvents 直发。
///
/// 返回的 `emitted_error` 表示 turn 内是否已持久化 durable ErrorOccurred（`TurnEvents`
/// 内部标志）；它只在 turn 结束后立即读取有效，finalizer 据此避免重复补发错误事件。
pub(crate) async fn drive_agent(
    agent: &mut TurnLoop,
    user_text: &str,
    turn_id: &TurnId,
) -> (Result<TurnOutput, TurnError>, bool) {
    let publisher = Arc::new(TurnEvents::new(agent.session().clone(), turn_id.clone()));
    let output = agent.process_prompt(user_text, turn_id, &publisher).await;
    (output, publisher.emitted_error())
}

/// 一次 turn 的临时执行器；durable projection 是跨 turn 状态。
pub(crate) struct TurnLoop {
    session: Session,
    llm: Arc<dyn astrcode_core::llm::LlmProvider>,
    cancellation_token: CancellationToken,
    tools: ToolCalls,
    compact_circuit_breaker: Mutex<CompactCircuitBreaker>,
}

/// Step 阶段间共享的 hook/publisher/lifecycle 上下文。
struct StepHooks<'a> {
    extension_runner: &'a dyn astrcode_extension_sdk::runtime_ports::TurnHooks,
    lifecycle_ctx: &'a astrcode_extension_sdk::extension::LifecycleContext,
    publisher: &'a Arc<TurnEvents>,
}

/// LLM 请求被消费前抓取的快照，供 outcome 后续阶段使用。
struct LlmRequestSnapshot {
    messages: Vec<LlmMessage>,
    context_window: usize,
}

impl TurnLoop {
    pub(crate) fn session(&self) -> &Session {
        &self.session
    }

    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_token.clone()
    }

    fn max_parallel_tool_calls(&self) -> usize {
        self.session.runtime_services().max_parallel_tool_calls()
    }

    fn shared(&self) -> &SharedTurnContext {
        self.tools.shared()
    }

    async fn recover_context_overflow(
        &self,
        extension_runner: &dyn astrcode_extension_sdk::runtime_ports::TurnHooks,
        state: &mut TurnState,
        turn_id: &TurnId,
        publisher: &TurnEvents,
    ) -> Result<bool, TurnError> {
        if state.reactive_compact_used() {
            return Ok(false);
        }

        state.mark_reactive_compact_used();
        let shared = self.shared().clone();
        let host = CompactionHost {
            session: &self.session,
            llm: &self.llm,
            shared: &shared,
            extension_runner,
            breaker: &self.compact_circuit_breaker,
        };
        run_reactive_compaction(&host, state, turn_id, publisher).await
    }

    pub(crate) fn new_with_llm(
        session: Session,
        session_state: &SessionReadModel,
        tool_selection: astrcode_core::tool::SessionToolSelection,
        session_store_dir: Option<std::path::PathBuf>,
        llm: Arc<dyn astrcode_core::llm::LlmProvider>,
        tool_registry: Arc<crate::ToolRegistry>,
        cancellation_token: CancellationToken,
    ) -> Result<Self, TurnError> {
        let runtime_services = session.runtime_services();
        let turn =
            TurnToolContext::for_turn(&session, session_state, tool_selection, session_store_dir);
        let tools = ToolCalls::new(
            turn,
            tool_registry,
            runtime_services.turn_hooks_arc(),
            session.clone(),
            cancellation_token.clone(),
        );
        let context_settings = runtime_services.context_assembler().settings();
        let compact_circuit_breaker = Mutex::new(CompactCircuitBreaker::new(
            context_settings.compact_circuit_breaker_threshold,
            Duration::from_secs(context_settings.compact_circuit_breaker_cooldown_secs),
        ));
        Ok(Self {
            session,
            llm,
            cancellation_token,
            tools,
            compact_circuit_breaker,
        })
    }

    pub(crate) async fn process_prompt(
        &mut self,
        user_text: &str,
        turn_id: &TurnId,
        publisher: &Arc<TurnEvents>,
    ) -> Result<TurnOutput, TurnError> {
        let extension_runner = self.session().runtime_services().turn_hooks_arc();
        let event_bridge = ExtensionEvents::start(Arc::clone(publisher), self.tools.shared_mut());
        let result = self
            .process_prompt_inner(user_text, turn_id, publisher)
            .await;
        if result.is_err() {
            self.finalize_turn_on_error(extension_runner.as_ref()).await;
        }
        let ingress_result = event_bridge.shutdown(self.tools.shared_mut()).await;
        match (result, ingress_result) {
            (Ok(output), Ok(())) => Ok(output),
            (Ok(_), Err(error)) => Err(error),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(ingress_error)) => {
                tracing::warn!(
                    error = %ingress_error,
                    "turn event ingress also failed while the turn was failing"
                );
                Err(error)
            },
        }
    }

    /// Turn 失败时统一补发 `TurnEnd`，避免 `?` 旁路错误漏掉扩展生命周期钩子。
    async fn finalize_turn_on_error(
        &self,
        extension_runner: &dyn astrcode_extension_sdk::runtime_ports::TurnHooks,
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
        user_text: &str,
        turn_id: &TurnId,
        publisher: &Arc<TurnEvents>,
    ) -> Result<TurnOutput, TurnError> {
        let all_tools = self.tools.list_definitions_with_prompt_metadata();
        let extension_runner = self.session().runtime_services().turn_hooks_arc();

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
        match publisher.snapshot_model().await {
            Ok(model) => state.set_synced_user_message_count(count_visible_user_messages(&model)),
            Err(error) => {
                // 降级为 0 会让首 step 把全部历史 user 消息误判为 mid-turn 新增，必须可观测。
                tracing::warn!(
                    error = %error,
                    "failed to snapshot model for mid-turn user message tracking; \
                     treating all user messages as unsynced"
                );
            },
        }

        // Step
        loop {
            self.check_aborted()?;
            match self
                .run_one_step(
                    &mut state,
                    extension_runner.as_ref(),
                    &lifecycle_ctx,
                    turn_id,
                    publisher,
                    user_text,
                )
                .await?
            {
                StepOutcome::Continue => continue,
                StepOutcome::Finished(output) => return Ok(output),
            }
        }
    }

    /// 单个 agent step：begin/end_step 的配对由本函数保证——无论 step 以何种方式结束，
    /// 已注册的 tool call key 都会进入跨 step 连续重复统计（end_step 对无工具调用的
    /// step 是 no-op；历史实现只在 ToolCalls 分支收口，早退路径会漏计已执行的 early 工具）。
    async fn run_one_step(
        &mut self,
        state: &mut TurnState,
        extension_runner: &dyn astrcode_extension_sdk::runtime_ports::TurnHooks,
        lifecycle_ctx: &astrcode_extension_sdk::extension::LifecycleContext,
        turn_id: &TurnId,
        publisher: &Arc<TurnEvents>,
        user_text: &str,
    ) -> Result<StepOutcome, TurnError> {
        state.begin_step();
        let result = self
            .step_body(
                state,
                extension_runner,
                lifecycle_ctx,
                turn_id,
                publisher,
                user_text,
            )
            .await;
        state.tool_deduplicator_mut().end_step();
        result
    }

    async fn step_body(
        &mut self,
        state: &mut TurnState,
        extension_runner: &dyn astrcode_extension_sdk::runtime_ports::TurnHooks,
        lifecycle_ctx: &astrcode_extension_sdk::extension::LifecycleContext,
        turn_id: &TurnId,
        publisher: &Arc<TurnEvents>,
        user_text: &str,
    ) -> Result<StepOutcome, TurnError> {
        let mid_turn_synced = self.sync_mid_turn_user_messages(publisher, state).await?;
        let step_ctx = lifecycle_ctx.clone().for_step_start(mid_turn_synced);

        extension_runner
            .emit_lifecycle(ExtensionEvent::StepStart, step_ctx)
            .await?;

        let visible_tools = state.visible_tools();
        let prepared = match self
            .prepare_stage(extension_runner, state, &visible_tools, turn_id, publisher)
            .await
        {
            Ok(prepared) => prepared,
            Err(TurnError::Llm(LlmError::ContextWindowExceeded { .. })) => {
                return self
                    .recover_or_fail(extension_runner, state, turn_id, publisher)
                    .await;
            },
            Err(error) => return Err(error),
        };

        // 提取 deduplicator 用于流式工具执行；llm_stage 返回后归还。
        // visible_tools 传给 early exec context 供 prepare 使用。
        let request = LlmRequestSnapshot {
            messages: prepared.messages.clone(),
            context_window: prepared.llm.model_limits().max_input_tokens,
        };
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
            Err(TurnError::Llm(LlmError::ContextWindowExceeded { .. })) => {
                return self
                    .recover_or_fail(extension_runner, state, turn_id, publisher)
                    .await;
            },
            Err(error) => return Err(error),
        };

        let hooks = StepHooks {
            extension_runner,
            lifecycle_ctx,
            publisher,
        };
        match outcome {
            StreamOutcome::Complete { .. } => {
                self.complete_stage(&hooks, state, outcome, user_text, request)
                    .await
            },
            StreamOutcome::ToolCalls { .. } => {
                self.tool_calls_stage(&hooks, state, outcome, request).await
            },
        }
    }

    async fn complete_stage(
        &self,
        hooks: &StepHooks<'_>,
        state: &mut TurnState,
        outcome: StreamOutcome,
        user_text: &str,
        request: LlmRequestSnapshot,
    ) -> Result<StepOutcome, TurnError> {
        let StreamOutcome::Complete {
            text,
            reasoning_content,
            finish_reason,
            message_id,
            message_started,
            usage,
        } = outcome
        else {
            unreachable!("complete_stage expects StreamOutcome::Complete");
        };

        let reasoning_content = non_empty_reasoning_content(reasoning_content);
        let assistant_text_for_continue = text.clone();
        state.record_assistant_text(&text, reasoning_content.clone());
        if (!text.is_empty() || reasoning_content.is_some()) && message_started {
            hooks
                .publisher
                .durable(DurableEventPayload::AssistantMessageCompleted {
                    message_id,
                    text,
                    reasoning_content,
                })
                .await?;
        }
        self.persist_token_usage(hooks.publisher, usage, request.context_window)
            .await?;
        on_step_end_best_effort(hooks.extension_runner, hooks.lifecycle_ctx).await;

        if self
            .should_continue_after_stop(
                hooks.extension_runner,
                &assistant_text_for_continue,
                &finish_reason,
                state,
            )
            .await?
        {
            return Ok(StepOutcome::Continue);
        }

        if self
            .has_pending_mid_turn_user_messages(hooks.publisher, state)
            .await?
        {
            tracing::debug!("pending mid-turn user messages; running one more agent step");
            return Ok(StepOutcome::Continue);
        }

        let hook_messages = state.provider_response_messages(request.messages);
        let output = self
            .postprocess_complete_stage(
                hooks.extension_runner,
                user_text.to_string(),
                state,
                finish_reason,
                hook_messages,
            )
            .await?;
        Ok(StepOutcome::Finished(output))
    }

    async fn tool_calls_stage(
        &self,
        hooks: &StepHooks<'_>,
        state: &mut TurnState,
        outcome: StreamOutcome,
        request: LlmRequestSnapshot,
    ) -> Result<StepOutcome, TurnError> {
        let StreamOutcome::ToolCalls {
            text,
            reasoning_content,
            tool_calls,
            early_results,
            message_id,
            message_started,
            usage,
        } = outcome
        else {
            unreachable!("tool_calls_stage expects StreamOutcome::ToolCalls");
        };

        let reasoning_content = non_empty_reasoning_content(reasoning_content);
        let visible_text = text.as_deref().unwrap_or_default();
        state.record_assistant_tool_calls(visible_text, reasoning_content.clone(), &tool_calls);
        if !tool_calls.is_empty() || message_started {
            if !message_started {
                hooks
                    .publisher
                    .live(LiveEventPayload::AssistantMessageStarted {
                        message_id: message_id.clone(),
                    });
            }
            hooks
                .publisher
                .durable(DurableEventPayload::AssistantMessageCompleted {
                    message_id,
                    text: visible_text.to_string(),
                    reasoning_content,
                })
                .await?;
        }
        self.persist_token_usage(hooks.publisher, usage, request.context_window)
            .await?;

        let hook_messages = state.provider_response_messages(request.messages);
        self.tools_stage(
            hooks.extension_runner,
            state,
            &tool_calls,
            early_results,
            hooks.publisher,
            hook_messages,
        )
        .await?;

        on_step_end_best_effort(hooks.extension_runner, hooks.lifecycle_ctx).await;
        Ok(StepOutcome::Continue)
    }

    /// 上下文溢出恢复：reactive compaction 成功则继续 step，否则返回 `CompactExhausted`。
    async fn recover_or_fail(
        &self,
        extension_runner: &dyn astrcode_extension_sdk::runtime_ports::TurnHooks,
        state: &mut TurnState,
        turn_id: &TurnId,
        publisher: &TurnEvents,
    ) -> Result<StepOutcome, TurnError> {
        if self
            .recover_context_overflow(extension_runner, state, turn_id, publisher)
            .await?
        {
            Ok(StepOutcome::Continue)
        } else {
            Err(TurnError::CompactExhausted)
        }
    }

    async fn prepare_stage(
        &mut self,
        extension_runner: &dyn astrcode_extension_sdk::runtime_ports::TurnHooks,
        state: &TurnState,
        visible_tools: &[ToolDefinition],
        turn_id: &TurnId,
        publisher: &Arc<TurnEvents>,
    ) -> Result<PreparedProviderRequest, TurnError> {
        let shared = self.shared().clone();
        let host = CompactionHost {
            session: &self.session,
            llm: &self.llm,
            shared: &shared,
            extension_runner,
            breaker: &self.compact_circuit_breaker,
        };
        let model = publisher.snapshot_model().await?;
        let snapshot = context_snapshot(&model);
        let llm = Arc::clone(host.llm);
        let compaction_plan = plan_auto_compaction(&host, &snapshot, visible_tools).await;

        let PreparedProviderHistory { snapshot, messages } =
            prepare_provider_history(&host, state, snapshot, turn_id, compaction_plan, publisher)
                .await?;

        let mut messages = snapshot.request_messages(messages);
        if let Some(reminder) = state.tool_deduplicator().check_reminder() {
            tracing::debug!("injecting tool deduplication system-reminder");
            messages.push(LlmMessage::user(reminder));
        }
        let messages = self
            .apply_before_provider_request_hook(extension_runner, messages)
            .await?;
        let model_limits = llm.model_limits();
        let model_context_window = model_limits.max_input_tokens;
        let token_snapshot = PromptTokenSnapshot {
            context_tokens: snapshot.estimate_input_tokens(
                &messages,
                visible_tools,
                model_context_window,
            ),
            threshold_tokens: compact_threshold_tokens(
                model_context_window,
                self.session
                    .runtime_services()
                    .context_assembler()
                    .settings()
                    .compact_threshold_percent,
            ),
            max_input_tokens: model_context_window,
            max_output_tokens: model_limits.max_output_tokens,
        };
        let max_output_tokens =
            request_max_output_tokens(token_snapshot, llm.minimum_output_tokens()).ok_or_else(
                || {
                    TurnError::Llm(LlmError::ContextWindowExceeded {
                        message: "local context budget leaves no output capacity".into(),
                    })
                },
            )?;
        Ok(PreparedProviderRequest {
            llm,
            messages,
            max_output_tokens,
        })
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
            .start_provider_stream(
                &prepared.llm,
                prepared.messages,
                tools,
                prepared.max_output_tokens,
                publisher,
            )
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
            Err(e @ TurnError::Llm(LlmError::ContextWindowExceeded { .. })) => Err(e),
            Err(error) => end_turn_with_error_typed(error),
        }
    }

    /// 持久化 token usage（若 provider 提供了统计）。
    async fn persist_token_usage(
        &self,
        publisher: &TurnEvents,
        usage: Option<LlmTokenUsage>,
        model_context_window: usize,
    ) -> Result<(), TurnError> {
        if let Some(usage) = usage {
            publisher
                .durable(DurableEventPayload::TokenUsageRecorded {
                    usage,
                    model_context_window,
                })
                .await?;
        }
        Ok(())
    }

    async fn with_usage_fallback(
        &self,
        mut outcome: StreamOutcome,
        llm: &Arc<dyn astrcode_core::llm::LlmProvider>,
        request_messages: Vec<LlmMessage>,
        tools: &[ToolDefinition],
    ) -> StreamOutcome {
        match &mut outcome {
            StreamOutcome::Complete { usage, .. } | StreamOutcome::ToolCalls { usage, .. } => {
                if usage.is_none() {
                    *usage = self
                        .fallback_token_usage(llm, request_messages, tools)
                        .await;
                }
            },
        }
        outcome
    }

    async fn fallback_token_usage(
        &self,
        llm: &Arc<dyn astrcode_core::llm::LlmProvider>,
        request_messages: Vec<LlmMessage>,
        tools: &[ToolDefinition],
    ) -> Option<LlmTokenUsage> {
        let effective = self.session.runtime_services().read_effective();
        let (input_tokens, source) = match llm
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
                (
                    Some(count.input_tokens),
                    LlmTokenUsageSource::ProviderCountFallback,
                )
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
                (
                    Some(token_estimate::estimate_request_tokens(&request_messages, None) as u64),
                    LlmTokenUsageSource::LocalEstimateFallback,
                )
            },
        };
        Some(LlmTokenUsage {
            input_tokens,
            cached_input_tokens: None,
            cache_creation_input_tokens: None,
            output_tokens: None,
            reasoning_output_tokens: None,
            total_tokens: None,
            source: Some(source),
        })
    }

    async fn tools_stage(
        &self,
        extension_runner: &dyn astrcode_extension_sdk::runtime_ports::TurnHooks,
        state: &mut TurnState,
        tool_calls: &[crate::tool_types::StreamedToolCall],
        early_results: Vec<crate::early_tool_scheduler::EarlyExecutionEntry>,
        publisher: &Arc<TurnEvents>,
        hook_messages: Vec<LlmMessage>,
    ) -> Result<(), TurnError> {
        self.dispatch_after_provider_response(extension_runner, hook_messages, state)
            .await?;

        let visible_tools = state.visible_tools();

        let plan = self
            .tools
            .prepare_tool_batch(tool_calls, early_results, &visible_tools, state)
            .await?;
        self.tools.declare_tool_batch(&plan, publisher).await?;

        let discovered_tools = match self
            .tools
            .execute_and_commit(ExecuteToolBatch {
                batch: plan,
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
        state.activate_deferred_tools(discovered_tools);
        Ok(())
    }

    async fn postprocess_complete_stage(
        &self,
        extension_runner: &dyn astrcode_extension_sdk::runtime_ports::TurnHooks,
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

    /// 运行 `BeforeRequest` 扩展钩子。返回值覆盖 LLM 请求的 messages。
    ///
    /// `send_messages.clone()` 不可消除：`ProviderContext` 持有 `Vec<LlmMessage>` 所有权，
    /// 而 `emit_provider` 需 `&self` 借用。消除 clone 是 extension-sdk 的 API 演进
    /// （`ProviderContext.messages` 改 `Arc<Vec<LlmMessage>>` + copy-on-write），不在本任务范围。
    async fn apply_before_provider_request_hook(
        &self,
        extension_runner: &dyn astrcode_extension_sdk::runtime_ports::TurnHooks,
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
        max_output_tokens: usize,
        publisher: &TurnEvents,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, TurnError> {
        let result = tokio::select! {
            _ = self.cancellation_token.cancelled() => return Err(TurnError::Aborted),
            result = llm.generate_request(
                LlmRequest::new(send_messages, tools.to_vec())
                    .with_max_output_tokens(max_output_tokens)
            ) => result,
        };
        match result {
            Ok(rx) => Ok(rx),
            Err(LlmError::ContextWindowExceeded { message }) => {
                Err(TurnError::Llm(LlmError::ContextWindowExceeded { message }))
            },
            Err(e) => {
                publisher
                    .durable_error(
                        crate::payload::JSON_RPC_INTERNAL_ERROR,
                        e.to_string(),
                        false,
                    )
                    .await?;
                end_turn_with_error_typed(e)
            },
        }
    }

    async fn dispatch_after_provider_response(
        &self,
        extension_runner: &dyn astrcode_extension_sdk::runtime_ports::TurnHooks,
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
    async fn sync_mid_turn_user_messages(
        &self,
        publisher: &TurnEvents,
        state: &mut TurnState,
    ) -> Result<u32, TurnError> {
        let model = publisher.snapshot_model().await?;
        let current = count_visible_user_messages(&model);
        let previous = state.synced_user_message_count();
        let synced = current.saturating_sub(previous) as u32;
        if synced > 0 {
            tracing::debug!(
                synced,
                previous,
                current,
                "mid-turn user messages synced into context for next step"
            );
        }
        state.set_synced_user_message_count(current);
        Ok(synced)
    }

    async fn has_pending_mid_turn_user_messages(
        &self,
        publisher: &TurnEvents,
        state: &TurnState,
    ) -> Result<bool, TurnError> {
        let model = publisher.snapshot_model().await?;
        Ok(has_pending_mid_turn_user_messages(
            &model,
            state.synced_user_message_count(),
        ))
    }

    async fn should_continue_after_stop(
        &self,
        extension_runner: &dyn astrcode_extension_sdk::runtime_ports::TurnHooks,
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

#[derive(Debug)]
pub struct TurnOutput {
    pub text: String,
    pub finish_reason: String,
    pub tool_results: Vec<astrcode_core::tool::ToolResult>,
}

/// 单个 agent step 的结果：`Continue` 进入下一 step，`Finished` 结束 turn。
enum StepOutcome {
    Continue,
    Finished(TurnOutput),
}

#[derive(Debug, Clone)]
pub struct TurnFinalization {
    pub finish_reason: String,
    pub pending_error: Option<String>,
    pub aborted: bool,
    pub terminal_persisted: bool,
}

impl TurnFinalization {
    /// 由 session 在 turn 终态持久化成功后调用；server 的 registry 依据该标志
    /// 决定是否保留所有权重试收尾。必须在写入 `SharedTurnFinalization` 之前调用。
    pub fn mark_persisted(&mut self) {
        self.terminal_persisted = true;
    }
}

pub struct RunTurnResult {
    pub output: Result<TurnOutput, TurnError>,
    pub finalization: TurnFinalization,
}

pub(crate) async fn run_turn(
    agent: &mut TurnLoop,
    user_text: &str,
    turn_id: &TurnId,
) -> RunTurnResult {
    let (output, emitted_error) = drive_agent(agent, user_text, turn_id).await;
    let aborted = matches!(output, Err(TurnError::Aborted));
    let finish_reason = match &output {
        Ok(output) => output.finish_reason.clone(),
        Err(TurnError::Aborted) => crate::payload::TURN_FINISH_ABORTED.into(),
        Err(_) => crate::payload::TURN_FINISH_ERROR.into(),
    };
    // 三种组合都不需要 finalizer 补发 durable ErrorOccurred：
    // - 用户中止：finalize_aborted_turn 自行处理；
    // - emitted_error=true：durable error 已由 TurnEvents 发出，补发会重复；
    // - 成功且无错误：无事可补。
    let pending_error = match (&output, emitted_error) {
        (Err(TurnError::Aborted), _) | (_, true) | (Ok(_), false) => None,
        (Err(error), false) => Some(error.to_string()),
    };

    RunTurnResult {
        output,
        finalization: TurnFinalization {
            finish_reason,
            pending_error,
            aborted,
            terminal_persisted: false,
        },
    }
}
