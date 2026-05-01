//! LLM 驱动的上下文压缩模块。
//!
//! 当上下文窗口接近容量上限时，通过 LLM 对历史对话进行摘要压缩，
//! 保留关键信息的同时释放 token 空间。

use astrcode_core::llm::{LlmContent, LlmMessage, LlmRole};

use crate::{
    settings::ContextWindowSettings,
    token_usage::{estimate_request_tokens, estimate_text_tokens},
};

const COMPACT_SUMMARY_MARKER: &str = "<compact_summary>";
const COMPACT_SUMMARY_END: &str = "</compact_summary>";

/// 压缩配置参数。
///
/// 控制压缩行为的关键阈值和保留策略。
pub struct CompactConfig {
    /// 压缩时保留的最近对话轮数。
    pub keep_recent_turns: u8,
    /// 压缩时保留的最近用户消息条数。
    pub keep_recent_user_messages: u8,
    /// 触发自动压缩的上下文占用百分比阈值（0–100）。
    pub threshold_percent: u8,
    /// 压缩失败时的最大重试次数。
    pub max_retry_attempts: u8,
    /// LLM 压缩输出的最大 token 数。
    pub max_output_tokens: usize,
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            keep_recent_turns: 5,
            keep_recent_user_messages: 3,
            threshold_percent: 90,
            max_retry_attempts: 3,
            max_output_tokens: 200000,
        }
    }
}

/// 压缩操作的结果。
///
/// 记录压缩前后的 token 数量以及 LLM 生成的摘要文本。
#[derive(Debug, Clone)]
pub struct CompactResult {
    /// 压缩前的 token 数量。
    pub pre_tokens: usize,
    /// 压缩后的 token 数量。
    pub post_tokens: usize,
    /// LLM 生成的对话摘要。
    pub summary: String,
    /// 压缩掉的可见消息数量。
    pub messages_removed: usize,
    /// 供 provider 使用的合成上下文消息。
    pub context_messages: Vec<LlmMessage>,
    /// 保留的可见消息尾部。
    pub retained_messages: Vec<LlmMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactSkipReason {
    Empty,
    NothingToCompact,
}

pub fn compact_messages(
    messages: &[LlmMessage],
    system_prompt: Option<&str>,
    settings: &ContextWindowSettings,
) -> Result<CompactResult, CompactSkipReason> {
    if messages.is_empty() {
        return Err(CompactSkipReason::Empty);
    }
    let keep_start = split_for_compaction(messages, settings.compact_keep_recent_turns as usize)
        .ok_or(CompactSkipReason::NothingToCompact)?;
    if keep_start == 0 {
        return Err(CompactSkipReason::NothingToCompact);
    }

    let prefix = &messages[..keep_start];
    let retained_messages = messages[keep_start..].to_vec();
    let pre_tokens = estimate_request_tokens(messages, system_prompt);
    let summary = summarize_prefix(prefix);
    let context_messages = vec![LlmMessage::user(format_compact_summary(&summary))];
    let post_tokens = estimate_request_tokens(
        &[context_messages.clone(), retained_messages.clone()].concat(),
        system_prompt,
    );

    Ok(CompactResult {
        pre_tokens,
        post_tokens,
        summary,
        messages_removed: keep_start,
        context_messages,
        retained_messages,
    })
}

pub fn format_compact_summary(summary: &str) -> String {
    format!(
        "{COMPACT_SUMMARY_MARKER}\n{}\n{COMPACT_SUMMARY_END}",
        summary.trim()
    )
}

pub fn is_compact_summary_message(message: &LlmMessage) -> bool {
    message.role == LlmRole::User
        && message.content.iter().any(|content| {
            matches!(
                content,
                LlmContent::Text { text }
                    if text.trim_start().starts_with(COMPACT_SUMMARY_MARKER)
            )
        })
}

pub fn is_synthetic_context_message(message: &LlmMessage) -> bool {
    if is_compact_summary_message(message) {
        return true;
    }
    message.role == LlmRole::User && message.content.iter().any(|content| {
        matches!(
            content,
            LlmContent::Text { text }
                if text.starts_with("Recovered file context after compaction.")
                    || text.starts_with("Recovered file context after compaction is unavailable.")
        )
    })
}

pub fn is_prompt_too_long_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    let positive = [
        "prompt too long",
        "context length",
        "maximum context",
        "too many tokens",
        "input is too long",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    let negative = ["rate limit", "quota", "throttle", "timeout"]
        .iter()
        .any(|needle| lower.contains(needle));
    positive && !negative
}

fn split_for_compaction(messages: &[LlmMessage], keep_recent_turns: usize) -> Option<usize> {
    let user_indices = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| (message.role == LlmRole::User).then_some(index))
        .filter(|index| !is_synthetic_context_message(&messages[*index]))
        .collect::<Vec<_>>();
    if user_indices.len() <= keep_recent_turns.max(1) {
        return fallback_keep_start(messages);
    }
    let keep_turns = keep_recent_turns.max(1);
    user_indices
        .get(user_indices.len().saturating_sub(keep_turns))
        .copied()
        .filter(|index| *index > 0)
}

fn fallback_keep_start(messages: &[LlmMessage]) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| {
            (index > 0
                && message.role == LlmRole::Assistant
                && !is_synthetic_context_message(message))
            .then_some(index)
        })
}

fn summarize_prefix(messages: &[LlmMessage]) -> String {
    let mut lines = vec![
        "Compacted conversation summary.".to_string(),
        format!("- Messages compacted: {}", messages.len()),
    ];

    for message in messages.iter().rev().take(12).rev() {
        let role = message.role.as_str();
        let text = visible_message_text(message);
        if text.trim().is_empty() {
            continue;
        }
        lines.push(format!("- {role}: {}", truncate_summary_line(&text)));
    }

    lines.join("\n")
}

fn visible_message_text(message: &LlmMessage) -> String {
    message
        .content
        .iter()
        .map(|content| match content {
            LlmContent::Text { text } => text.clone(),
            LlmContent::Image { .. } => "[image]".to_string(),
            LlmContent::ToolCall {
                name, arguments, ..
            } => format!("requested tool {name} with {arguments}"),
            LlmContent::ToolResult { content, .. } => content.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_summary_line(text: &str) -> String {
    let max_chars = 320usize.min(estimate_text_tokens(text).saturating_mul(4).max(1));
    if text.chars().count() <= max_chars {
        return text.trim().to_string();
    }
    let mut end = 0usize;
    for (index, _) in text.char_indices().take(max_chars) {
        end = index;
    }
    format!("{}...", text[..end].trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_keeps_recent_user_turns_and_builds_context_message() {
        let messages = vec![
            LlmMessage::user("old one"),
            LlmMessage::assistant("answer"),
            LlmMessage::user("old two"),
            LlmMessage::assistant("answer"),
            LlmMessage::user("recent"),
        ];
        let settings = ContextWindowSettings {
            compact_keep_recent_turns: 1,
            ..Default::default()
        };

        let result = compact_messages(&messages, None, &settings).unwrap();

        assert_eq!(result.messages_removed, 4);
        assert_eq!(result.retained_messages.len(), 1);
        assert!(is_compact_summary_message(&result.context_messages[0]));
    }

    #[test]
    fn compact_turn_split_ignores_synthetic_context_messages() {
        let messages = vec![
            LlmMessage::user(format_compact_summary("old compacted work")),
            LlmMessage::user("old real"),
            LlmMessage::assistant("answer"),
            LlmMessage::user("recent real"),
        ];
        let settings = ContextWindowSettings {
            compact_keep_recent_turns: 1,
            ..Default::default()
        };

        let result = compact_messages(&messages, None, &settings).unwrap();

        assert_eq!(result.retained_messages.len(), 1);
        assert_eq!(result.messages_removed, 3);
    }

    #[test]
    fn prompt_too_long_classifier_ignores_rate_limits() {
        assert!(is_prompt_too_long_message(
            "maximum context length exceeded"
        ));
        assert!(!is_prompt_too_long_message(
            "rate limit: too many tokens per minute"
        ));
    }
}
