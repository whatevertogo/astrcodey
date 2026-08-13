//! Session compact 的稳定领域契约。

use serde::{Deserialize, Serialize};

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
