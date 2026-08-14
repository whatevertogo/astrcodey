//! Turn 内 auto 规划、reactive 补救与 compact 后 context 刷新。

use std::sync::Arc;

use astrcode_context::{ContextPrepareInput, ContextSnapshot};
use astrcode_core::{
    compaction::CompactStrategy,
    llm::{LlmMessage, LlmProvider},
    tool::ToolDefinition,
};
use astrcode_extension_sdk::{extension::RuntimeHookCallContext, runtime_ports::TurnHooks};

use super::{
    circuit_breaker::CompactCircuitBreaker,
    pipeline::{CompactionPipeline, CompactionPipelineOutcome, try_provider_input_tokens},
};
use crate::{
    deferred_tools::append_deferred_tools_reminder, projection_context::context_snapshot,
    session::Session, turn_context::TurnError, turn_publish::TurnEvents, turn_stages::TurnState,
};

pub(crate) struct CompactionPlan {
    strategy: CompactStrategy,
    use_llm: bool,
}

#[derive(PartialEq, Eq)]
enum CompactionOutcome {
    Committed,
    NotCommitted { reason: String },
}

pub(crate) struct PreparedProviderHistory {
    pub snapshot: ContextSnapshot,
    pub messages: Vec<LlmMessage>,
}

pub(crate) struct CompactionHost<'a> {
    pub session: &'a Session,
    pub llm: &'a Arc<dyn LlmProvider>,
    pub hook_call: RuntimeHookCallContext,
    pub extension_runner: &'a dyn TurnHooks,
    pub breaker: &'a parking_lot::Mutex<CompactCircuitBreaker>,
}

pub(crate) async fn plan_auto_compaction(
    host: &CompactionHost<'_>,
    snapshot: &ContextSnapshot,
    tools: &[ToolDefinition],
) -> Option<CompactionPlan> {
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
    if !context_assembler.auto_compact_enabled() || !threshold_met {
        return None;
    }

    Some(CompactionPlan {
        strategy: CompactStrategy::Auto,
        use_llm: host.breaker.lock().should_attempt(),
    })
}

pub(crate) async fn prepare_provider_history(
    host: &CompactionHost<'_>,
    state: &TurnState,
    initial_snapshot: ContextSnapshot,
    plan: Option<CompactionPlan>,
    tools: &[ToolDefinition],
    publisher: &TurnEvents,
) -> Result<PreparedProviderHistory, TurnError> {
    let context_assembler = host.session.runtime_services().context_assembler_arc();
    let snapshot = match plan {
        Some(plan) => {
            let (snapshot, _) = run_compaction(
                host,
                initial_snapshot.messages.len(),
                plan,
                tools,
                publisher,
            )
            .await?;
            snapshot
        },
        None => initial_snapshot,
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

    Ok(PreparedProviderHistory { snapshot, messages })
}

pub(crate) async fn run_reactive_compaction(
    host: &CompactionHost<'_>,
    state: &TurnState,
    publisher: &TurnEvents,
) -> Result<bool, TurnError> {
    let model = publisher.snapshot_model().await?;
    let snapshot = context_snapshot(&model);
    let tools = state.visible_tools();
    let (_, outcome) = run_compaction(
        host,
        snapshot.messages.len(),
        CompactionPlan {
            strategy: CompactStrategy::ReactivePromptTooLong,
            use_llm: true,
        },
        &tools,
        publisher,
    )
    .await?;
    if let CompactionOutcome::NotCommitted { reason } = &outcome {
        tracing::warn!(%reason, "reactive compaction did not commit; prompt remains too long");
    }
    Ok(outcome == CompactionOutcome::Committed)
}

async fn run_compaction(
    host: &CompactionHost<'_>,
    probe_message_count: usize,
    plan: CompactionPlan,
    tools: &[ToolDefinition],
    publisher: &TurnEvents,
) -> Result<(ContextSnapshot, CompactionOutcome), TurnError> {
    let CompactionPlan { strategy, use_llm } = plan;
    let records_breaker_attempt = matches!(strategy, CompactStrategy::Auto) && use_llm;
    let outcome = CompactionPipeline {
        session: host.session,
        llm: Arc::clone(host.llm),
        extension_runner: host.extension_runner,
        hook_call: host.hook_call.clone(),
        pre_hook_message_count: probe_message_count,
        tools,
        strategy,
        use_llm,
    }
    .run()
    .await;

    if records_breaker_attempt {
        host.breaker.lock().finish_attempt(outcome.llm_attempt());
    }
    // append 已更新进程内 projection 但 fsync 失败时，本 turn 仍必须沿用冻结的原上下文。
    let fallback_snapshot = match &outcome {
        CompactionPipelineOutcome::Failed {
            source_snapshot: Some(snapshot),
            ..
        } => Some(snapshot.clone()),
        _ => None,
    };
    let result = match outcome {
        CompactionPipelineOutcome::Compacted { .. } => CompactionOutcome::Committed,
        CompactionPipelineOutcome::Skipped { message, .. } => {
            CompactionOutcome::NotCommitted { reason: message }
        },
        CompactionPipelineOutcome::Failed { error, .. } => CompactionOutcome::NotCommitted {
            reason: error.to_string(),
        },
    };
    let snapshot = match fallback_snapshot {
        Some(snapshot) => snapshot,
        None => {
            let model = publisher.snapshot_model().await?;
            context_snapshot(&model)
        },
    };
    Ok((snapshot, result))
}
