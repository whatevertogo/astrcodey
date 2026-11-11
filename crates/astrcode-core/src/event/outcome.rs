//! 执行结束的领域事实；成功、失败、取消和进程中断互不混用。

use serde::{Deserialize, Serialize};

use crate::tool::ToolResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnCompletionReason {
    Stop,
    ToolCalls,
    Length,
    Refusal,
}

impl TurnCompletionReason {
    pub fn from_provider(value: &str) -> Option<Self> {
        match value {
            "stop" | "end_turn" | "stop_sequence" => Some(Self::Stop),
            "tool_calls" | "tool_use" | "function_call" => Some(Self::ToolCalls),
            "length" | "max_tokens" => Some(Self::Length),
            "refusal" | "content_filter" => Some(Self::Refusal),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::ToolCalls => "tool_calls",
            Self::Length => "length",
            Self::Refusal => "refusal",
        }
    }
}

impl std::fmt::Display for TurnCompletionReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnEndReason {
    Completed { finish_reason: TurnCompletionReason },
    Failed { error: String },
    Cancelled { reason: String },
    Interrupted { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum StepEndReason {
    Continue,
    Completed { finish_reason: TurnCompletionReason },
    Failed { error: String },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AssistantAttemptOutcome {
    Completed,
    Failed { error: String },
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallOutcome {
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl ToolCallOutcome {
    /// 工具正常返回也可能包含业务错误；非正常结束必须携带错误结果。
    pub fn settled_call_failed(self, result: &ToolResult) -> bool {
        self != Self::Completed || result.is_error
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentSessionOutcome {
    Completed { summary: String },
    Failed { error: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompactionOutcome {
    Completed { messages_removed: usize },
    Skipped { reason: String },
    Failed { reason: String },
}
