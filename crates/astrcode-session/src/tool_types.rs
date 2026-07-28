//! Agent 工具调用的数据类型定义。
//!
//! 包含工具调用从 LLM 流式响应中积累、预处理、到最终执行各阶段的类型。

use std::collections::{BTreeMap, HashMap};

use astrcode_core::{
    permission::ApprovalSource,
    tool::{ExecutionMode, ToolDefinition, ToolExecutionResult, ToolResult},
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
    pub(crate) raw_arguments: Option<String>,
    pub(crate) mode: ExecutionMode,
    pub(crate) discovery_gate: Option<String>,
    pub(crate) disposition: PreparedToolDisposition,
}

pub(crate) struct ToolBatch {
    pub(crate) calls: Vec<PreparedToolInvocation>,
    pub(crate) pre_executed: HashMap<usize, ToolExecutionOutcome>,
}

pub(crate) struct ExecuteToolBatch<'a> {
    pub(crate) batch: ToolBatch,
    pub(crate) tools: &'a [ToolDefinition],
    pub(crate) state: &'a mut TurnState,
    pub(crate) publisher: std::sync::Arc<TurnEvents>,
}

#[derive(Clone)]
pub(crate) enum PreparedToolDisposition {
    Execute,
    Rejected {
        error: String,
    },
    /// 同 step 内与先前调用相同 `(toolName, args)`，复用 Primary 的最终结果。
    ReuseSameStep,
    /// 需用户审批后执行。
    AwaitApproval {
        prompt: String,
        rule_key: Option<String>,
        source: ApprovalSource,
    },
}

/// 一次工具调用在 session 编排层的终态。
///
/// `Completed` 包含工具正常返回的结果，结果本身仍可为业务错误；`Failed`
/// 表示执行或编排失败；`Cancelled` 只表示显式取消。
#[derive(Clone, Debug)]
pub(crate) enum ToolExecutionOutcome {
    Completed(ToolResultCommit),
    Failed {
        error: String,
        metadata: BTreeMap<String, serde_json::Value>,
        duration_ms: Option<u64>,
    },
    Cancelled {
        reason: String,
        duration_ms: Option<u64>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolResultArtifactState {
    Inline,
    Persisted,
}

/// Session-private commit state kept outside the wire-facing [`ToolResult`].
#[derive(Clone, Debug)]
pub(crate) struct ToolResultCommit {
    pub(crate) result: ToolResult,
    pub(crate) discovered_tool_names: Vec<String>,
    pub(crate) artifact_state: ToolResultArtifactState,
}

impl ToolResultCommit {
    #[cfg(test)]
    pub(crate) fn completed(result: ToolResult) -> Self {
        Self {
            result,
            discovered_tool_names: Vec::new(),
            artifact_state: ToolResultArtifactState::Inline,
        }
    }

    pub(crate) fn from_execution_result(result: ToolExecutionResult) -> Self {
        let (result, discovered_tool_names) = result.into_parts();
        Self {
            result,
            discovered_tool_names,
            artifact_state: ToolResultArtifactState::Inline,
        }
    }
}

impl std::ops::Deref for ToolResultCommit {
    type Target = ToolResult;

    fn deref(&self) -> &Self::Target {
        &self.result
    }
}

impl std::ops::DerefMut for ToolResultCommit {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.result
    }
}

impl ToolExecutionOutcome {
    pub(crate) fn failed(error: impl Into<String>) -> Self {
        Self::Failed {
            error: error.into(),
            metadata: BTreeMap::new(),
            duration_ms: None,
        }
    }

    pub(crate) fn cancelled(reason: impl Into<String>, duration_ms: Option<u64>) -> Self {
        Self::Cancelled {
            reason: reason.into(),
            duration_ms,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ExecutableToolInvocation {
    pub(crate) index: usize,
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) tool_input: serde_json::Value,
}

pub(crate) fn tool_call_completion_arguments(
    tool_input: serde_json::Value,
    raw_arguments: Option<String>,
) -> (String, Option<serde_json::Value>) {
    match raw_arguments {
        Some(raw) => (raw, None),
        None => (tool_input.to_string(), Some(tool_input)),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_arguments_distinguish_raw_from_valid_json_strings() {
        let cases = [
            (
                serde_json::Value::String("hello".into()),
                None,
                r#""hello""#,
                true,
            ),
            (
                serde_json::Value::String(r#"{"query":"unfinished"#.into()),
                Some(r#"{"query":"unfinished"#.into()),
                r#"{"query":"unfinished"#,
                false,
            ),
        ];

        for (input, raw, expected_text, has_json) in cases {
            let (text, json) = tool_call_completion_arguments(input, raw);
            assert_eq!(text, expected_text);
            assert_eq!(json.is_some(), has_json);
        }
    }
}
