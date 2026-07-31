//! Token 估算模块。
//!
//! 提供基于文本长度的粗略 token 估算，并据此判断上下文是否达到压缩阈值。

pub use astrcode_core::llm::token_estimate::{
    estimate_message_tokens, estimate_request_tokens, estimate_text_tokens,
};
use astrcode_core::llm::{
    LlmMessage, ModelLimits,
    token_estimate::{estimate_char_budget, estimate_provider_message_tokens},
};

use crate::prompt_engine::system_messages_from_prompt;

/// 一次 provider 请求的 token 快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptTokenSnapshot {
    /// 当前请求的估算输入 token。
    pub context_tokens: usize,
    /// 根据当前模型窗口和 compact 阈值计算出的触发线。
    pub threshold_tokens: usize,
    /// 当前模型的输入窗口上限。
    pub max_input_tokens: usize,
    /// 模型配置允许的最大输出 token。
    pub max_output_tokens: usize,
}

/// 构建 compact gate 使用的 token 快照。
///
/// `limits` 必须来自当前请求使用的模型，不能由 context manager 缓存。
pub fn build_prompt_snapshot(
    messages: &[LlmMessage],
    system_prompt: Option<&str>,
    limits: ModelLimits,
    threshold_percent: f32,
) -> PromptTokenSnapshot {
    let context_tokens = estimate_request_tokens_with_prompt(messages, system_prompt);
    PromptTokenSnapshot {
        context_tokens,
        threshold_tokens: compact_threshold_tokens(limits.max_input_tokens, threshold_percent),
        max_input_tokens: limits.max_input_tokens,
        max_output_tokens: limits.max_output_tokens,
    }
}

pub(crate) fn estimate_request_tokens_with_prompt(
    messages: &[LlmMessage],
    system_prompt: Option<&str>,
) -> usize {
    let system_messages = system_prompt
        .map(system_messages_from_prompt)
        .unwrap_or_default();
    estimate_provider_message_tokens(system_messages.iter().chain(messages))
}

/// 根据模型输入窗口和百分比阈值计算 compact 触发 token。
pub fn compact_threshold_tokens(effective_window: usize, threshold_percent: f32) -> usize {
    let threshold_percent = if threshold_percent.is_finite() {
        threshold_percent.clamp(0.0, 100.0)
    } else {
        100.0
    };
    ((effective_window as f64) * f64::from(threshold_percent) / 100.0).floor() as usize
}

/// 判断当前请求是否已经达到 compact 阈值。
pub fn should_compact(snapshot: PromptTokenSnapshot) -> bool {
    snapshot.context_tokens >= snapshot.threshold_tokens
}

const MAX_REQUEST_SAFETY_TOKENS: usize = 4096;
const REQUEST_SAFETY_WINDOW_DIVISOR: usize = 16;
const MIN_REQUEST_OUTPUT_TOKENS: usize = 16;

/// 根据最终 input 动态限制单次请求的最大输出。
///
/// 小窗口测试模型按窗口比例缩放安全余量；生产模型最多保留 4096 tokens。返回
/// `None` 表示连最小输出空间都无法提供，调用方应在发送网络请求前 compact。
pub fn request_max_output_tokens(
    snapshot: PromptTokenSnapshot,
    provider_minimum_output_tokens: usize,
) -> Option<usize> {
    let safety_tokens =
        MAX_REQUEST_SAFETY_TOKENS.min(snapshot.max_input_tokens / REQUEST_SAFETY_WINDOW_DIVISOR);
    let available_output_tokens = snapshot
        .max_input_tokens
        .saturating_sub(snapshot.context_tokens)
        .saturating_sub(safety_tokens);
    let max_output_tokens = snapshot.max_output_tokens.min(available_output_tokens);
    let minimum = snapshot
        .max_output_tokens
        .min(MIN_REQUEST_OUTPUT_TOKENS.max(provider_minimum_output_tokens))
        .max(1);
    (max_output_tokens >= minimum).then_some(max_output_tokens)
}

/// 估算下一轮 token 增长（EMA + 最近一轮取最大值，下限为 baseline）。
pub fn estimate_turn_growth(messages: &[LlmMessage], baseline: usize) -> usize {
    let turns = turn_token_totals(messages);
    if turns.is_empty() {
        return baseline;
    }

    let latest = turns[turns.len() - 1];
    let mut ema = turns[0] as f64;
    for tokens in turns.iter().skip(1) {
        ema = ema * 0.6 + *tokens as f64 * 0.4;
    }
    let ema = ema.round() as usize;

    baseline.max(latest.max(ema))
}

/// 预测性判断：
/// effective_budget = min(threshold, max_input - safety_margin)
/// trigger_if: current_tokens + growth >= effective_budget
pub fn should_compact_predictive(
    snapshot: PromptTokenSnapshot,
    growth_estimate: usize,
    model_limits: ModelLimits,
) -> bool {
    let safety_tokens = MAX_REQUEST_SAFETY_TOKENS
        .min(model_limits.max_input_tokens / REQUEST_SAFETY_WINDOW_DIVISOR);
    let hard_budget = model_limits.max_input_tokens.saturating_sub(safety_tokens);
    let effective_budget = snapshot.threshold_tokens.min(hard_budget);
    snapshot.context_tokens.saturating_add(growth_estimate) >= effective_budget
}

/// 按同一套粗略 token 估算裁剪文本，并追加调用方指定的截断标记。
pub fn truncate_text_to_tokens(content: &str, max_tokens: usize, marker: &str) -> String {
    if estimate_text_tokens(content) <= max_tokens {
        return content.to_string();
    }
    let max_chars = estimate_char_budget(max_tokens);
    let content_budget = max_chars.saturating_sub(marker.chars().count());
    let mut truncated = content.chars().take(content_budget).collect::<String>();
    truncated.push_str(marker);
    truncated
}

/// 按字符数裁剪文本，并追加调用方指定的截断标记。
pub fn truncate_chars(content: &str, max_chars: usize, marker: &str) -> String {
    if content.chars().count() <= max_chars {
        return content.to_string();
    }
    let mut truncated = content.chars().take(max_chars).collect::<String>();
    truncated.push_str(marker);
    truncated
}

fn turn_token_totals(messages: &[LlmMessage]) -> Vec<usize> {
    let mut turns = Vec::new();
    let mut current = 0usize;

    for message in messages {
        if message.role == astrcode_core::llm::LlmRole::User && current > 0 {
            turns.push(current);
            current = 0;
        }
        current = current.saturating_add(estimate_message_tokens(message));
    }

    if current > 0 {
        turns.push(current);
    }

    turns
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_compact_uses_fractional_threshold() {
        let threshold_tokens = compact_threshold_tokens(20_000, 83.5);
        assert_eq!(threshold_tokens, 16_700);

        let below_threshold = PromptTokenSnapshot {
            context_tokens: 16_699,
            threshold_tokens,
            max_input_tokens: 20_000,
            max_output_tokens: 1024,
        };
        assert!(!should_compact(below_threshold));

        let at_threshold = PromptTokenSnapshot {
            context_tokens: 16_700,
            ..below_threshold
        };
        assert!(should_compact(at_threshold));
    }

    #[test]
    fn predictive_compact_uses_latest_or_baseline_growth() {
        let messages = vec![
            LlmMessage::user("short"),
            LlmMessage::assistant("brief"),
            LlmMessage::user("x".repeat(2000)),
            LlmMessage::assistant("y".repeat(2000)),
        ];
        let growth = estimate_turn_growth(&messages, 200);
        assert!(growth >= 200);

        let snapshot = PromptTokenSnapshot {
            context_tokens: 15_000,
            threshold_tokens: 16_000,
            max_input_tokens: 20_000,
            max_output_tokens: 2_000,
        };
        assert!(should_compact_predictive(
            snapshot,
            growth,
            ModelLimits {
                max_input_tokens: 20_000,
                max_output_tokens: 2_000,
            }
        ));
    }

    #[test]
    fn request_output_budget_clamps_large_model_cap_and_rejects_exhausted_context() {
        let snapshot = PromptTokenSnapshot {
            context_tokens: 656_158,
            threshold_tokens: 835_000,
            max_input_tokens: 1_000_000,
            max_output_tokens: 393_216,
        };
        assert_eq!(request_max_output_tokens(snapshot, 1), Some(339_746));
        assert_eq!(
            request_max_output_tokens(
                PromptTokenSnapshot {
                    context_tokens: 999_990,
                    ..snapshot
                },
                1
            ),
            None
        );
        assert_eq!(request_max_output_tokens(snapshot, 340_000), None);
    }
}
