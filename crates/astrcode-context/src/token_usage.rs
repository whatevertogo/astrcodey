//! Token 估算与使用量追踪模块。
//!
//! 提供基于文本长度的粗略 token 估算，
//! 并支持锚定到 LLM 提供商返回的实际 token 计数。

use astrcode_core::llm::{LlmContent, LlmMessage, ModelLimits};

const MESSAGE_BASE_TOKENS: usize = 6;
const TOOL_CALL_BASE_TOKENS: usize = 12;
const REQUEST_ESTIMATE_PADDING_NUMERATOR: usize = 4;
const REQUEST_ESTIMATE_PADDING_DENOMINATOR: usize = 3;

/// 一次 provider 请求的 token 快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptTokenSnapshot {
    pub context_tokens: usize,
    pub budget_tokens: usize,
    pub context_window: usize,
    pub effective_window: usize,
    pub threshold_tokens: usize,
    pub remaining_context_tokens: usize,
    pub reserved_context_tokens: usize,
}

/// Token 使用量追踪器。
///
/// 维护已报告的输入/输出 token 计数，
/// 并提供基于字符数的粗略 token 估算能力。
pub struct TokenUsageTracker {
    /// 提供商报告的实际输入 token 数。
    reported_input_tokens: usize,
    /// 提供商报告的实际输出 token 数。
    reported_output_tokens: usize,
    anchored_budget_tokens: usize,
}

impl TokenUsageTracker {
    /// 创建一个新的 token 使用量追踪器，初始计数为零。
    pub fn new() -> Self {
        Self {
            reported_input_tokens: 0,
            reported_output_tokens: 0,
            anchored_budget_tokens: 0,
        }
    }

    /// 基于文本字符数估算 token 数量。
    ///
    /// 使用 4/3 的乘数作为填充系数，即假设平均每 4 个字节约对应 3 个 token。
    /// 这是一个粗略估算，实际 token 数取决于分词器和文本内容。
    pub fn estimate_request_tokens(&self, text: &str) -> usize {
        estimate_text_tokens(text)
    }

    /// 用提供商返回的实际 token 计数更新追踪器。
    ///
    /// # 参数
    /// - `input`：提供商报告的输入 token 数
    /// - `output`：提供商报告的输出 token 数
    pub fn anchor_actuals(&mut self, input: usize, output: usize) {
        self.reported_input_tokens = input;
        self.reported_output_tokens = output;
        self.anchored_budget_tokens = input.saturating_add(output);
    }

    pub fn budget_tokens(&self, estimated_context_tokens: usize) -> usize {
        if self.anchored_budget_tokens > 0 {
            self.anchored_budget_tokens
        } else {
            estimated_context_tokens
        }
    }
}

impl Default for TokenUsageTracker {
    fn default() -> Self {
        Self::new()
    }
}

pub fn build_prompt_snapshot(
    tracker: &TokenUsageTracker,
    messages: &[LlmMessage],
    system_prompt: Option<&str>,
    limits: ModelLimits,
    threshold_percent: u8,
    summary_reserve_tokens: usize,
    reserved_context_tokens: usize,
) -> PromptTokenSnapshot {
    let context_tokens = estimate_request_tokens(messages, system_prompt);
    let effective_window = effective_context_window(&limits, summary_reserve_tokens);
    PromptTokenSnapshot {
        context_tokens,
        budget_tokens: tracker.budget_tokens(context_tokens),
        context_window: limits.max_input_tokens,
        effective_window,
        threshold_tokens: compact_threshold_tokens(effective_window, threshold_percent),
        remaining_context_tokens: effective_window.saturating_sub(context_tokens),
        reserved_context_tokens,
    }
}

pub fn effective_context_window(limits: &ModelLimits, summary_reserve_tokens: usize) -> usize {
    limits
        .max_input_tokens
        .saturating_sub(summary_reserve_tokens.min(limits.max_input_tokens))
}

pub fn compact_threshold_tokens(effective_window: usize, threshold_percent: u8) -> usize {
    effective_window
        .saturating_mul(threshold_percent as usize)
        .saturating_div(100)
}

pub fn should_compact(snapshot: PromptTokenSnapshot) -> bool {
    snapshot.context_tokens >= snapshot.threshold_tokens
        || snapshot.remaining_context_tokens <= snapshot.reserved_context_tokens
}

pub fn estimate_request_tokens(messages: &[LlmMessage], system_prompt: Option<&str>) -> usize {
    let system_tokens = system_prompt.map_or(0, estimate_text_tokens);
    let raw_total = system_tokens + messages.iter().map(estimate_message_tokens).sum::<usize>();
    raw_total
        .saturating_mul(REQUEST_ESTIMATE_PADDING_NUMERATOR)
        .div_ceil(REQUEST_ESTIMATE_PADDING_DENOMINATOR)
}

pub fn estimate_message_tokens(message: &LlmMessage) -> usize {
    MESSAGE_BASE_TOKENS
        + message
            .content
            .iter()
            .map(estimate_content_tokens)
            .sum::<usize>()
}

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
    fn snapshot_reserves_summary_space_before_threshold() {
        let messages = vec![LlmMessage::user("x".repeat(2000))];
        let tracker = TokenUsageTracker::default();
        let snapshot = build_prompt_snapshot(
            &tracker,
            &messages,
            Some("system"),
            ModelLimits {
                max_input_tokens: 1000,
                max_output_tokens: 100,
            },
            80,
            200,
            64,
        );

        assert_eq!(snapshot.effective_window, 800);
        assert_eq!(snapshot.threshold_tokens, 640);
        assert!(should_compact(snapshot));
    }
}
