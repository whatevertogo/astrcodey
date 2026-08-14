//! Turn 内 auto 规划、reactive 补救与 compact 后 context 刷新。

use std::sync::Arc;

use astrcode_context::{ContextAssembler, ContextPrepareInput, ContextSnapshot};
use astrcode_core::{
    compaction::CompactStrategy,
    llm::{LlmMessage, LlmProvider},
    tool::ToolDefinition,
};
use astrcode_extension_sdk::{
    extension::internal::RuntimeHookCallContext, runtime_ports::TurnHooks,
};

use super::{
    circuit_breaker::{CompactAttemptPermit, CompactCircuitBreaker},
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
    pub context_assembler: &'a Arc<dyn ContextAssembler>,
    pub hook_call: RuntimeHookCallContext,
    pub extension_runner: &'a dyn TurnHooks,
    pub breaker: &'a parking_lot::Mutex<CompactCircuitBreaker>,
}

pub(crate) async fn plan_auto_compaction(
    host: &CompactionHost<'_>,
    snapshot: &ContextSnapshot,
    tools: &[ToolDefinition],
) -> Option<CompactionPlan> {
    let provider_input_tokens = try_provider_input_tokens(
        host.session,
        host.llm,
        snapshot.request_messages(snapshot.messages.clone()),
        tools,
        "compact_gate",
    )
    .await;
    let threshold_met = host
        .context_assembler
        .should_auto_compact(&ContextPrepareInput {
            messages: snapshot.messages.clone(),
            system_prompt: Some(&snapshot.system_prompt),
            model_limits: host.llm.model_limits(),
            provider_input_tokens,
        });
    if !host.context_assembler.auto_compact_enabled() || !threshold_met {
        return None;
    }

    Some(CompactionPlan {
        strategy: CompactStrategy::Auto,
        use_llm: true,
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
    let messages = host
        .context_assembler
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
    let breaker_attempt = if matches!(strategy, CompactStrategy::Auto) && use_llm {
        CompactAttemptPermit::acquire(host.breaker)
    } else {
        None
    };
    let use_llm = if matches!(strategy, CompactStrategy::Auto) {
        use_llm && breaker_attempt.is_some()
    } else {
        use_llm
    };
    let outcome = CompactionPipeline {
        session: host.session,
        llm: Arc::clone(host.llm),
        context_assembler: Arc::clone(host.context_assembler),
        extension_runner: host.extension_runner,
        hook_call: host.hook_call.clone(),
        pre_hook_message_count: probe_message_count,
        tools,
        strategy,
        use_llm,
    }
    .run()
    .await;

    if let Some(attempt) = breaker_attempt {
        attempt.finish(outcome.llm_attempt());
    }
    let outcome = match outcome {
        CompactionPipelineOutcome::Failed { error, .. }
            if error.uncertain_through_seq().is_some() =>
        {
            return Err(error.into());
        },
        outcome => outcome,
    };
    let fallback_snapshot = match &outcome {
        CompactionPipelineOutcome::Failed {
            source_snapshot: Some(snapshot),
            ..
        } => Some(snapshot.as_ref().clone()),
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
