use astrcode_core::llm::{LlmMessage, ModelLimits};

use crate::{
    compaction::{CompactResult, CompactSkipReason, compact_messages_with_render_options},
    settings::ContextWindowSettings,
    token_usage::{build_prompt_snapshot, should_compact},
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
}

/// 已准备好的 provider 消息。
///
/// system prompt 不在这里返回；server 可以继续用自己的 system-message 前缀，
/// 这里负责返回 compact 后的可见消息窗口。
#[derive(Debug, Clone)]
pub struct PreparedContext {
    pub messages: Vec<LlmMessage>,
    pub compaction: Option<CompactResult>,
}

/// LLM 上下文组装门面。
///
/// 它负责 token gate 与 compact pipeline；不持有 provider/model limits，
/// 避免模型切换后沿用旧窗口。
pub struct LlmContextAssembler {
    settings: ContextWindowSettings,
}

impl LlmContextAssembler {
    /// 创建上下文组装器；settings 是稳定策略，模型窗口由每次 request 输入提供。
    pub fn new(settings: ContextWindowSettings) -> Self {
        Self { settings }
    }

    /// 准备 provider 可见消息；达到阈值时使用 deterministic compact fallback。
    ///
    /// 这个入口不需要 LLM/provider，适合测试或没有 provider-backed compact 的路径。
    pub fn prepare_messages(&self, input: ContextPrepareInput<'_>) -> PreparedContext {
        let mut messages = input.messages;
        let snapshot = self.snapshot(&messages, input.system_prompt, input.model_limits);
        let compaction = if self.settings.auto_compact_enabled && should_compact(snapshot) {
            match compact_messages_with_render_options(
                &messages,
                input.system_prompt,
                &Default::default(),
            ) {
                Ok(compaction) => {
                    let prepared = prepared_context_from_compaction(compaction);
                    messages = prepared.messages;
                    prepared.compaction
                },
                Err(CompactSkipReason::Empty | CompactSkipReason::NothingToCompact) => None,
            }
        } else {
            None
        };

        PreparedContext {
            messages,
            compaction,
        }
    }

    pub fn auto_compact_enabled(&self) -> bool {
        self.settings.auto_compact_enabled
    }

    pub fn settings(&self) -> &ContextWindowSettings {
        &self.settings
    }

    pub fn prompt_snapshot(
        &self,
        input: &ContextPrepareInput<'_>,
    ) -> crate::token_usage::PromptTokenSnapshot {
        self.snapshot(
            &input.messages,
            input.system_prompt,
            input.model_limits.clone(),
        )
    }

    fn snapshot(
        &self,
        messages: &[LlmMessage],
        system_prompt: Option<&str>,
        model_limits: ModelLimits,
    ) -> crate::token_usage::PromptTokenSnapshot {
        build_prompt_snapshot(
            messages,
            system_prompt,
            model_limits,
            self.settings.compact_threshold_percent,
        )
    }
}

fn prepared_context_from_compaction(compaction: CompactResult) -> PreparedContext {
    let messages = [
        compaction.context_messages.clone(),
        compaction.retained_messages.clone(),
    ]
    .concat();
    PreparedContext {
        messages,
        compaction: Some(compaction),
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::llm::LlmRole;

    use super::*;

    #[test]
    fn prepare_messages_uses_current_model_limits_each_call() {
        let assembler = LlmContextAssembler::new(ContextWindowSettings::default());
        let messages = vec![
            LlmMessage::user("old user ".repeat(400)),
            LlmMessage::assistant("old answer ".repeat(400)),
            LlmMessage::user("current"),
        ];

        let large_window = assembler.prepare_messages(ContextPrepareInput {
            messages: messages.clone(),
            system_prompt: None,
            model_limits: ModelLimits {
                max_input_tokens: 200_000,
                max_output_tokens: 1024,
            },
        });
        let small_window = assembler.prepare_messages(ContextPrepareInput {
            messages,
            system_prompt: None,
            model_limits: ModelLimits {
                max_input_tokens: 100,
                max_output_tokens: 1024,
            },
        });

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
}
