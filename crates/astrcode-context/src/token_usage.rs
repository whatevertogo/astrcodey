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
        };
        assert!(!should_compact(below_threshold));

        let at_threshold = PromptTokenSnapshot {
            context_tokens: 16_700,
            ..below_threshold
        };
        assert!(should_compact(at_threshold));
    }
}
