//! Compaction 协调 — turn 内 auto / reactive compact 的统一入口。

use std::sync::Arc;

use astrcode_core::{
    context::{
        CompactIfNeededOutcome, CompactMessagesOptions, CompactResult, CompactSummaryRenderOptions,
        ContextPrepareInput, PostCompactEnrichInput,
    },
    event::EventPayload,
    extension::{CompactStrategy, CompactTrigger},
    llm::LlmMessage,
    storage::SessionReadModel,
    types::TurnId,
};
use astrcode_kernel::ExtensionRuntime;
use astrcode_support::{hash::hex_fingerprint, sync::lock_parking};

use crate::{
    compact::{
        CompactHookContext, collect_compact_instructions, dispatch_post_compact,
        make_compact_request_fn, persist_compact_result,
    },
    deferred_tools::append_deferred_tools_reminder,
    llm_request_history::visible_messages_for_assembler,
    session::Session,
    turn_context::{SharedTurnContext, TurnError},
    turn_publish::TurnEvents,
    turn_stages::TurnState,
};

/// 一次 compaction 请求的参数。
pub(crate) struct CompactionRequest {
    pub trigger: CompactTrigger,
    pub strategy: CompactStrategy,
    /// 是否执行 compact（阈值 + 配置，或 reactive force）。
    pub run_compact: bool,
    /// compact 时是否调用 LLM（断路器关闭时仍可做确定性 compact）。
    pub use_llm_for_compact: bool,
    pub force_compact: bool,
    pub base_event_seq: u64,
    pub keep_recent_turns: Option<usize>,
}

/// context assembler 输出，含是否实际执行了 compaction。
pub(crate) struct PreparedContextMessages {
    pub context_messages: Vec<LlmMessage>,
    pub compaction_applied: bool,
}

#[derive(Clone)]
pub(crate) struct CompactionStageMeta {
    pub(crate) base_event_seq: u64,
    pub(crate) trigger: CompactTrigger,
    pub(crate) strategy: CompactStrategy,
    pub(crate) llm_api_failed: bool,
}

/// Turn 内 compaction 调用所需的 session / hook 上下文。
pub(crate) struct CompactionHost<'a> {
    pub session: &'a Session,
    pub llm: &'a Arc<dyn astrcode_core::llm::LlmProvider>,
    pub shared: &'a SharedTurnContext,
    pub extension_runner: &'a dyn ExtensionRuntime,
}

/// Turn 内 compaction 编排：system prompt 快照、assembler 调用与 persist。
pub(crate) struct Compaction {
    system_prompt: String,
    extra_system_prompt: Option<String>,
}

impl Compaction {
    pub(crate) fn new(system_prompt: String, extra_system_prompt: Option<String>) -> Self {
        Self {
            system_prompt,
            extra_system_prompt,
        }
    }

    pub(crate) fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    pub(crate) async fn refresh_system_prompt(
        &mut self,
        session: &Session,
        shared: &SharedTurnContext,
    ) -> Result<(), TurnError> {
        let Some(prompt) = session.current_system_prompt().await? else {
            return Ok(());
        };
        if prompt == self.system_prompt {
            return Ok(());
        }

        tracing::info!(
            session_id = %shared.session_id,
            "system_prompt changed mid-turn, refreshing"
        );
        self.system_prompt = prompt;
        Ok(())
    }

    pub(crate) async fn build_auto_compaction_request(
        &self,
        host: &CompactionHost<'_>,
        model: &SessionReadModel,
    ) -> Result<CompactionRequest, TurnError> {
        let context_assembler = host.session.caps().context_assembler_arc();
        let custom_instructions = self
            .compact_instructions(host, model, CompactTrigger::AutoThreshold)
            .await;
        let probe_input = ContextPrepareInput {
            messages: visible_messages_for_assembler(model),
            system_prompt: Some(self.system_prompt()),
            model_limits: host.llm.model_limits(),
            custom_instructions,
        };
        let threshold_met = context_assembler.should_auto_compact(&probe_input);
        let run_compact = context_assembler.auto_compact_enabled() && threshold_met;
        let use_llm_for_compact = run_compact
            && Self::should_attempt_llm_compact(host.session, CompactTrigger::AutoThreshold);
        let base_event_seq = if run_compact {
            Self::read_base_event_seq(host.session).await?
        } else {
            0
        };
        Ok(CompactionRequest {
            trigger: CompactTrigger::AutoThreshold,
            strategy: CompactStrategy::Auto,
            run_compact,
            use_llm_for_compact,
            force_compact: false,
            base_event_seq,
            keep_recent_turns: None,
        })
    }

    pub(crate) async fn prepare_context_messages(
        &self,
        host: &CompactionHost<'_>,
        state: &TurnState,
        model: &SessionReadModel,
        turn_id: &TurnId,
        request: CompactionRequest,
        publisher: &TurnEvents,
    ) -> Result<PreparedContextMessages, TurnError> {
        let context_assembler = host.session.caps().context_assembler_arc();
        let custom_instructions = self
            .compact_instructions(host, model, request.trigger)
            .await;
        let messages_before_compact = visible_messages_for_assembler(model);
        let input = ContextPrepareInput {
            messages: messages_before_compact.clone(),
            system_prompt: Some(self.system_prompt()),
            model_limits: host.llm.model_limits(),
            custom_instructions: custom_instructions.clone(),
        };
        let request_fn = make_compact_request_fn(Arc::clone(host.llm));
        let keep_recent_turns = request
            .keep_recent_turns
            .or_else(|| context_assembler.settings().compact_keep_recent_turns);
        let render_options = CompactSummaryRenderOptions {
            custom_instructions: custom_instructions.clone(),
            ..Default::default()
        };

        let compact_outcome = context_assembler
            .compact_if_needed(
                input.messages,
                input.system_prompt,
                &custom_instructions,
                render_options,
                CompactMessagesOptions {
                    run: request.force_compact || request.run_compact,
                    use_llm: request.use_llm_for_compact,
                    keep_recent_turns,
                },
                request_fn,
            )
            .await;

        let (mut context_messages, compaction_applied) = match compact_outcome {
            CompactIfNeededOutcome::NotRun { messages }
            | CompactIfNeededOutcome::Skipped { messages } => (messages, false),
            CompactIfNeededOutcome::Applied {
                messages,
                compaction,
            } => {
                let mut compaction_result = compaction.result;
                let persisted = Self::handle_compaction_stage(
                    host,
                    self.system_prompt(),
                    self.extra_system_prompt.as_deref(),
                    state,
                    model,
                    &mut compaction_result,
                    context_assembler.settings(),
                    turn_id,
                    CompactionStageMeta {
                        base_event_seq: request.base_event_seq,
                        trigger: request.trigger,
                        strategy: request.strategy,
                        llm_api_failed: compaction.llm_api_failed,
                    },
                )
                .await;
                if persisted {
                    (messages, true)
                } else {
                    tracing::warn!(
                        trigger = ?request.trigger,
                        "compaction generated but persist skipped; using pre-compact messages"
                    );
                    (messages_before_compact, false)
                }
            },
        };

        append_deferred_tools_reminder(
            &mut context_messages,
            state.all_tool_snapshots(),
            state.active_deferred_tools(),
        );

        if compaction_applied {
            publisher.reload_model_cache().await?;
        }

        Ok(PreparedContextMessages {
            context_messages,
            compaction_applied,
        })
    }

    pub(crate) async fn run_reactive_compaction(
        &mut self,
        host: &CompactionHost<'_>,
        state: &TurnState,
        turn_id: &TurnId,
        publisher: &TurnEvents,
    ) -> Result<bool, TurnError> {
        self.refresh_system_prompt(host.session, host.shared)
            .await?;
        let model = publisher.snapshot_model().await?;
        let base_event_seq = Self::read_base_event_seq(host.session).await?;
        let PreparedContextMessages {
            compaction_applied, ..
        } = self
            .prepare_context_messages(
                host,
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
                publisher,
            )
            .await?;
        Ok(compaction_applied)
    }

    pub(crate) async fn compact_instructions(
        &self,
        host: &CompactionHost<'_>,
        model: &SessionReadModel,
        trigger: CompactTrigger,
    ) -> Vec<String> {
        collect_compact_instructions(
            host.extension_runner,
            Self::compact_hook_context(host.shared, model, trigger),
        )
        .await
        .unwrap_or_default()
    }

    fn should_attempt_llm_compact(session: &Session, trigger: CompactTrigger) -> bool {
        match trigger {
            CompactTrigger::AutoThreshold => {
                lock_parking(session.runtime().compact_circuit_breaker()).should_attempt()
            },
            CompactTrigger::ManualCommand | CompactTrigger::ReactivePromptTooLong => true,
        }
    }

    async fn read_base_event_seq(session: &Session) -> Result<u64, TurnError> {
        let cursor = session.latest_cursor().await?;
        Ok(crate::session::parse_base_event_seq(cursor)?)
    }

    fn compact_hook_context<'a>(
        shared: &'a SharedTurnContext,
        model: &'a SessionReadModel,
        trigger: CompactTrigger,
    ) -> CompactHookContext<'a> {
        CompactHookContext {
            session_id: shared.session_id.as_str(),
            working_dir: &shared.working_dir,
            model_id: &shared.model_id,
            trigger,
            message_count: model
                .context_messages
                .len()
                .saturating_add(model.messages.len()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_compaction_stage(
        host: &CompactionHost<'_>,
        system_prompt: &str,
        extra_system_prompt: Option<&str>,
        state: &TurnState,
        model: &SessionReadModel,
        compaction: &mut CompactResult,
        settings: &astrcode_core::config::ContextSettings,
        turn_id: &TurnId,
        meta: CompactionStageMeta,
    ) -> bool {
        host.session
            .emit_live(Some(turn_id), EventPayload::CompactionStarted)
            .await;
        let visible_tools = state.visible_tools();
        let provider_messages = model.provider_messages();
        host.session
            .caps()
            .post_compact_enricher()
            .enrich(
                compaction,
                PostCompactEnrichInput {
                    session_id: host.shared.session_id.as_str(),
                    source_messages: &provider_messages,
                    working_dir: &host.shared.working_dir,
                    system_prompt: Some(system_prompt),
                    tools: &visible_tools,
                    settings,
                    session_store_dir: host.shared.session_store_dir.clone(),
                },
            )
            .await;
        let hook_ctx = Self::compact_hook_context(host.shared, model, meta.trigger);
        if let Err(e) = dispatch_post_compact(host.extension_runner, hook_ctx, compaction).await {
            tracing::warn!(error = %e, "PostCompact extension dispatch failed");
        }

        if meta.trigger == CompactTrigger::AutoThreshold && meta.llm_api_failed {
            lock_parking(host.session.runtime().compact_circuit_breaker()).record_llm_failure();
        }

        let fp = hex_fingerprint(system_prompt.as_bytes());
        let trigger_name = meta.trigger.as_str();
        match persist_compact_result(
            host.session,
            compaction,
            trigger_name,
            system_prompt,
            &fp,
            extra_system_prompt,
            meta.base_event_seq,
            meta.strategy,
        )
        .await
        {
            Ok(persisted) => {
                if meta.trigger == CompactTrigger::AutoThreshold && !meta.llm_api_failed {
                    lock_parking(host.session.runtime().compact_circuit_breaker())
                        .record_compact_success();
                }
                host.session
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
                host.session
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
}
