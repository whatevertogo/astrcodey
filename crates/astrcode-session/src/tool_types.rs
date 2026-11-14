//! Agent 工具调用的数据类型定义。
//!
//! 包含工具调用从 LLM 流式响应中积累、预处理、到最终执行各阶段的类型。

use std::collections::BTreeMap;

use astrcode_core::{
    permission::ApprovalSource,
    tool::{ExecutionMode, ToolDefinition, ToolExecutionResult, ToolResult, access::ToolPlan},
};

use super::turn_publish::TurnEvents;
use crate::turn_stages::TurnState;

/// Provider stream 中解析出的工具调用草稿，参数可能仍是逐段拼接的原始 JSON 字符串。
#[derive(Debug)]
pub(crate) struct StreamedToolCall {
    pub(crate) call_id: String,
    pub(crate) provider_call_id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
}

#[derive(Clone)]
pub(crate) struct PreparedToolInvocation {
    pub(crate) index: usize,
    pub(crate) call_id: String,
    pub(crate) provider_call_id: String,
    pub(crate) name: String,
    pub(crate) tool_input: serde_json::Value,
    pub(crate) raw_arguments: String,
    pub(crate) plan: ToolPlan,
    pub(crate) mode: ExecutionMode,
    pub(crate) discovery_gate: Option<String>,
    pub(crate) disposition: PreparedToolDisposition,
}

/// 一个 agent step 中已完成预处理、待执行的工具调用集合（prepare 阶段的纯数据产物）。
///
/// `calls` 保持 provider 原始顺序。该批次被 `execute_and_commit` 消费后即失效。
pub(crate) struct ToolBatch {
    pub(crate) calls: Vec<PreparedToolInvocation>,
}

/// 执行阶段的批次包装：批数据 + 执行所需的运行期依赖。
///
/// 与 [`ToolBatch`] 的区别：`ToolBatch` 只承载数据，`ExecuteToolBatch` 额外附加
/// 工具定义、turn 状态与事件发布器，生命周期仅限单次 `execute_and_commit` 调用。
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
    /// 所有尚未被会话记忆消解的审批条件，按声明顺序逐项确认。
    AwaitApprovals(Vec<PreparedToolApproval>),
}

#[derive(Clone)]
pub(crate) struct PreparedToolApproval {
    pub(crate) prompt: String,
    pub(crate) rule_key: Option<String>,
    pub(crate) source: ApprovalSource,
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
    /// 通过 DerefMut 修改 `result` 会绕过 `artifact_state` 的同步：调用方修改内容后
    /// 必须自行判断 `artifact_state` 是否仍然成立（例如把已 Persisted 的结果改小
    /// 仍安全；把 Inline 结果改大到超限则不会自动触发持久化）。
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
    pub(crate) plan: ToolPlan,
}

impl PreparedToolInvocation {
    pub(crate) fn to_executable(&self) -> ExecutableToolInvocation {
        ExecutableToolInvocation {
            index: self.index,
            call_id: self.call_id.clone(),
            name: self.name.clone(),
            tool_input: self.tool_input.clone(),
            plan: self.plan.clone(),
        }
    }
}
