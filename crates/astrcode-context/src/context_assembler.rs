use std::future::Future;

use astrcode_core::{
    context::{
        CompactError, CompactIfNeededOutcome, CompactMessagesOptions, CompactRequestFn,
        CompactSummaryRenderOptions, ContextAssembler, ContextPrepareInput, PrepareMessagesOptions,
        PreparedCompaction, PreparedContext,
    },
    llm::{LlmMessage, ModelLimits},
};

use crate::{
    ContextSettings,
    compaction::{
        CompactExecution, CompactSkipReason, compact_messages_deterministic,
        compact_messages_with_fallback,
    },
    token_budget::{
        build_prompt_snapshot, estimate_turn_growth, should_compact, should_compact_predictive,
    },
};

/// LLM 上下文组装门面。
///
/// 它负责 token gate 与 compact pipeline；不持有 provider/model limits，
/// 避免模型切换后沿用旧窗口。
pub struct LlmContextAssembler {
    settings: ContextSettings,
}

impl LlmContextAssembler {
    /// 创建上下文组装器；settings 是稳定策略，模型窗口由每次 request 输入提供。
    pub fn new(settings: ContextSettings) -> Self {
        Self { settings }
    }

    /// 在需要时执行 compact，返回新的可见消息窗口。
    pub async fn compact_if_needed<F, Fut>(
        &self,
        messages: Vec<LlmMessage>,
        system_prompt: Option<&str>,
        custom_instructions: &[String],
        render_options: CompactSummaryRenderOptions,
        options: CompactMessagesOptions,
        request_text: F,
    ) -> CompactIfNeededOutcome
    where
        F: FnMut(Vec<LlmMessage>) -> Fut,
        Fut: Future<Output = Result<String, CompactError>>,
    {
        if !options.run {
            return CompactIfNeededOutcome::NotRun { messages };
        }

        let keep_recent_turns = options
            .keep_recent_turns
            .or(self.settings.compact_keep_recent_turns);

        let execution = if options.use_llm {
            compact_messages_with_fallback(
                &messages,
                system_prompt,
                &self.settings,
                custom_instructions,
                &render_options,
                keep_recent_turns,
                request_text,
            )
            .await
        } else {
            compact_messages_deterministic(
                &messages,
                system_prompt,
                &render_options,
                keep_recent_turns,
            )
        };

        match execution {
            Ok(compaction) => {
                let PreparedCompaction {
                    result,
                    llm_api_failed,
                } = prepared_compaction_from_execution(compaction);
                let messages = [
                    result.context_messages.clone(),
                    result.retained_messages.clone(),
                ]
                .concat();
                CompactIfNeededOutcome::Applied {
                    messages,
                    compaction: PreparedCompaction {
                        result,
                        llm_api_failed,
                    },
                }
            },
            Err(CompactSkipReason::Empty | CompactSkipReason::NothingToCompact) => {
                CompactIfNeededOutcome::Skipped { messages }
            },
        }
    }

    /// 准备 provider 可见消息；达到阈值时先尝试 LLM compact，失败降级到 deterministic。
    pub async fn prepare_messages_with_llm<F, Fut>(
        &self,
        input: ContextPrepareInput<'_>,
        options: PrepareMessagesOptions,
        request_text: F,
    ) -> PreparedContext
    where
        F: FnMut(Vec<LlmMessage>) -> Fut,
        Fut: Future<Output = Result<String, CompactError>>,
    {
        let snapshot = self.snapshot(
            &input.messages,
            input.system_prompt,
            input.model_limits.clone(),
            input.provider_input_tokens,
        );
        let threshold_met = should_compact(snapshot)
            || (self.settings.predictive_compact_enabled
                && should_compact_predictive(
                    snapshot,
                    estimate_turn_growth(
                        &input.messages,
                        self.settings.predictive_compact_baseline_growth_tokens,
                    ),
                    input.model_limits.clone(),
                ));
        let render_options = CompactSummaryRenderOptions {
            custom_instructions: input.custom_instructions.clone(),
            ..Default::default()
        };
        let compact_options = CompactMessagesOptions {
            run: options.force_compact || (options.run_compact && threshold_met),
            use_llm: options.use_llm_for_compact,
            keep_recent_turns: options.keep_recent_turns,
        };

        match self
            .compact_if_needed(
                input.messages,
                input.system_prompt,
                &input.custom_instructions,
                render_options,
                compact_options,
                request_text,
            )
            .await
        {
            CompactIfNeededOutcome::NotRun { messages }
            | CompactIfNeededOutcome::Skipped { messages } => PreparedContext {
                messages,
                compaction: None,
            },
            CompactIfNeededOutcome::Applied {
                messages,
                compaction,
            } => PreparedContext {
                messages,
                compaction: Some(compaction),
            },
        }
    }

    pub fn auto_compact_enabled(&self) -> bool {
        self.settings.auto_compact_enabled
    }

    pub fn settings(&self) -> &ContextSettings {
        &self.settings
    }

    pub fn prompt_snapshot(
        &self,
        input: &ContextPrepareInput<'_>,
    ) -> crate::token_budget::PromptTokenSnapshot {
        self.snapshot(
            &input.messages,
            input.system_prompt,
            input.model_limits.clone(),
            input.provider_input_tokens,
        )
    }

    pub fn should_auto_compact(&self, input: &ContextPrepareInput<'_>) -> bool {
        let snapshot = self.prompt_snapshot(input);
        should_compact(snapshot)
            || (self.settings.predictive_compact_enabled
                && should_compact_predictive(
                    snapshot,
                    estimate_turn_growth(
                        &input.messages,
                        self.settings.predictive_compact_baseline_growth_tokens,
                    ),
                    input.model_limits.clone(),
                ))
    }

    fn snapshot(
        &self,
        messages: &[LlmMessage],
        system_prompt: Option<&str>,
        model_limits: ModelLimits,
        provider_input_tokens: Option<usize>,
    ) -> crate::token_budget::PromptTokenSnapshot {
        let mut snapshot = build_prompt_snapshot(
            messages,
            system_prompt,
            model_limits,
            self.settings.compact_threshold_percent,
        );
        if let Some(context_tokens) = provider_input_tokens {
            snapshot.context_tokens = context_tokens;
        }
        snapshot
    }
}

#[async_trait::async_trait]
impl ContextAssembler for LlmContextAssembler {
    fn settings(&self) -> &ContextSettings {
        self.settings()
    }

    fn should_auto_compact(&self, input: &ContextPrepareInput<'_>) -> bool {
        self.should_auto_compact(input)
    }

    async fn compact_if_needed(
        &self,
        messages: Vec<LlmMessage>,
        system_prompt: Option<&str>,
        custom_instructions: &[String],
        render_options: CompactSummaryRenderOptions,
        options: CompactMessagesOptions,
        request_text: CompactRequestFn,
    ) -> CompactIfNeededOutcome {
        LlmContextAssembler::compact_if_needed(
            self,
            messages,
            system_prompt,
            custom_instructions,
            render_options,
            options,
            request_text,
        )
        .await
    }
}

fn prepared_compaction_from_execution(compaction: CompactExecution) -> PreparedCompaction {
    PreparedCompaction {
        result: compaction.result,
        llm_api_failed: compaction.llm_api_failed,
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::llm::LlmRole;

    use super::*;

    #[tokio::test]
    async fn prepare_messages_with_llm_uses_current_model_limits_each_call() {
        let assembler = LlmContextAssembler::new(ContextSettings::default());
        let messages = vec![
            LlmMessage::user("old user ".repeat(400)),
            LlmMessage::assistant("old answer ".repeat(400)),
            LlmMessage::user("current"),
        ];

        let large_window = assembler
            .prepare_messages_with_llm(
                ContextPrepareInput {
                    messages: messages.clone(),
                    system_prompt: None,
                    model_limits: ModelLimits {
                        max_input_tokens: 200_000,
                        max_output_tokens: 1024,
                    },
                    provider_input_tokens: None,
                    custom_instructions: Vec::new(),
                },
                PrepareMessagesOptions {
                    run_compact: true,
                    use_llm_for_compact: true,
                    force_compact: false,
                    keep_recent_turns: None,
                },
                |_msgs| async {
                    Err(CompactError::Llm(astrcode_core::llm::LlmError::Transport(
                        "test".into(),
                    )))
                },
            )
            .await;
        let small_window = assembler
            .prepare_messages_with_llm(
                ContextPrepareInput {
                    messages,
                    system_prompt: None,
                    model_limits: ModelLimits {
                        max_input_tokens: 100,
                        max_output_tokens: 1024,
                    },
                    provider_input_tokens: None,
                    custom_instructions: Vec::new(),
                },
                PrepareMessagesOptions {
                    run_compact: true,
                    use_llm_for_compact: true,
                    force_compact: false,
                    keep_recent_turns: None,
                },
                |_msgs| async {
                    Err(CompactError::Llm(astrcode_core::llm::LlmError::Transport(
                        "test".into(),
                    )))
                },
            )
            .await;

        assert!(large_window.compaction.is_none());
        assert!(small_window.compaction.is_some());
        assert!(small_window.messages.first().is_some_and(|message| {
            message.role == LlmRole::User
                && message
                    .content
                    .iter()
                    .any(|content| matches!(content, astrcode_core::llm::LlmContent::Text { text } if text.contains("<compact_summary>")))
        }));
    }

    #[test]
    fn prompt_snapshot_uses_provider_input_tokens_when_available() {
        let assembler = LlmContextAssembler::new(ContextSettings::default());
        let snapshot = assembler.prompt_snapshot(&ContextPrepareInput {
            messages: vec![LlmMessage::user("short")],
            system_prompt: None,
            model_limits: ModelLimits {
                max_input_tokens: 10_000,
                max_output_tokens: 1024,
            },
            provider_input_tokens: Some(4_200),
            custom_instructions: Vec::new(),
        });

        assert_eq!(snapshot.context_tokens, 4_200);
    }

    #[tokio::test]
    async fn compact_if_needed_runs_deterministic_when_llm_disabled() {
        let assembler = LlmContextAssembler::new(ContextSettings::default());
        let messages = vec![
            LlmMessage::user("old user ".repeat(400)),
            LlmMessage::assistant("old answer ".repeat(400)),
            LlmMessage::user("current"),
        ];

        let outcome = assembler
            .compact_if_needed(
                messages,
                None,
                &[],
                CompactSummaryRenderOptions::default(),
                CompactMessagesOptions {
                    run: true,
                    use_llm: false,
                    keep_recent_turns: None,
                },
                |_msgs| async { panic!("deterministic compact must not call LLM request_fn") },
            )
            .await;

        assert!(matches!(outcome, CompactIfNeededOutcome::Applied { .. }));
    }
}
