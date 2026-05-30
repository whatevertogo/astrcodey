//! TurnLoop — 临时回合处理器与回合驱动。
//!
//! 负责处理一轮完整的对话：调用 LLM、执行工具调用、
//! 分发扩展钩子事件，并将事件流式传输给客户端。
//! Agent 是无状态的短暂对象，处理完一个回合后即被丢弃。

use std::{sync::Arc, time::Duration};

use astrcode_core::{
    event::EventPayload,
    extension::{ExtensionEvent, ProviderEvent, ProviderResult},
    llm::{LlmError, LlmEvent, LlmMessage, provider_visible_messages},
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
    tool_exec::TurnToolContext,
    tool_pipeline::ToolCalls,
    tool_types::ExecuteToolCalls,
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
            self.finalize_turn_on_error(&extension_runner).await;
        }
        event_bridge.shutdown(self.tools.shared_mut()).await;
        result
    }

    /// Turn 失败时统一补发 `TurnEnd`，避免 `?` 旁路错误漏掉扩展生命周期钩子。
    async fn finalize_turn_on_error(
        &self,
        extension_runner: &astrcode_extensions::runner::ExtensionRunner,
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

        loop {
            self.check_aborted()?;
            publisher.invalidate_model_cache().await;

            extension_runner
                .emit_lifecycle(ExtensionEvent::StepStart, lifecycle_ctx.clone())
                .await?;

            let prepared = self
                .prepare_stage(&extension_runner, &state, turn_id, publisher)
                .await?;
            let visible_tools = state.visible_tools();
            let outcome = match self.llm_stage(prepared, &visible_tools, publisher).await {
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
                        extension_runner: &extension_runner,
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
                                reasoning_content,
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
        let compaction_request = self
            .compaction
            .build_auto_compaction_request(&host, &model)
            .await?;

        let PreparedContextMessages {
            context_messages,
            compaction_applied: _,
        } = self
            .compaction
            .prepare_context_messages(&host, state, &model, turn_id, compaction_request, publisher)
            .await?;

        let messages = build_llm_request_messages(self.system_prompt(), context_messages);
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
    ) -> Result<StreamOutcome, TurnError> {
        let rx = self
            .start_provider_stream(&prepared.llm, prepared.messages, tools, publisher)
            .await?;
        let message_id = new_message_id();
        match consume_llm_stream(rx, publisher, message_id, &self.cancellation_token).await {
            Ok(outcome) => Ok(outcome),
            Err(e @ TurnError::Llm(LlmError::PromptTooLong(_))) => Err(e),
            Err(error) => end_turn_with_error_typed(error),
        }
    }

    async fn tools_stage(
        &self,
        extension_runner: &astrcode_extensions::runner::ExtensionRunner,
        state: &mut TurnState,
        tool_calls: &[crate::tool_types::PendingToolCall],
        publisher: &Arc<TurnEvents>,
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
                return end_turn_with_error_typed(error);
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
                return end_turn_with_error_typed(error);
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

    async fn apply_before_provider_request_hook(
        &self,
        extension_runner: &astrcode_extensions::runner::ExtensionRunner,
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
                publisher.live_error(-32603, e.to_string(), false).await;
                end_turn_with_error_typed(e)
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
                self.shared().lifecycle_ctx(),
            )
            .await
        {
            return end_turn_with_error_typed(e);
        }
        Ok(())
    }

    fn check_aborted(&self) -> Result<(), TurnError> {
        if self.cancellation_token.is_cancelled() {
            Err(TurnError::Aborted)
        } else {
            Ok(())
        }
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
