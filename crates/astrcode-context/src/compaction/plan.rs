//! Compact 输入规划。
//!
//! 这里把原始 LLM 消息转换成适合摘要模型阅读的紧凑消息序列，并识别
//! 是否存在上一轮 compact summary。真正的“保留哪些尾部消息”在
//! `compaction::CompactJob` 中完成。

use astrcode_core::llm::{LlmContent, LlmMessage, LlmRole};

use super::{
    assemble::{
        collapse_compaction_whitespace, parse_compact_summary_message, sanitize_compact_summary,
    },
    is_synthetic_context_message,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompactPromptMode {
    /// 首次压缩，没有可合并的旧 summary。
    Fresh,
    /// 已有旧 summary，本次要求模型输出合并后的完整 summary。
    Incremental { previous_summary: String },
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedCompactInput {
    /// 归一化后的历史消息，用作 compact request 的对话正文。
    pub messages: Vec<LlmMessage>,
    /// 决定使用 fresh 还是 incremental prompt。
    pub prompt_mode: CompactPromptMode,
}

/// 准备摘要模型要读取的消息前缀。
///
/// Synthetic compact summary 不再作为普通用户消息重复压缩，而是转成
/// incremental prompt 的 previous summary。
pub(crate) fn prepare_compact_input(messages: &[LlmMessage]) -> PreparedCompactInput {
    let prompt_mode = latest_previous_summary(messages)
        .map(|previous_summary| CompactPromptMode::Incremental { previous_summary })
        .unwrap_or(CompactPromptMode::Fresh);
    let messages = messages
        .iter()
        .filter_map(normalize_compaction_message)
        .collect::<Vec<_>>();
    PreparedCompactInput {
        messages,
        prompt_mode,
    }
}

/// 把多模态/工具内容降级成摘要模型可读的纯文本。
pub(crate) fn visible_message_text(message: &LlmMessage) -> String {
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

/// 找到最近一次 compact summary，用于 incremental compact。
fn latest_previous_summary(messages: &[LlmMessage]) -> Option<String> {
    messages.iter().rev().find_map(|message| {
        if message.role != LlmRole::User {
            return None;
        }
        message.content.iter().find_map(|content| match content {
            LlmContent::Text { text } => parse_compact_summary_message(text)
                .map(|envelope| sanitize_compact_summary(&envelope.summary)),
            _ => None,
        })
    })
}

/// 去掉 system/synthetic message，并把工具消息归一化为普通 tool result。
fn normalize_compaction_message(message: &LlmMessage) -> Option<LlmMessage> {
    match message.role {
        LlmRole::System => None,
        LlmRole::User if is_synthetic_context_message(message) => None,
        LlmRole::User => {
            let text = collapse_compaction_whitespace(&visible_message_text(message));
            (!text.is_empty()).then(|| LlmMessage::user(text))
        },
        LlmRole::Assistant => {
            let text = collapse_compaction_whitespace(&visible_message_text(message));
            (!text.is_empty()).then(|| LlmMessage::assistant(text))
        },
        LlmRole::Tool => {
            let text = collapse_compaction_whitespace(&visible_message_text(message));
            (!text.is_empty()).then(|| {
                LlmMessage::tool(
                    message.name.clone().unwrap_or_else(|| "tool".to_string()),
                    first_tool_call_id(message).unwrap_or_else(|| "tool-result".to_string()),
                    text,
                    first_tool_is_error(message),
                )
            })
        },
    }
}

fn first_tool_call_id(message: &LlmMessage) -> Option<String> {
    message.content.iter().find_map(|content| match content {
        LlmContent::ToolResult { tool_call_id, .. } => Some(tool_call_id.clone()),
        _ => None,
    })
}

fn first_tool_is_error(message: &LlmMessage) -> bool {
    message
        .content
        .iter()
        .any(|content| matches!(content, LlmContent::ToolResult { is_error: true, .. }))
}
