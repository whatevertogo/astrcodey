//! Agent 工具调用的数据类型定义。
//!
//! 包含工具调用从 LLM 流式响应中积累、预处理、到最终执行各阶段的类型。

use std::collections::HashMap;

use astrcode_core::{
    extension::AfterToolResult,
    permission::ApprovalSource,
    tool::{ExecutionMode, ToolDefinition, ToolResult},
};

use super::turn_publish::TurnEvents;
use crate::turn_stages::TurnState;

/// Provider stream 中解析出的工具调用草稿，参数可能仍是逐段拼接的原始 JSON 字符串。
pub(crate) struct StreamedToolCall {
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
}

#[derive(Clone)]
pub(crate) struct PreparedToolInvocation {
    pub(crate) index: usize,
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) tool_input: serde_json::Value,
    pub(crate) mode: ExecutionMode,
    pub(crate) outcome: PreparedToolInvocationOutcome,
}

pub(crate) struct PreparedToolBatch {
    pub(crate) prepared: Vec<PreparedToolInvocation>,
    pub(crate) pre_executed: HashMap<usize, ToolResult>,
}

pub(crate) struct DeclaredToolBatch {
    pub(crate) prepared: Vec<PreparedToolInvocation>,
    pub(crate) pre_executed: HashMap<usize, ToolResult>,
}

pub(crate) struct ExecuteDeclaredToolBatch<'a> {
    pub(crate) declared: DeclaredToolBatch,
    pub(crate) tools: &'a [ToolDefinition],
    pub(crate) state: &'a mut TurnState,
    pub(crate) publisher: std::sync::Arc<TurnEvents>,
}

#[derive(Default)]
pub(crate) struct CommittedToolResults {
    pub(crate) discovered_tools: Vec<String>,
    pub(crate) tool_results: Vec<AfterToolResult>,
}

impl CommittedToolResults {
    pub(crate) fn extend(&mut self, other: Self) {
        self.discovered_tools.extend(other.discovered_tools);
        self.tool_results.extend(other.tool_results);
    }
}

#[derive(Clone)]
pub(crate) enum PreparedToolInvocationOutcome {
    Ready,
    Blocked(ToolResult),
    /// 同 step 内与先前调用相同 `(toolName, args)`，复用 Primary 的最终结果。
    DuplicateSameStep,
    /// 需用户审批后执行。
    NeedsApproval {
        prompt: String,
        rule_key: Option<String>,
        source: ApprovalSource,
    },
}

#[derive(Clone)]
pub(crate) struct ExecutableToolInvocation {
    pub(crate) index: usize,
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) tool_input: serde_json::Value,
}

impl PreparedToolInvocation {
    /// 将预处理后的工具调用转换为可执行任务输入。
    pub(crate) fn to_executable(&self) -> ExecutableToolInvocation {
        ExecutableToolInvocation {
            index: self.index,
            call_id: self.call_id.clone(),
            name: self.name.clone(),
            tool_input: self.tool_input.clone(),
        }
    }
}
