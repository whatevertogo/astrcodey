use std::future::Future;

use astrcode_core::llm::{LlmMessage, ModelLimits};

use crate::{
    ContextSettings,
    compaction::{
        CompactError, CompactExecution, CompactResult, CompactSkipReason,
        CompactSummaryRenderOptions, compact_messages_deterministic,
        compact_messages_with_fallback,
    },
    token_budget::{
        build_prompt_snapshot, estimate_turn_growth, should_compact, should_compact_predictive,
    },
};

/// 一次 provider request 的上下文准备输入。
///
/// `model_limits` 必须由调用方在每次请求前传入当前模型的限制，
/// 这样切换模型后 compact 阈值会立即跟随新窗口大小。
#[derive(Debug, Clone)]
pub struct ContextPrepareInput<'a> {
    /// 不包含 system prompt 的可见对话消息。
    pub messages: Vec<LlmMessage>,
    /// 已组装好的 system prompt；这里只参与 token 估算和 compact request。
    pub system_prompt: Option<&'a str>,
    /// 当前 provider/model 的上下文限制。
    pub model_limits: ModelLimits,
    /// 插件提供的 compact 指令，追加到 compact summary 中。
    pub custom_instructions: Vec<String>,
}

/// 已准备好的 provider 消息。
///
/// system prompt 不在这里返回；server 可以继续用自己的 system-message 前缀，
/// 这里负责返回 compact 后的可见消息窗口。
#[derive(Debug, Clone)]
pub struct PreparedContext {
    pub messages: Vec<LlmMessage>,
    pub compaction: Option<PreparedCompaction>,
}

#[derive(Debug, Clone)]
pub struct PreparedCompaction {
    pub result: CompactResult,
    pub llm_api_failed: bool,
}

/// 是否执行 compact，以及 compact 时是否调用 LLM。
#[derive(Debug, Clone, Copy)]
pub struct CompactMessagesOptions {
    pub run: bool,
    pub use_llm: bool,
    pub keep_recent_turns: Option<usize>,
}

/// compact 执行结果（不含持久化）。
#[derive(Debug, Clone)]
pub enum CompactIfNeededOutcome {
    /// 未触发 compact（阈值未到且非 force）。
    NotRun { messages: Vec<LlmMessage> },
    /// 触发但无安全前缀可压（Empty / NothingToCompact）。
    Skipped { messages: Vec<LlmMessage> },
    /// 已生成摘要与新的可见窗口。
    Applied {
        messages: Vec<LlmMessage>,
        compaction: PreparedCompaction,
    },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PrepareMessagesOptions {
    /// 根据阈值或 force 执行 compact（与是否调用 LLM 无关）。
    pub run_compact: bool,
    /// compact 时是否调用 LLM；为 false 时仅用确定性模板。
    pub use_llm_for_compact: bool,
    pub force_compact: bool,
    pub keep_recent_turns: Option<usize>,
}

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
    ) -> crate::token_budget::PromptTokenSnapshot {
        build_prompt_snapshot(
            messages,
            system_prompt,
            model_limits,
            self.settings.compact_threshold_percent,
        )
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
