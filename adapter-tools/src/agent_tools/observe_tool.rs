//! # observe 工具
//!
//! 四工具模型中的观测工具。返回目标 child agent 的只读快照，
//! 融合 live control state 与对话投影。

use std::sync::Arc;

use astrcode_core::{ObserveParams, Result};
use astrcode_runtime_contract::tool::{
    Tool, ToolCapabilityMetadata, ToolContext, ToolDefinition, ToolExecutionResult,
    ToolPromptMetadata,
};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent_tools::{
    collab_result_mapping::{collaboration_error_result, map_collaboration_result},
    collaboration_executor::CollaborationExecutor,
};

const TOOL_NAME: &str = "observe";

/// 获取目标 child agent 只读快照的观测工具。
///
/// 只返回直接子 agent 的快照，非直接父、兄弟、跨树调用被拒绝。
/// 快照只返回状态、任务与最近输出尾部。
pub struct ObserveAgentTool {
    executor: Arc<dyn CollaborationExecutor>,
}

impl ObserveAgentTool {
    pub fn new(executor: Arc<dyn CollaborationExecutor>) -> Self {
        Self { executor }
    }

    fn build_description() -> String {
        r#"Get the current state snapshot of a specified sub-agent.

Use `observe` to decide the next action for one direct child.

- Use the exact `agentId` returned earlier
- Call it only when you cannot decide between `wait`, `send`, or `close` without a fresh snapshot
- Read the snapshot fields directly; `observe` no longer returns advisory next-step fields
- Treat the tail fields as short excerpts, not as full history

Do not poll repeatedly with no decision attached. If you are simply waiting for a running child,
pause briefly with your current shell tool (for example `sleep`) instead of spending another
tool call on `observe`. Do not alternate `sleep -> observe -> sleep -> observe` while no new
delivery or decision point has appeared. Do not use it for unrelated agents."#
            .to_string()
    }

    fn parameters_schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "agentId": {
                    "type": "string",
                    "description": "Stable ID of the sub-agent to observe."
                }
            },
            "required": ["agentId"]
        })
    }
}

#[async_trait]
impl Tool for ObserveAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: TOOL_NAME.to_string(),
            description: Self::build_description(),
            parameters: Self::parameters_schema(),
        }
    }

    fn capability_metadata(&self) -> ToolCapabilityMetadata {
        ToolCapabilityMetadata::builtin()
            .tag("agent")
            .tag("collaboration")
            // observe 是只读查询，可以安全并发
            .concurrency_safe(true)
            // observe 的 tool result 在 compact 模式下可折叠
            .compact_clearable(true)
            .prompt(
                ToolPromptMetadata::new(
                    "Observe child state when you need to decide the next action.",
                    "Use `observe` when the next decision depends on current child state. It is \
                     a synchronous query for one direct child and should usually answer `wait`, \
                     `send`, or `close`, not act as a polling loop.",
                )
                .caveat(
                    "Only returns snapshots for direct child agents. Never rewrite `agent-1` as \
                     `agent-01`.",
                )
                .caveat(
                    "Prefer one well-timed observe over repeated checking. If you are just \
                     waiting for a running child, use your current shell tool to sleep briefly and \
                     then continue, instead of polling `observe` again. Do not alternate \
                     `sleep -> observe -> sleep -> observe` while no new delivery has arrived.",
                )
                .caveat(
                    "`observe` only exposes recent output and the last turn tail. It is \
                     intentionally not a full transcript dump.",
                )
                .prompt_tag("collaboration"),
            )
    }

    async fn execute(
        &self,
        tool_call_id: String,
        input: Value,
        ctx: &ToolContext,
    ) -> Result<ToolExecutionResult> {
        let params = match serde_json::from_value::<ObserveParams>(input) {
            Ok(params) => params,
            Err(error) => {
                return Ok(collaboration_error_result(
                    tool_call_id,
                    TOOL_NAME,
                    format!("invalid observe params: {error}"),
                ));
            },
        };

        if let Err(err) = params.validate() {
            return Ok(collaboration_error_result(
                tool_call_id,
                TOOL_NAME,
                format!("invalid observe params: {err}"),
            ));
        }

        let result = self.executor.observe(params, ctx).await?;
        Ok(map_collaboration_result(tool_call_id, TOOL_NAME, result))
    }
}
