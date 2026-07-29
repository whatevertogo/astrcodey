//! Session compaction：共享持久化、manual 入口与 turn 内 auto/reactive 编排。

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use astrcode_context::{
    CompactError, CompactResult, CompactSummaryRenderOptions, ContextPrepareInput, ContextSnapshot,
    PostCompactEnrichInput,
    compaction::{compact_messages_deterministic, compact_messages_with_fallback},
};
use astrcode_core::{
    compaction::{CompactStrategy, CompactTrigger},
    config::ModelSelection,
    event::LiveEventPayload,
    llm::{self, LlmMessage, LlmProvider},
    tool::ToolDefinition,
    types::TurnId,
};
use astrcode_extension_sdk::{
    extension::{
        CompactContext, CompactEvent, CompactResult as TypedCompactResult, ExtensionError,
    },
    runtime_ports::TurnHooks,
};
use astrcode_storage::CompactSnapshotInput;

use crate::{
    deferred_tools::append_deferred_tools_reminder,
    projection_context::context_snapshot,
    session::{Session, SessionError},
    turn_context::{SharedTurnContext, TurnError},
    turn_publish::TurnEvents,
    turn_stages::TurnState,
};

// ── Turn compaction ──

pub(crate) struct CompactionPlan {
    trigger: CompactTrigger,
    strategy: CompactStrategy,
    use_llm_for_compact: bool,
    keep_recent_turns: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CompactionOutcome {
    Skipped,
    Committed,
}

pub(crate) struct PreparedProviderHistory {
    pub snapshot: ContextSnapshot,
    pub messages: Vec<LlmMessage>,
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
        trigger: CompactTrigger::AutoThreshold,
        strategy: CompactStrategy::Auto,
        use_llm_for_compact: should_attempt_auto_llm_compact(host.session),
        keep_recent_turns: None,
    })
}

pub(crate) async fn prepare_provider_history(
    host: &CompactionHost<'_>,
    state: &TurnState,
    initial_snapshot: ContextSnapshot,
    turn_id: &TurnId,
    plan: Option<CompactionPlan>,
    publisher: &TurnEvents,
) -> Result<PreparedProviderHistory, TurnError> {
    let context_assembler = host.session.runtime_services().context_assembler_arc();
    let snapshot = match plan {
        Some(plan) => {
            let (snapshot, _) = run_compaction(
                host,
                state,
                initial_snapshot.messages.len(),
                turn_id,
                plan,
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

async fn run_compaction(
    host: &CompactionHost<'_>,
    state: &TurnState,
    probe_message_count: usize,
    turn_id: &TurnId,
    plan: CompactionPlan,
    publisher: &TurnEvents,
) -> Result<(ContextSnapshot, CompactionOutcome), TurnError> {
    let custom_instructions = collect_compact_instructions(
        host.extension_runner,
        compact_hook_context(host.shared, probe_message_count, plan.trigger),
    )
    .await;

    // PreCompact may append durable facts. Compact must start from the projection after the
    // hook, not from the probe snapshot used only to decide whether compaction is needed.
    publisher.reload_model_cache().await?;
    let source_snapshot = context_snapshot(&publisher.snapshot_model().await?);
    let custom_instructions = match custom_instructions {
        Ok(instructions) => instructions,
        Err(error) => {
            tracing::warn!(error = %error, "PreCompact extension dispatch failed");
            return Ok((source_snapshot, CompactionOutcome::Skipped));
        },
    };

    let context_assembler = host.session.runtime_services().context_assembler_arc();
    let keep_recent_turns = plan
        .keep_recent_turns
        .or_else(|| context_assembler.settings().compact_keep_recent_turns);
    let render_options = CompactSummaryRenderOptions {
        custom_instructions: custom_instructions.clone(),
        ..Default::default()
    };
    let execution = if plan.use_llm_for_compact {
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
    let Ok(execution) = execution else {
        return Ok((source_snapshot, CompactionOutcome::Skipped));
    };

    let mut compaction = execution.result;
    update_compaction_token_counts(host, state, &source_snapshot, &mut compaction).await;
    let outcome = commit_compaction(
        host,
        state,
        &source_snapshot,
        &mut compaction,
        context_assembler.settings(),
        turn_id,
        CompactionStageMeta {
            trigger: plan.trigger,
            strategy: plan.strategy,
            llm_api_failed: execution.llm_api_failed,
        },
    )
    .await;
    publisher.reload_model_cache().await?;
    Ok((
        context_snapshot(&publisher.snapshot_model().await?),
        outcome,
    ))
}

pub(crate) async fn run_reactive_compaction(
    host: &CompactionHost<'_>,
    state: &TurnState,
    turn_id: &TurnId,
    publisher: &TurnEvents,
) -> Result<bool, TurnError> {
    publisher.reload_model_cache().await?;
    let snapshot = context_snapshot(&publisher.snapshot_model().await?);
    let (_, outcome) = run_compaction(
        host,
        state,
        snapshot.messages.len(),
        turn_id,
        CompactionPlan {
            trigger: CompactTrigger::ReactivePromptTooLong,
            strategy: CompactStrategy::ReactivePromptTooLong,
            use_llm_for_compact: true,
            keep_recent_turns: None,
        },
        publisher,
    )
    .await?;
    Ok(outcome != CompactionOutcome::Skipped)
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
    session
        .runtime()
        .compact_circuit_breaker()
        .lock()
        .should_attempt()
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
        host.session
            .runtime()
            .compact_circuit_breaker()
            .lock()
            .record_llm_failure();
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
        Ok(()) => {
            if meta.trigger == CompactTrigger::AutoThreshold && !meta.llm_api_failed {
                host.session
                    .runtime()
                    .compact_circuit_breaker()
                    .lock()
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
            CompactionOutcome::Skipped
        },
    }
}

// ── Shared hook and persistence ──

#[derive(Clone, Copy)]
struct CompactHookContext<'a> {
    session_id: &'a str,
    working_dir: &'a str,
    model_id: &'a str,
    trigger: CompactTrigger,
    message_count: usize,
}

impl CompactHookContext<'_> {
    fn build_compact_context(&self, compaction: Option<&CompactResult>) -> CompactContext {
        CompactContext {
            session_id: self.session_id.to_string(),
            working_dir: self.working_dir.to_string(),
            model: ModelSelection::simple(self.model_id),
            trigger: self.trigger,
            message_count: self.message_count,
            pre_tokens: compaction.map(|compaction| compaction.pre_tokens),
            post_tokens: compaction.map(|compaction| compaction.post_tokens),
            summary: compaction.map(|compaction| compaction.summary.clone()),
        }
    }
}

async fn collect_compact_instructions(
    extension_runner: &dyn TurnHooks,
    input: CompactHookContext<'_>,
) -> Result<Vec<String>, ExtensionError> {
    let result = extension_runner
        .emit_compact(CompactEvent::PreCompact, input.build_compact_context(None))
        .await?;
    match result {
        TypedCompactResult::Contributions(contributions) => Ok(contributions
            .instructions
            .into_iter()
            .map(|instruction| instruction.trim().to_string())
            .filter(|instruction| !instruction.is_empty())
            .collect()),
        TypedCompactResult::Block { reason } => Err(ExtensionError::Blocked { reason }),
        TypedCompactResult::Allow => Ok(Vec::new()),
    }
}

async fn dispatch_post_compact(
    extension_runner: &dyn TurnHooks,
    input: CompactHookContext<'_>,
    compaction: &CompactResult,
) -> Result<(), ExtensionError> {
    extension_runner
        .emit_compact(
            CompactEvent::PostCompact,
            input.build_compact_context(Some(compaction)),
        )
        .await?;
    Ok(())
}

async fn request_compact_summary(
    llm: Arc<dyn LlmProvider>,
    messages: Vec<LlmMessage>,
) -> Result<String, CompactError> {
    let rx = llm
        .generate(messages, vec![])
        .await
        .map_err(CompactError::Llm)?;
    llm::collect_stream_text(rx)
        .await
        .map_err(CompactError::Llm)
}

/// 重写 compact 输入快照对应的 transcript 前缀；projection 会保留之后到达的 tail。
async fn persist_compact_result(
    session: &Session,
    compaction: &CompactResult,
    trigger_name: &str,
    source_seq: u64,
    strategy: CompactStrategy,
) -> Result<(), SessionError> {
    session
        .rewrite_transcript_for_compaction(
            trigger_name.to_owned(),
            compaction.clone(),
            source_seq,
            strategy,
        )
        .await?;
    Ok(())
}

// ── Idle compaction ──

/// 空闲态 compact 结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdleCompactionOutcome {
    Compacted { messages_removed: usize },
    Skipped { message: String },
}

#[derive(Debug, thiserror::Error)]
pub enum IdleCompactionError {
    #[error("{0}")]
    Session(#[from] SessionError),
    #[error("{0}")]
    Extension(#[from] ExtensionError),
}

/// 在无 active turn 时压缩会话历史并持久化。
pub async fn compact_idle_session(
    session: &Session,
    keep_recent_turns: Option<usize>,
) -> Result<IdleCompactionOutcome, IdleCompactionError> {
    let runtime_services = session.runtime_services();
    let extension_runner = runtime_services.turn_hooks_arc();
    let context_assembler = runtime_services.context_assembler_arc();

    let state = session.read_model().await?;
    let pre_hook = CompactHookContext {
        session_id: session.id.as_str(),
        working_dir: &state.identity.working_dir,
        model_id: &state.identity.model_id,
        trigger: CompactTrigger::ManualCommand,
        message_count: state.transcript.messages.len(),
    };
    let custom_instructions =
        collect_compact_instructions(extension_runner.as_ref(), pre_hook).await?;

    let state = session.read_model().await?;
    let snapshot = context_snapshot(&state);
    let llm = runtime_services.llm();
    let post_hook = CompactHookContext {
        session_id: session.id.as_str(),
        working_dir: &state.identity.working_dir,
        model_id: &state.identity.model_id,
        trigger: CompactTrigger::ManualCommand,
        message_count: snapshot.messages.len(),
    };
    let tool_registry = session
        .tool_registry_snapshot(&state.identity.working_dir)
        .await?;
    let tools = tool_registry.list_definitions();
    let transcript_path = session
        .write_compact_snapshot(CompactSnapshotInput {
            trigger: CompactTrigger::ManualCommand.as_str().into(),
            model_id: state.identity.model_id.clone(),
            working_dir: state.identity.working_dir.clone(),
            system_prompt: Some(snapshot.system_prompt.clone()),
            provider_messages: snapshot.messages.clone(),
        })
        .await?;
    let render_options = CompactSummaryRenderOptions {
        transcript_path,
        custom_instructions: custom_instructions.clone(),
    };
    let compact_execution = compact_messages_with_fallback(
        &snapshot.messages,
        Some(&snapshot.system_prompt),
        context_assembler.settings(),
        &custom_instructions,
        &render_options,
        keep_recent_turns,
        |messages| request_compact_summary(Arc::clone(&llm), messages),
    )
    .await;

    let mut compaction = match compact_execution {
        Err(_) => {
            return Ok(IdleCompactionOutcome::Skipped {
                message: "Nothing to compact".into(),
            });
        },
        Ok(compaction) => compaction.result,
    };

    let session_store_dir = session.session_store_dir().await;
    session
        .runtime_services()
        .post_compact_enricher()
        .enrich(
            &mut compaction,
            PostCompactEnrichInput {
                session_id: session.id.as_str(),
                source_messages: &snapshot.messages,
                working_dir: &state.identity.working_dir,
                system_prompt: Some(&snapshot.system_prompt),
                tools: &tools,
                settings: context_assembler.settings(),
                session_store_dir,
            },
        )
        .await;

    persist_compact_result(
        session,
        &compaction,
        CompactTrigger::ManualCommand.as_str(),
        snapshot.source_seq,
        CompactStrategy::Manual { keep_recent_turns },
    )
    .await?;

    if let Err(error) =
        dispatch_post_compact(extension_runner.as_ref(), post_hook, &compaction).await
    {
        tracing::warn!(error = %error, "PostCompact extension dispatch failed");
    }
    Ok(IdleCompactionOutcome::Compacted {
        messages_removed: compaction.messages_removed,
    })
}

// ── Circuit breaker ──

#[derive(Debug, Clone)]
enum CircuitState {
    Closed,
    Open { until: Instant },
    HalfOpen,
}

#[derive(Debug, Clone)]
pub(crate) struct CompactCircuitBreaker {
    state: CircuitState,
    consecutive_llm_failures: u32,
    threshold: u32,
    cooldown: Duration,
    half_open_attempt_in_flight: bool,
}

impl CompactCircuitBreaker {
    pub(crate) fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            state: CircuitState::Closed,
            consecutive_llm_failures: 0,
            threshold: threshold.max(1),
            cooldown,
            half_open_attempt_in_flight: false,
        }
    }

    pub(crate) fn reconfigure(&mut self, threshold: u32, cooldown: Duration) {
        self.threshold = threshold.max(1);
        self.cooldown = cooldown;
    }

    pub(crate) fn should_attempt(&mut self) -> bool {
        match &self.state {
            CircuitState::Closed => true,
            CircuitState::Open { until } => {
                if Instant::now() < *until {
                    return false;
                }
                self.state = CircuitState::HalfOpen;
                self.half_open_attempt_in_flight = false;
                self.allow_half_open_attempt()
            },
            CircuitState::HalfOpen => self.allow_half_open_attempt(),
        }
    }

    pub(crate) fn record_llm_failure(&mut self) {
        self.consecutive_llm_failures = self.consecutive_llm_failures.saturating_add(1);
        if matches!(self.state, CircuitState::HalfOpen)
            || self.consecutive_llm_failures >= self.threshold
        {
            self.start_cooldown();
        }
    }

    pub(crate) fn record_compact_success(&mut self) {
        self.consecutive_llm_failures = 0;
        self.start_cooldown();
    }

    fn allow_half_open_attempt(&mut self) -> bool {
        if self.half_open_attempt_in_flight {
            return false;
        }
        self.half_open_attempt_in_flight = true;
        true
    }

    fn start_cooldown(&mut self) {
        self.state = CircuitState::Open {
            until: Instant::now() + self.cooldown,
        };
        self.half_open_attempt_in_flight = false;
    }
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use super::CompactCircuitBreaker;

    #[test]
    fn circuit_breaker_enforces_failure_threshold_cooldown_and_half_open_probe() {
        let mut breaker = CompactCircuitBreaker::new(2, Duration::from_millis(5));

        assert!(breaker.should_attempt());
        breaker.record_llm_failure();
        assert!(breaker.should_attempt());
        breaker.record_llm_failure();
        assert!(!breaker.should_attempt());

        thread::sleep(Duration::from_millis(10));
        assert!(breaker.should_attempt());
        assert!(!breaker.should_attempt());

        breaker.record_compact_success();
        assert!(!breaker.should_attempt());
        thread::sleep(Duration::from_millis(10));
        assert!(breaker.should_attempt());
        assert!(!breaker.should_attempt());
    }
}
