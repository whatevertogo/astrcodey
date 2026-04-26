use std::sync::Arc;

use astrcode_core::{Result, SendAgentParams};
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

const TOOL_NAME: &str = "send";

/// 统一父子协作消息入口。
///
/// 同一个 `send` 既可以向 direct child 发送下一条具体指令，
/// 也可以在 child 上下文中向 direct parent 发送 typed upward delivery。
pub struct SendAgentTool {
    executor: Arc<dyn CollaborationExecutor>,
}

impl SendAgentTool {
    pub fn new(executor: Arc<dyn CollaborationExecutor>) -> Self {
        Self { executor }
    }

    fn build_description() -> String {
        r#"Send a collaboration message along the direct parent/child edge.

Use `send` in one of two shapes:

- Downstream: `direction="child" + agentId + message (+ context)` sends the next concrete instruction to a direct child
- Upstream: `direction="parent" + kind + payload` sends a typed delivery to the direct parent from a child context

Do not use `send` for status checks, vague reminders, sibling chat, or cross-tree routing."#
            .to_string()
    }

    fn parameters_schema() -> Value {
        let progress_payload = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "message": { "type": "string" }
            },
            "required": ["message"]
        });
        let completed_payload = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "message": { "type": "string" },
                "findings": {
                    "type": "array",
                    "items": { "type": "string" }
                },
                "artifacts": {
                    "type": "array",
                    "items": { "type": "object" }
                }
            },
            "required": ["message"]
        });
        let failed_payload = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "message": { "type": "string" },
                "code": {
                    "type": "string",
                    "enum": ["transport", "provider_http", "stream_parse", "interrupted", "internal"]
                },
                "technicalMessage": { "type": "string" },
                "retryable": { "type": "boolean" }
            },
            "required": ["message", "code", "retryable"]
        });
        let close_request_payload = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "message": { "type": "string" },
                "reason": { "type": "string" }
            },
            "required": ["message"]
        });

        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "agentId": {
                    "type": "string",
                    "description": "Target direct child stable ID."
                },
                "direction": {
                    "type": "string",
                    "enum": ["child", "parent"],
                    "description": "Direct collaboration edge direction."
                },
                "message": {
                    "type": "string",
                    "description": "Concrete instruction for the child."
                },
                "context": {
                    "type": "string",
                    "description": "Optional supplementary context."
                },
                "kind": {
                    "type": "string",
                    "enum": ["progress", "completed", "failed", "close_request"]
                },
                "payload": {
                    "type": "object",
                    "description": "Typed upstream delivery payload selected by kind."
                }
            },
            "oneOf": [
                {
                    "required": ["direction", "agentId", "message"],
                    "properties": {
                        "direction": { "const": "child" }
                    }
                },
                {
                    "oneOf": [
                        {
                            "required": ["direction", "kind", "payload"],
                            "properties": {
                                "direction": { "const": "parent" },
                                "kind": { "const": "progress" },
                                "payload": progress_payload
                            }
                        },
                        {
                            "required": ["direction", "kind", "payload"],
                            "properties": {
                                "direction": { "const": "parent" },
                                "kind": { "const": "completed" },
                                "payload": completed_payload
                            }
                        },
                        {
                            "required": ["direction", "kind", "payload"],
                            "properties": {
                                "direction": { "const": "parent" },
                                "kind": { "const": "failed" },
                                "payload": failed_payload
                            }
                        },
                        {
                            "required": ["direction", "kind", "payload"],
                            "properties": {
                                "direction": { "const": "parent" },
                                "kind": { "const": "close_request" },
                                "payload": close_request_payload
                            }
                        }
                    ],
                    "not": {
                        "anyOf": [
                            { "required": ["agentId"] },
                            { "required": ["message"] },
                            { "required": ["context"] }
                        ]
                    }
                }
            ]
        })
    }
}

#[async_trait]
impl Tool for SendAgentTool {
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
            .concurrency_safe(true)
            // send 的 tool result 不应在 compact 模式下被折叠，
            // 因为它包含 childRef，LLM 需要逐字复用其中的 agentId
            .compact_clearable(false)
            .prompt(
                ToolPromptMetadata::new(
                    "Send a downstream instruction or an upstream typed delivery on the direct collaboration edge.",
                    "Use `send` with `direction=\"child\" + agentId + message` when you need a direct child to continue. \
                     Use `send` with `direction=\"parent\" + kind + payload` when you need to report progress, completion, \
                     failure, or a close request to your direct parent. The same middle-layer agent \
                     can use both directions in one turn.",
                )
                .caveat(
                    "Downstream sends only target direct children. Upstream sends never accept an \
                     explicit parent id; routing comes from the current child context.",
                )
                .caveat(
                    "Do not use `send` for status checks. If you already know a child is still \
                     running and are simply waiting, do not call `observe` repeatedly either; wait \
                     briefly with your current shell tool instead. Do not alternate \
                     `sleep -> observe -> sleep -> observe` while the child has not produced a \
                     new delivery.",
                )
                .caveat(
                    "Messages must stay on the direct parent/child edge. No sibling chat, no \
                     cross-tree routing, no vague reminders.",
                )
                .caveat(
                    "Keep downstream messages delta-oriented, and keep upstream messages \
                     collaboration-oriented. Do not restate the whole branch transcript.",
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
        let params = match serde_json::from_value::<SendAgentParams>(input) {
            Ok(params) => params,
            Err(error) => {
                return Ok(collaboration_error_result(
                    tool_call_id,
                    TOOL_NAME,
                    format!("invalid send params: {error}"),
                ));
            },
        };

        if let Err(err) = params.validate() {
            return Ok(collaboration_error_result(
                tool_call_id,
                TOOL_NAME,
                format!("invalid send params: {err}"),
            ));
        }

        let result = self.executor.send(params, ctx).await?;
        Ok(map_collaboration_result(tool_call_id, TOOL_NAME, result))
    }
}
