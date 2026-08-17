//! Session compact 的稳定领域契约。

use serde::{Deserialize, Serialize};

use crate::llm::{LlmContent, LlmMessage, LlmRole};

pub const COMPACT_SUMMARY_MARKER: &str = "<compact_summary>";
pub const POST_COMPACT_CONTEXT_MARKER: &str = "<post_compact_context>";

/// 判断消息是否是 compact 后注入的 synthetic context message。
pub fn is_compact_summary_message(message: &LlmMessage) -> bool {
    message.role == LlmRole::User
        && message
            .content
            .iter()
            .filter_map(LlmContent::as_text)
            .any(is_compact_summary_text)
}

/// 检测文本内容是否以 compact summary 标记开头。
pub fn is_compact_summary_text(content: &str) -> bool {
    content.trim_start().starts_with(COMPACT_SUMMARY_MARKER)
}

/// 判断消息是否是 compact/post-compact 注入的 synthetic context message。
pub fn is_synthetic_context_message(message: &LlmMessage) -> bool {
    is_compact_summary_message(message)
        || (message.role == LlmRole::User
            && message
                .content
                .iter()
                .filter_map(LlmContent::as_text)
                .any(|text| text.trim_start().starts_with(POST_COMPACT_CONTEXT_MARKER)))
}

/// 触发 compact 的来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactTrigger {
    /// 自动阈值触发。
    AutoThreshold,
    /// 用户手动执行 compact 命令。
    ManualCommand,
    /// LLM 返回 prompt_too_long 后的补救 compact。
    ReactivePromptTooLong,
}

impl CompactTrigger {
    /// 返回事件记录和审计使用的稳定标识。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AutoThreshold => "auto_threshold",
            Self::ManualCommand => "manual_command",
            Self::ReactivePromptTooLong => "reactive_prompt_too_long",
        }
    }
}

/// Compact 使用的策略，记录在事件中用于 replay 和审计。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompactStrategy {
    Auto,
    Manual {
        #[serde(skip_serializing_if = "Option::is_none")]
        keep_recent_turns: Option<usize>,
    },
    ReactivePromptTooLong,
}

impl CompactStrategy {
    /// 返回该策略唯一对应的触发来源，避免调用方分别传递两个可能冲突的事实。
    pub const fn trigger(self) -> CompactTrigger {
        match self {
            Self::Auto => CompactTrigger::AutoThreshold,
            Self::Manual { .. } => CompactTrigger::ManualCommand,
            Self::ReactivePromptTooLong => CompactTrigger::ReactivePromptTooLong,
        }
    }

    /// 返回策略显式指定的保留 turn 数；其它策略使用 context 默认配置。
    pub const fn keep_recent_turns(self) -> Option<usize> {
        match self {
            Self::Manual { keep_recent_turns } => keep_recent_turns,
            Self::Auto | Self::ReactivePromptTooLong => None,
        }
    }
}
