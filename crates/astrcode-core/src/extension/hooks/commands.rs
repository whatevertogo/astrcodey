//! Slash command types: definitions, completions, and execution results.

use serde::{Deserialize, Serialize};

use super::types::StatusItemUpdatePayload;

/// 扩展注册的斜杠命令。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommand {
    /// 命令名称（不含前导斜杠 `/`）。
    pub name: String,
    /// 人类可读的命令描述。
    pub description: String,
    /// 参数的 JSON Schema 定义。
    pub args_schema: Option<serde_json::Value>,
    /// 是否要求当前 session 空闲。
    #[serde(default)]
    pub requires_idle: bool,
    /// 是否提供参数补全。
    #[serde(default)]
    pub argument_completions: bool,
    /// 同来源命令冲突时的优先级，数值越高优先级越高。
    #[serde(default)]
    pub priority: i32,
}

/// 斜杠命令参数补全项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandCompletionItem {
    pub label: String,
    pub insert_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 斜杠命令参数补全结果。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandCompletions {
    #[serde(default)]
    pub items: Vec<CommandCompletionItem>,
    #[serde(default)]
    pub truncated: bool,
}

/// 扩展斜杠命令的执行结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExtensionCommandResult {
    /// 只展示文本，不启动 agent turn。
    Display {
        content: String,
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status_update: Option<StatusItemUpdatePayload>,
    },
    /// 同步处理完成，不启动 agent turn。
    Handled { message: String },
    /// 启动一个 agent turn，携带附加指令合并到用户消息中。
    StartTurn { instructions: String },
}

impl ExtensionCommandResult {
    pub fn display(content: impl Into<String>, is_error: bool) -> Self {
        Self::Display {
            content: content.into(),
            is_error,
            status_update: None,
        }
    }

    pub fn display_with_status(
        content: impl Into<String>,
        is_error: bool,
        status_update: StatusItemUpdatePayload,
    ) -> Self {
        Self::Display {
            content: content.into(),
            is_error,
            status_update: Some(status_update),
        }
    }

    pub fn handled(message: impl Into<String>) -> Self {
        Self::Handled {
            message: message.into(),
        }
    }

    pub fn start_turn(instructions: impl Into<String>) -> Self {
        Self::StartTurn {
            instructions: instructions.into(),
        }
    }
}
