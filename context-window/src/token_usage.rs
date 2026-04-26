use astrcode_core::{LlmMessage, UserMessageOrigin};
use astrcode_runtime_contract::llm::{LlmUsage, ModelLimits};

const MESSAGE_BASE_TOKENS: usize = 6;
const TOOL_CALL_BASE_TOKENS: usize = 12;
const REQUEST_ESTIMATE_PADDING_NUMERATOR: usize = 4;
const REQUEST_ESTIMATE_PADDING_DENOMINATOR: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptTokenSnapshot {
    pub context_tokens: usize,
    pub budget_tokens: usize,
    pub context_window: usize,
    pub effective_window: usize,
    pub threshold_tokens: usize,
    pub remaining_context_tokens: usize,
    pub reserved_context_size: usize,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TokenUsageTracker {
    anchored_budget_tokens: usize,
}

impl TokenUsageTracker {
    pub fn record_usage(&mut self, usage: Option<LlmUsage>) {
        let Some(usage) = usage else {
            return;
        };
        self.anchored_budget_tokens = self
            .anchored_budget_tokens
            .saturating_add(usage.total_tokens());
    }

    pub fn budget_tokens(&self, estimated_context_tokens: usize) -> usize {
        if self.anchored_budget_tokens > 0 {
            self.anchored_budget_tokens
        } else {
            estimated_context_tokens
        }
    }
}

pub fn build_prompt_snapshot(
    tracker: &TokenUsageTracker,
    messages: &[LlmMessage],
    system_prompt: Option<&str>,
    limits: ModelLimits,
    threshold_percent: u8,
    summary_reserve_tokens: usize,
    reserved_context_size: usize,
) -> PromptTokenSnapshot {
    let context_tokens = estimate_request_tokens(messages, system_prompt);
    let effective_window = effective_context_window(limits, summary_reserve_tokens);
    PromptTokenSnapshot {
        context_tokens,
        budget_tokens: tracker.budget_tokens(context_tokens),
        context_window: limits.context_window,
        effective_window,
        threshold_tokens: compact_threshold_tokens(effective_window, threshold_percent),
        remaining_context_tokens: effective_window.saturating_sub(context_tokens),
        reserved_context_size,
    }
}

pub fn effective_context_window(limits: ModelLimits, summary_reserve_tokens: usize) -> usize {
    limits
        .context_window
        .saturating_sub(summary_reserve_tokens.min(limits.context_window))
}

pub fn compact_threshold_tokens(effective_window: usize, threshold_percent: u8) -> usize {
    effective_window
        .saturating_mul(threshold_percent as usize)
        .saturating_div(100)
}

pub fn should_compact(snapshot: PromptTokenSnapshot) -> bool {
    snapshot.context_tokens >= snapshot.threshold_tokens
        || snapshot.remaining_context_tokens <= snapshot.reserved_context_size
}

pub fn estimate_request_tokens(messages: &[LlmMessage], system_prompt: Option<&str>) -> usize {
    let system_tokens = system_prompt.map_or(0, estimate_text_tokens);
    let raw_total = system_tokens + messages.iter().map(estimate_message_tokens).sum::<usize>();
    raw_total
        .saturating_mul(REQUEST_ESTIMATE_PADDING_NUMERATOR)
        .div_ceil(REQUEST_ESTIMATE_PADDING_DENOMINATOR)
}

pub fn estimate_message_tokens(message: &LlmMessage) -> usize {
    match message {
        LlmMessage::User { content, origin } => {
            MESSAGE_BASE_TOKENS
                + estimate_text_tokens(content)
                + match origin {
                    UserMessageOrigin::User => 0,
                    UserMessageOrigin::QueuedInput => 8,
                    UserMessageOrigin::ContinuationPrompt => 10,
                    UserMessageOrigin::ReactivationPrompt => 8,
                    UserMessageOrigin::RecentUserContextDigest => 8,
                    UserMessageOrigin::RecentUserContext => 8,
                    UserMessageOrigin::CompactSummary => 16,
                }
        },
        LlmMessage::Assistant {
            content,
            tool_calls,
            reasoning,
        } => {
            MESSAGE_BASE_TOKENS
                + estimate_text_tokens(content)
                + reasoning
                    .as_ref()
                    .map_or(0, |reasoning| estimate_text_tokens(&reasoning.content))
                + tool_calls
                    .iter()
                    .map(|call| {
                        TOOL_CALL_BASE_TOKENS
                            + estimate_text_tokens(&call.id)
                            + estimate_text_tokens(&call.name)
                            + estimate_json_tokens(&call.args.to_string())
                    })
                    .sum::<usize>()
        },
        LlmMessage::Tool {
            tool_call_id,
            content,
        } => {
            MESSAGE_BASE_TOKENS + estimate_text_tokens(tool_call_id) + estimate_text_tokens(content)
        },
        LlmMessage::System { content, .. } => MESSAGE_BASE_TOKENS + estimate_text_tokens(content),
    }
}

pub fn estimate_text_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    chars.div_ceil(4).max(1)
}

fn estimate_json_tokens(json: &str) -> usize {
    estimate_text_tokens(json) + 4
}
