//! Turn 内 auto / reactive compaction 协调。

use std::sync::Arc;

use astrcode_context::{
    CompactResult, CompactSummaryRenderOptions, ContextPrepareInput, ContextSnapshot,
    PostCompactEnrichInput,
    compaction::{compact_messages_deterministic, compact_messages_with_fallback},
};
use astrcode_core::{
    compaction::{CompactStrategy, CompactTrigger},
    event::LiveEventPayload,
    llm::{LlmMessage, LlmProvider},
    tool::ToolDefinition,
    types::TurnId,
};
use astrcode_extension_sdk::runtime_ports::TurnHooks;
use astrcode_support::sync::lock_parking;

use crate::{
    compact::{
        CompactHookContext, PersistCompactionOutcome, collect_compact_instructions,
        dispatch_post_compact, persist_compact_result, request_compact_summary,
    },
    deferred_tools::append_deferred_tools_reminder,
    projection_context::context_snapshot,
    session::Session,
    turn_context::{SharedTurnContext, TurnError},
    turn_publish::TurnEvents,
    turn_stages::TurnState,
};

pub(crate) struct CompactionRequest {
    pub trigger: CompactTrigger,
    pub strategy: CompactStrategy,
    pub run_compact: bool,
    pub use_llm_for_compact: bool,
    pub keep_recent_turns: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionOutcome {
    NotRun,
    Committed,
    Stale,
}

pub(crate) struct PreparedProviderHistory {
    pub snapshot: ContextSnapshot,
    pub messages: Vec<LlmMessage>,
    pub outcome: CompactionOutcome,
}

struct CompactionStageMeta {
    trigger: CompactTrigger,
    strategy: CompactStrategy,
    llm_api_failed: bool,
}

pub(crate) struct CompactionHost<'a> {
    pub session: &'a Session,
    pub llm: &'a Arc<dyn LlmProvider>,
    pub shared: &'a SharedTurnContext,
    pub extension_runner: &'a dyn TurnHooks,
}

async fn try_provider_input_tokens(
    session: &Session,
    llm: &Arc<dyn LlmProvider>,
    messages: Vec<LlmMessage>,
    tools: &[ToolDefinition],
    stage: &'static str,
) -> Option<usize> {
    match llm.count_input_tokens(messages, tools.to_vec()).await {
        Ok(count) => match usize::try_from(count.input_tokens) {
            Ok(tokens) => Some(tokens),
            Err(_) => {
                let effective = session.runtime_services().read_effective();
                tracing::warn!(
                    provider = %effective.llm.provider_kind,
                    model = %effective.llm.model_id,
                    stage,
                    input_tokens = count.input_tokens,
                    max_tokens = usize::MAX,
                    "provider input token count exceeds local usize range; clamping to usize::MAX"
                );
                Some(usize::MAX)
            },
        },
        Err(error) => {
            let effective = session.runtime_services().read_effective();
            tracing::warn!(
                provider = %effective.llm.provider_kind,
                model = %effective.llm.model_id,
                stage,
                error = %error,
                "provider input token count unavailable; falling back to local estimate"
            );
            None
        },
    }
}

pub(crate) async fn build_auto_compaction_request(
    host: &CompactionHost<'_>,
    snapshot: &ContextSnapshot,
    tools: &[ToolDefinition],
) -> CompactionRequest {
    let context_assembler = host.session.runtime_services().context_assembler_arc();
    let provider_input_tokens = try_provider_input_tokens(
        host.session,
        host.llm,
        snapshot.request_messages(snapshot.messages.clone()),
        tools,
        "compact_gate",
    )
    .await;
    let threshold_met = context_assembler.should_auto_compact(&ContextPrepareInput {
        messages: snapshot.messages.clone(),
        system_prompt: Some(&snapshot.system_prompt),
        model_limits: host.llm.model_limits(),
        provider_input_tokens,
    });
    let run_compact = context_assembler.auto_compact_enabled() && threshold_met;
    CompactionRequest {
        trigger: CompactTrigger::AutoThreshold,
        strategy: CompactStrategy::Auto,
        run_compact,
        use_llm_for_compact: run_compact && should_attempt_auto_llm_compact(host.session),
        keep_recent_turns: None,
    }
}

pub(crate) async fn prepare_provider_history(
    host: &CompactionHost<'_>,
    state: &TurnState,
    initial_snapshot: ContextSnapshot,
    turn_id: &TurnId,
    request: CompactionRequest,
    publisher: &TurnEvents,
) -> Result<PreparedProviderHistory, TurnError> {
    let context_assembler = host.session.runtime_services().context_assembler_arc();
    let (snapshot, outcome) = if request.run_compact {
        let custom_instructions = collect_compact_instructions(
            host.extension_runner,
            compact_hook_context(
                host.shared,
                initial_snapshot.messages.len(),
                request.trigger,
            ),
        )
        .await;

        // PreCompact may append durable facts. Compact must start from the projection after the
        // hook, not from the probe snapshot used only to decide whether compaction is needed.
        publisher.reload_model_cache().await?;
        let source_snapshot = context_snapshot(&publisher.snapshot_model().await?);
        match custom_instructions {
            Err(error) => {
                tracing::warn!(error = %error, "PreCompact extension dispatch failed");
                (source_snapshot, CompactionOutcome::NotRun)
            },
            Ok(custom_instructions) => {
                let keep_recent_turns = request
                    .keep_recent_turns
                    .or_else(|| context_assembler.settings().compact_keep_recent_turns);
                let render_options = CompactSummaryRenderOptions {
                    custom_instructions: custom_instructions.clone(),
                    ..Default::default()
                };
                let execution = if request.use_llm_for_compact {
                    compact_messages_with_fallback(
                        &source_snapshot.messages,
                        Some(&source_snapshot.system_prompt),
                        context_assembler.settings(),
                        &custom_instructions,
                        &render_options,
                        keep_recent_turns,
                        |messages| request_compact_summary(Arc::clone(host.llm), messages),
                    )
                    .await
                } else {
                    compact_messages_deterministic(
                        &source_snapshot.messages,
                        Some(&source_snapshot.system_prompt),
                        &render_options,
                        keep_recent_turns,
                    )
                };

                match execution {
                    Err(_) => (source_snapshot, CompactionOutcome::NotRun),
                    Ok(execution) => {
                        let mut compaction = execution.result;
                        update_compaction_token_counts(
                            host,
                            state,
                            &source_snapshot,
                            &mut compaction,
                        )
                        .await;
                        let outcome = commit_compaction(
                            host,
                            state,
                            &source_snapshot,
                            &mut compaction,
                            context_assembler.settings(),
                            turn_id,
                            CompactionStageMeta {
                                trigger: request.trigger,
                                strategy: request.strategy,
                                llm_api_failed: execution.llm_api_failed,
                            },
                        )
                        .await;
                        publisher.reload_model_cache().await?;
                        (
                            context_snapshot(&publisher.snapshot_model().await?),
                            outcome,
                        )
                    },
                }
            },
        }
    } else {
        (initial_snapshot, CompactionOutcome::NotRun)
    };

    let mut messages = snapshot.messages.clone();
    append_deferred_tools_reminder(
        &mut messages,
        state.all_tool_snapshots(),
        state.active_deferred_tools(),
    );
    let messages = context_assembler
        .prepare_messages(ContextPrepareInput {
            messages,
            system_prompt: Some(&snapshot.system_prompt),
            model_limits: host.llm.model_limits(),
            provider_input_tokens: None,
        })
        .messages;

    Ok(PreparedProviderHistory {
        snapshot,
        messages,
        outcome,
    })
}

pub(crate) async fn run_reactive_compaction(
    host: &CompactionHost<'_>,
    state: &TurnState,
    turn_id: &TurnId,
    publisher: &TurnEvents,
) -> Result<bool, TurnError> {
    publisher.reload_model_cache().await?;
    let snapshot = context_snapshot(&publisher.snapshot_model().await?);
    let prepared = prepare_provider_history(
        host,
        state,
        snapshot,
        turn_id,
        CompactionRequest {
            trigger: CompactTrigger::ReactivePromptTooLong,
            strategy: CompactStrategy::ReactivePromptTooLong,
            run_compact: true,
            use_llm_for_compact: true,
            keep_recent_turns: None,
        },
        publisher,
    )
    .await?;
    Ok(prepared.outcome != CompactionOutcome::NotRun)
}

async fn update_compaction_token_counts(
    host: &CompactionHost<'_>,
    state: &TurnState,
    snapshot: &ContextSnapshot,
    compaction: &mut CompactResult,
) {
    let compacted_messages = compaction
        .summary_messages
        .iter()
        .chain(&compaction.retained_messages)
        .cloned()
        .collect();
    let tools = state.visible_tools();
    if let Some(tokens) = try_provider_input_tokens(
        host.session,
        host.llm,
        snapshot.request_messages(snapshot.messages.clone()),
        &tools,
        "compact_pre_tokens",
    )
    .await
    {
        compaction.pre_tokens = tokens;
    }
    if let Some(tokens) = try_provider_input_tokens(
        host.session,
        host.llm,
        snapshot.request_messages(compacted_messages),
        &tools,
        "compact_post_tokens",
    )
    .await
    {
        compaction.post_tokens = tokens;
    }
}

fn should_attempt_auto_llm_compact(session: &Session) -> bool {
    lock_parking(session.runtime().compact_circuit_breaker()).should_attempt()
}

fn compact_hook_context(
    shared: &SharedTurnContext,
    message_count: usize,
    trigger: CompactTrigger,
) -> CompactHookContext<'_> {
    CompactHookContext {
        session_id: shared.session_id.as_str(),
        working_dir: &shared.working_dir,
        model_id: &shared.model_id,
        trigger,
        message_count,
    }
}

async fn commit_compaction(
    host: &CompactionHost<'_>,
    state: &TurnState,
    snapshot: &ContextSnapshot,
    compaction: &mut CompactResult,
    settings: &astrcode_core::config::ContextSettings,
    turn_id: &TurnId,
    meta: CompactionStageMeta,
) -> CompactionOutcome {
    host.session
        .emit_live(Some(turn_id), LiveEventPayload::CompactionStarted)
        .await;
    let tools = state.visible_tools();
    host.session
        .runtime_services()
        .post_compact_enricher()
        .enrich(
            compaction,
            PostCompactEnrichInput {
                session_id: host.shared.session_id.as_str(),
                source_messages: &snapshot.messages,
                working_dir: &host.shared.working_dir,
                system_prompt: Some(&snapshot.system_prompt),
                tools: &tools,
                settings,
                session_store_dir: host.shared.session_store_dir.clone(),
            },
        )
        .await;

    if meta.trigger == CompactTrigger::AutoThreshold && meta.llm_api_failed {
        lock_parking(host.session.runtime().compact_circuit_breaker()).record_llm_failure();
    }

    let result = persist_compact_result(
        host.session,
        compaction,
        meta.trigger.as_str(),
        snapshot.source_seq,
        meta.strategy,
    )
    .await;
    match result {
        Ok(PersistCompactionOutcome::Committed) => {
            if meta.trigger == CompactTrigger::AutoThreshold && !meta.llm_api_failed {
                lock_parking(host.session.runtime().compact_circuit_breaker())
                    .record_compact_success();
            }
            let hook = compact_hook_context(host.shared, snapshot.messages.len(), meta.trigger);
            if let Err(error) = dispatch_post_compact(host.extension_runner, hook, compaction).await
            {
                tracing::warn!(error = %error, "PostCompact extension dispatch failed");
            }
            host.session
                .emit_live(
                    Some(turn_id),
                    LiveEventPayload::CompactionCompleted {
                        messages_removed: compaction.messages_removed,
                    },
                )
                .await;
            CompactionOutcome::Committed
        },
        Ok(PersistCompactionOutcome::Stale) => {
            host.session
                .emit_live(
                    Some(turn_id),
                    LiveEventPayload::CompactionSkipped {
                        reason: "context changed during compaction".into(),
                    },
                )
                .await;
            CompactionOutcome::Stale
        },
        Err(error) => {
            tracing::warn!(error = %error, "compaction persist failed");
            host.session
                .emit_live(
                    Some(turn_id),
                    LiveEventPayload::CompactionSkipped {
                        reason: error.to_string(),
                    },
                )
                .await;
            CompactionOutcome::NotRun
        },
    }
}
