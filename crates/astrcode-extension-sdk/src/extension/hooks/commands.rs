//! Slash command types: definitions, completions, and execution results.

use serde::{Deserialize, Serialize};

use super::types::StatusItemUpdatePayload;
pub use crate::wire::manifest::{CommandAvailability, CommandExecution, SessionCommandKind};

/// 扩展注册的斜杠命令。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommand {
    /// 命令名称（不含前导斜杠 `/`）。
    pub name: String,
    /// 人类可读的命令描述。
    pub description: String,
    /// 参数的 JSON Schema 定义。
    #[serde(deserialize_with = "deserialize_required_option")]
    pub args_schema: Option<serde_json::Value>,
    /// 是否要求当前 session 空闲。
    pub requires_idle: bool,
    /// 是否提供参数补全。
    pub argument_completions: bool,
    /// 同名命令冲突时的优先级，数值越高优先级越高。
    pub priority: i32,
    /// 命令是否只应出现在具备交互 UI 的传输中。
    pub availability: CommandAvailability,
    /// 命令执行的责任边界。
    pub execution: CommandExecution,
}

/// Typed request produced by a command extension for the host to execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionCommandIntent {
    CompactSession {
        #[serde(deserialize_with = "deserialize_required_option")]
        keep_recent_turns: Option<usize>,
    },
    SelectModel,
}

impl SessionCommandIntent {
    pub const fn kind(&self) -> SessionCommandKind {
        match self {
            Self::CompactSession { .. } => SessionCommandKind::CompactSession,
            Self::SelectModel => SessionCommandKind::SelectModel,
        }
    }
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
    /// 请求宿主在当前 session operation 内执行一个特权命令。
    HostCommand { intent: SessionCommandIntent },
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

    pub const fn host_command(intent: SessionCommandIntent) -> Self {
        Self::HostCommand { intent }
    }
}

fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}
