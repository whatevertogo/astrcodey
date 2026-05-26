//! Agent 工具调用的数据类型定义。
//!
//! 包含工具调用从 LLM 流式响应中积累、预处理、到最终执行各阶段的类型。

use std::collections::BTreeMap;

use astrcode_core::tool::{ExecutionMode, ToolDefinition, ToolResult};

use super::turn_publish::TurnPublisher;
use crate::turn_stages::TurnState;

/// 等待执行的工具调用，在 LLM 流式响应中逐步积累参数。
pub struct PendingToolCall {
    /// 工具调用的唯一标识
    pub call_id: String,
    /// 工具名称
    pub name: String,
    /// 工具调用的 JSON 参数（可能跨多个 delta 事件拼接）
    pub arguments: String,
}

pub struct PreparedToolCall {
    pub index: usize,
    pub call_id: String,
    pub name: String,
    pub tool_input: serde_json::Value,
    pub mode: ExecutionMode,
    pub outcome: PreparedToolOutcome,
}

pub struct ExecuteToolCalls<'a> {
    pub prepared: &'a [PreparedToolCall],
    pub tools: &'a [ToolDefinition],
    pub state: &'a mut TurnState,
    pub publisher: std::sync::Arc<TurnPublisher>,
}

pub struct CommitToolResults<'a> {
    pub prepared: &'a [PreparedToolCall],
    pub results: BTreeMap<usize, ToolResult>,
    pub state: &'a mut TurnState,
    pub publisher: std::sync::Arc<TurnPublisher>,
}

pub struct PendingCommittedToolResult {
    pub call_id: String,
    pub tool_name: String,
    pub result: ToolResult,
    pub arguments: String,
    pub arguments_json: Option<serde_json::Value>,
}

pub enum ToolExecutionStep {
    Blocked(ToolResult),
    Parallel(ExecutableToolCall),
    Sequential(ExecutableToolCall),
}

pub enum PreparedToolOutcome {
    Ready,
    Blocked(ToolResult),
}

#[derive(Clone)]
pub struct ExecutableToolCall {
    pub index: usize,
    pub call_id: String,
    pub name: String,
    pub tool_input: serde_json::Value,
}

impl PreparedToolCall {
    /// 将预处理后的工具调用转换为可执行任务输入。
    pub fn to_executable(&self) -> ExecutableToolCall {
        ExecutableToolCall {
            index: self.index,
            call_id: self.call_id.clone(),
            name: self.name.clone(),
            tool_input: self.tool_input.clone(),
        }
    }
}
