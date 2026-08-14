//! Manual、auto 与 reactive compact 共用的同步状态机。

use std::sync::Arc;

use astrcode_context::{
    CompactError, CompactResult, CompactSummaryRenderOptions, ContextSnapshot,
    PostCompactEnrichInput,
    compaction::{
        LlmCompactAttempt, compact_messages_deterministic, compact_messages_with_fallback,
    },
};
use astrcode_core::{
    compaction::{CompactStrategy, CompactTrigger},
    event::LiveEventPayload,
    llm::{self, LlmMessage, LlmProvider, LlmRequest},
    tool::ToolDefinition,
};
use astrcode_extension_sdk::{
    extension::{
        CompactEvent, CompactPayload, CompactResult as TypedCompactResult, ExtensionError,
        RuntimeCompactContext, RuntimeHookCallContext,
    },
    runtime_ports::TurnHooks,
};
use astrcode_storage::CompactSnapshotInput;

use super::persistence::persist_compaction;
use crate::{SessionError, projection_context::context_snapshot, session::Session};

pub(crate) struct CompactionPipeline<'a> {
    pub session: &'a Session,
    pub llm: Arc<dyn LlmProvider>,
    pub extension_runner: &'a dyn TurnHooks,
    pub hook_call: RuntimeHookCallContext,
    pub pre_hook_message_count: usize,
    pub tools: &'a [ToolDefinition],
    pub strategy: CompactStrategy,
    pub use_llm: bool,
}

#[derive(Debug)]
pub(crate) enum CompactionPipelineOutcome {
    Compacted {
        messages_removed: usize,
        llm_attempt: LlmCompactAttempt,
    },
    Skipped {
        message: String,
    },
    Failed {
        error: SessionError,
        llm_attempt: LlmCompactAttempt,
        source_snapshot: Option<ContextSnapshot>,
    },
}

impl CompactionPipelineOutcome {
    pub(crate) const fn llm_attempt(&self) -> LlmCompactAttempt {
        match self {
            Self::Compacted { llm_attempt, .. } | Self::Failed { llm_attempt, .. } => *llm_attempt,
            Self::Skipped { .. } => LlmCompactAttempt::NotAttempted,
        }
    }
}

impl CompactionPipeline<'_> {
    /// 运行一次完整 compact，并恰好发出一个 started 与一个 terminal live event。
    pub(crate) async fn run(self) -> CompactionPipelineOutcome {
        let turn_id = self
            .hook_call
            .turn_id()
            .map(astrcode_core::types::TurnId::from);
        self.session
            .emit_live(turn_id.as_ref(), LiveEventPayload::CompactionStarted);

        let outcome = self.run_inner().await;
        let terminal = match &outcome {
            CompactionPipelineOutcome::Compacted {
                messages_removed, ..
            } => LiveEventPayload::CompactionCompleted {
                messages_removed: *messages_removed,
            },
            CompactionPipelineOutcome::Skipped { message, .. } => {
                LiveEventPayload::CompactionSkipped {
                    reason: message.clone(),
                }
            },
            CompactionPipelineOutcome::Failed { error, .. } => LiveEventPayload::CompactionFailed {
                reason: error.to_string(),
            },
        };
        self.session.emit_live(turn_id.as_ref(), terminal);
        outcome
    }

    async fn run_inner(&self) -> CompactionPipelineOutcome {
        let trigger = self.strategy.trigger();
        let custom_instructions = match collect_compact_instructions(
            self.extension_runner,
            CompactHookContext {
                call: self.hook_call.clone(),
                trigger,
                message_count: self.pre_hook_message_count,
            },
        )
        .await
        {
            Ok(PreCompactOutcome::Ready(instructions)) => instructions,
            Ok(PreCompactOutcome::Blocked(reason)) => {
                return CompactionPipelineOutcome::Skipped { message: reason };
            },
            Err(error) => {
                return CompactionPipelineOutcome::Failed {
                    error: error.into(),
                    llm_attempt: LlmCompactAttempt::NotAttempted,
                    source_snapshot: None,
                };
            },
        };

        // PreCompact 可追加 durable facts；provider context 必须在 hook 返回后重新冻结。
        let source_model = match self.session.read_model().await {
            Ok(model) => model,
            Err(error) => {
                return CompactionPipelineOutcome::Failed {
                    error,
                    llm_attempt: LlmCompactAttempt::NotAttempted,
                    source_snapshot: None,
                };
            },
        };
        let source_snapshot = context_snapshot(&source_model);

        let transcript_path = if matches!(self.strategy, CompactStrategy::Manual { .. }) {
            match self
                .session
                .write_compact_snapshot(CompactSnapshotInput {
                    trigger: trigger.as_str().into(),
                    model_id: source_model.identity.model_id.clone(),
                    working_dir: source_model.identity.working_dir.clone(),
                    system_prompt: Some(source_snapshot.system_prompt.clone()),
                    provider_messages: source_snapshot.messages.clone(),
                })
                .await
            {
                Ok(path) => path,
                Err(error) => {
                    return CompactionPipelineOutcome::Failed {
                        error,
                        llm_attempt: LlmCompactAttempt::NotAttempted,
                        source_snapshot: Some(source_snapshot),
                    };
                },
            }
        } else {
            None
        };

        let context_assembler = self.session.runtime_services().context_assembler_arc();
        let keep_recent_turns = self
            .strategy
            .keep_recent_turns()
            .or(context_assembler.settings().compact_keep_recent_turns);
        let render_options = CompactSummaryRenderOptions {
            transcript_path,
            custom_instructions: custom_instructions.clone(),
        };
        let execution = if self.use_llm {
            let max_output_tokens = context_assembler.settings().compact_max_output_tokens;
            compact_messages_with_fallback(
                &source_snapshot.messages,
                Some(&source_snapshot.system_prompt),
                context_assembler.settings(),
                &custom_instructions,
                &render_options,
                keep_recent_turns,
                |messages| {
                    request_compact_summary(Arc::clone(&self.llm), messages, max_output_tokens)
                },
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
        let execution = match execution {
            Ok(execution) => execution,
            Err(reason) => {
                return CompactionPipelineOutcome::Skipped {
                    message: format!("compact skipped: {reason:?}"),
                };
            },
        };

        let mut compaction = execution.result;
        update_compaction_token_counts(
            self.session,
            &self.llm,
            self.tools,
            &source_snapshot,
            &mut compaction,
        )
        .await;
        self.session
            .runtime_services()
            .post_compact_enricher()
            .enrich(
                &mut compaction,
                PostCompactEnrichInput {
                    session_id: self.session.id().as_str(),
                    source_messages: &source_snapshot.messages,
                    working_dir: &source_model.identity.working_dir,
                    system_prompt: Some(&source_snapshot.system_prompt),
                    tools: self.tools,
                    settings: context_assembler.settings(),
                    session_store_dir: self
                        .hook_call
                        .session_store_dir()
                        .map(std::path::Path::to_path_buf),
                },
            )
            .await;

        if let Err(error) =
            persist_compaction(self.session, &compaction, &source_snapshot, self.strategy).await
        {
            tracing::warn!(error = %error, "compaction persist failed");
            return CompactionPipelineOutcome::Failed {
                error,
                llm_attempt: execution.llm_attempt,
                source_snapshot: Some(source_snapshot),
            };
        }

        let post_hook = CompactHookContext {
            call: self.hook_call.clone(),
            trigger,
            message_count: source_snapshot.messages.len(),
        };
        if let Err(error) =
            dispatch_post_compact(self.extension_runner, post_hook, &compaction).await
        {
            tracing::warn!(error = %error, "PostCompact extension dispatch failed");
        }

        CompactionPipelineOutcome::Compacted {
            messages_removed: compaction.messages_removed,
            llm_attempt: execution.llm_attempt,
        }
    }
}

pub(crate) async fn try_provider_input_tokens(
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

async fn update_compaction_token_counts(
    session: &Session,
    llm: &Arc<dyn LlmProvider>,
    tools: &[ToolDefinition],
    snapshot: &ContextSnapshot,
    compaction: &mut CompactResult,
) {
    let compacted_messages = compaction
        .summary_messages
        .iter()
        .chain(&compaction.retained_messages)
        .cloned()
        .collect();
    if let Some(tokens) = try_provider_input_tokens(
        session,
        llm,
        snapshot.request_messages(snapshot.messages.clone()),
        tools,
        "compact_pre_tokens",
    )
    .await
    {
        compaction.pre_tokens = tokens;
    }
    if let Some(tokens) = try_provider_input_tokens(
        session,
        llm,
        snapshot.request_messages(compacted_messages),
        tools,
        "compact_post_tokens",
    )
    .await
    {
        compaction.post_tokens = tokens;
    }
}

struct CompactHookContext {
    call: RuntimeHookCallContext,
    trigger: CompactTrigger,
    message_count: usize,
}

impl CompactHookContext {
    fn build_context(&self, compaction: Option<&CompactResult>) -> RuntimeCompactContext {
        RuntimeCompactContext::new(
            self.call.clone(),
            CompactPayload::new(
                self.trigger,
                self.message_count,
                compaction.map(|compaction| compaction.pre_tokens),
                compaction.map(|compaction| compaction.post_tokens),
                compaction.map(|compaction| compaction.summary.clone()),
            ),
        )
    }
}

enum PreCompactOutcome {
    Ready(Vec<String>),
    Blocked(String),
}

async fn collect_compact_instructions(
    extension_runner: &dyn TurnHooks,
    input: CompactHookContext,
) -> Result<PreCompactOutcome, ExtensionError> {
    let result = extension_runner
        .emit_compact(CompactEvent::PreCompact, input.build_context(None))
        .await?;
    Ok(match result {
        TypedCompactResult::Contributions(contributions) => PreCompactOutcome::Ready(
            contributions
                .instructions
                .into_iter()
                .map(|instruction| instruction.trim().to_string())
                .filter(|instruction| !instruction.is_empty())
                .collect(),
        ),
        TypedCompactResult::Block { reason } => PreCompactOutcome::Blocked(reason),
        TypedCompactResult::Allow => PreCompactOutcome::Ready(Vec::new()),
    })
}

async fn dispatch_post_compact(
    extension_runner: &dyn TurnHooks,
    input: CompactHookContext,
    compaction: &CompactResult,
) -> Result<(), ExtensionError> {
    extension_runner
        .emit_compact(
            CompactEvent::PostCompact,
            input.build_context(Some(compaction)),
        )
        .await?;
    Ok(())
}

async fn request_compact_summary(
    llm: Arc<dyn LlmProvider>,
    messages: Vec<LlmMessage>,
    max_output_tokens: usize,
) -> Result<String, CompactError> {
    let rx = llm
        .generate_request(
            LlmRequest::new(messages, vec![]).with_max_output_tokens(max_output_tokens),
        )
        .await
        .map_err(CompactError::Llm)?;
    llm::collect_stream_text(rx)
        .await
        .map_err(CompactError::Llm)
}

#[cfg(test)]
mod tests;
