use astrcode_core::llm::{LlmMessage, ModelLimits, provider_visible_messages};

use crate::{
    ContextSettings,
    token_budget::{
        PromptTokenSnapshot, build_prompt_snapshot, estimate_turn_growth, should_compact,
        should_compact_predictive,
    },
};

/// 一次 provider request 的上下文准备输入。
///
/// `model_limits` 由调用方按请求传入，避免切换模型后沿用旧窗口。
#[derive(Debug, Clone)]
pub struct ContextPrepareInput<'a> {
    /// 不包含 system prompt 的可见对话消息。
    pub messages: Vec<LlmMessage>,
    /// 已组装好的 system prompt，仅参与 token 估算。
    pub system_prompt: Option<&'a str>,
    pub model_limits: ModelLimits,
    /// provider 返回的 input token 统计；缺失时回退本地估算。
    pub provider_input_tokens: Option<usize>,
}

/// 经过 provider 协议归一化的可见消息及其 token 快照。
#[derive(Debug, Clone)]
pub struct PreparedContext {
    pub messages: Vec<LlmMessage>,
    pub token_snapshot: PromptTokenSnapshot,
}

/// provider-ready 上下文组装边界。
///
/// Compact 是独立的 transcript 重写事务，不属于普通 request prepare。
pub trait ContextAssembler: Send + Sync {
    fn settings(&self) -> &ContextSettings;

    fn prepare_messages(&self, input: ContextPrepareInput<'_>) -> PreparedContext;

    fn should_auto_compact(&self, input: &ContextPrepareInput<'_>) -> bool;

    fn auto_compact_enabled(&self) -> bool {
        self.settings().auto_compact_enabled
    }
}

pub struct LlmContextAssembler {
    settings: ContextSettings,
}

impl LlmContextAssembler {
    pub fn new(settings: ContextSettings) -> Self {
        Self { settings }
    }

    fn snapshot(
        &self,
        messages: &[LlmMessage],
        system_prompt: Option<&str>,
        model_limits: ModelLimits,
        provider_input_tokens: Option<usize>,
    ) -> PromptTokenSnapshot {
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

impl ContextAssembler for LlmContextAssembler {
    fn settings(&self) -> &ContextSettings {
        &self.settings
    }

    fn prepare_messages(&self, input: ContextPrepareInput<'_>) -> PreparedContext {
        let messages = provider_visible_messages(input.messages);
        let token_snapshot = self.snapshot(
            &messages,
            input.system_prompt,
            input.model_limits,
            input.provider_input_tokens,
        );
        PreparedContext {
            messages,
            token_snapshot,
        }
    }

    fn should_auto_compact(&self, input: &ContextPrepareInput<'_>) -> bool {
        let snapshot = self.snapshot(
            &input.messages,
            input.system_prompt,
            input.model_limits.clone(),
            input.provider_input_tokens,
        );
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
}

#[cfg(test)]
mod tests {
    use astrcode_core::llm::{LlmRole, ModelLimits};

    use super::*;

    #[test]
    fn prepares_provider_messages_and_uses_current_token_source() {
        let assembler = LlmContextAssembler::new(ContextSettings::default());
        let input = ContextPrepareInput {
            messages: vec![
                LlmMessage::system("stale"),
                LlmMessage::user("hello"),
                LlmMessage::assistant("world"),
            ],
            system_prompt: Some("current system"),
            model_limits: ModelLimits {
                max_input_tokens: 10_000,
                max_output_tokens: 1_024,
            },
            provider_input_tokens: Some(4_200),
        };

        let prepared = assembler.prepare_messages(input);

        assert_eq!(prepared.token_snapshot.context_tokens, 4_200);
        assert!(
            prepared
                .messages
                .iter()
                .any(|message| message.role == LlmRole::User)
        );
    }

    #[test]
    fn auto_compact_uses_limits_from_each_request() {
        let assembler = LlmContextAssembler::new(ContextSettings::default());
        let messages = vec![
            LlmMessage::user("old user ".repeat(400)),
            LlmMessage::assistant("old answer ".repeat(400)),
            LlmMessage::user("current"),
        ];
        let input = |max_input_tokens| ContextPrepareInput {
            messages: messages.clone(),
            system_prompt: None,
            model_limits: ModelLimits {
                max_input_tokens,
                max_output_tokens: 1_024,
            },
            provider_input_tokens: None,
        };

        assert!(!assembler.should_auto_compact(&input(200_000)));
        assert!(assembler.should_auto_compact(&input(100)));
    }
}
