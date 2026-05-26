//! Token 估算模块。
//!
//! 提供基于文本长度的粗略 token 估算，并据此判断上下文是否达到压缩阈值。

use astrcode_core::llm::{LlmContent, LlmMessage, ModelLimits};

const MESSAGE_BASE_TOKENS: usize = 6;
const TOOL_CALL_BASE_TOKENS: usize = 12;
const REQUEST_ESTIMATE_PADDING_NUMERATOR: usize = 4;
const REQUEST_ESTIMATE_PADDING_DENOMINATOR: usize = 3;

/// 一次 provider 请求的 token 快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptTokenSnapshot {
    /// 当前请求的估算输入 token。
    pub context_tokens: usize,
    /// 根据当前模型窗口和 compact 阈值计算出的触发线。
    pub threshold_tokens: usize,
    /// 当前模型的输入窗口上限。
    pub max_input_tokens: usize,
    /// 预留输出 token。
    pub reserved_output_tokens: usize,
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
    let context_tokens = estimate_request_tokens(messages, system_prompt);
    PromptTokenSnapshot {
        context_tokens,
        threshold_tokens: compact_threshold_tokens(limits.max_input_tokens, threshold_percent),
        max_input_tokens: limits.max_input_tokens,
        reserved_output_tokens: limits.max_output_tokens,
    }
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
/// effective_budget = min(threshold, max_input - reserved_output)
/// trigger_if: current_tokens + growth >= effective_budget
pub fn should_compact_predictive(
    snapshot: PromptTokenSnapshot,
    growth_estimate: usize,
    model_limits: ModelLimits,
) -> bool {
    let hard_budget = model_limits
        .max_input_tokens
        .saturating_sub(model_limits.max_output_tokens);
    let effective_budget = snapshot.threshold_tokens.min(hard_budget);
    snapshot.context_tokens.saturating_add(growth_estimate) >= effective_budget
}

/// 估算完整 provider request 的输入 token。
///
/// 这是轻量启发式估算，不追求 tokenizer 级精确；额外 padding 用来降低
/// 低估上下文长度导致 prompt-too-long 的概率。
pub fn estimate_request_tokens(messages: &[LlmMessage], system_prompt: Option<&str>) -> usize {
    let system_tokens = system_prompt.map_or(0, estimate_text_tokens);
    let raw_total = system_tokens + messages.iter().map(estimate_message_tokens).sum::<usize>();
    raw_total
        .saturating_mul(REQUEST_ESTIMATE_PADDING_NUMERATOR)
        .div_ceil(REQUEST_ESTIMATE_PADDING_DENOMINATOR)
}

/// 估算单条 LLM message 的 token。
pub fn estimate_message_tokens(message: &LlmMessage) -> usize {
    MESSAGE_BASE_TOKENS
        + message
            .content
            .iter()
            .map(estimate_content_tokens)
            .sum::<usize>()
}

/// 粗略按 4 chars ~= 1 token 估算文本 token。
pub fn estimate_text_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4).max(1)
}

/// 按同一套粗略 token 估算裁剪文本，并追加调用方指定的截断标记。
pub fn truncate_text_to_tokens(content: &str, max_tokens: usize, marker: &str) -> String {
    if estimate_text_tokens(content) <= max_tokens {
        return content.to_string();
    }
    let max_chars = max_tokens.saturating_mul(4);
    let content_budget = max_chars.saturating_sub(marker.chars().count());
    let mut truncated = content.chars().take(content_budget).collect::<String>();
    truncated.push_str(marker);
    truncated
}

fn estimate_content_tokens(content: &LlmContent) -> usize {
    match content {
        LlmContent::Text { text } => estimate_text_tokens(text),
        LlmContent::Image { base64, .. } => estimate_text_tokens(base64),
        LlmContent::ToolCall {
            call_id,
            name,
            arguments,
        } => {
            TOOL_CALL_BASE_TOKENS
                + estimate_text_tokens(call_id)
                + estimate_text_tokens(name)
                + estimate_text_tokens(&arguments.to_string())
        },
        LlmContent::ToolResult {
            tool_call_id,
            content,
            ..
        } => estimate_text_tokens(tool_call_id) + estimate_text_tokens(content),
    }
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
            reserved_output_tokens: 1024,
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
            reserved_output_tokens: 2_000,
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
}
