use std::collections::BTreeMap;

use astrcode_extension_sdk::{
    extension::{ExtensionCall, ExtensionError, ToolContext, ToolHandler, ToolPlanContext},
    host::{
        HOST_TOOL_RESULT_DEFAULT_MAX_BYTES, HOST_TOOL_RESULT_MAX_BYTES, HOST_TOOL_RESULT_MIN_BYTES,
        HostToolResultReadRequest,
    },
    tool::{HostResource, ToolDefinition, ToolExecutionResult, ToolOrigin, ToolPlan, ToolResult},
};
use serde::Deserialize;

use crate::invalid_input;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReadToolResultArgs {
    artifact_id: String,
    #[serde(default)]
    byte_offset: usize,
    #[serde(default)]
    max_bytes: Option<usize>,
}

pub(super) struct ReadToolResultHandler;

#[async_trait::async_trait]
impl ToolHandler for ReadToolResultHandler {
    async fn plan(&self, context: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        let args: ReadToolResultArgs = context.arguments()?;
        validate(&args)?;
        Ok(ToolPlan::host(HostResource::ToolResultArtifact))
    }

    async fn execute(&self, context: ToolContext) -> Result<ToolExecutionResult, ExtensionError> {
        let args: ReadToolResultArgs = context.arguments()?;
        validate(&args)?;
        let output = context
            .host()
            .tool_results()?
            .read(HostToolResultReadRequest {
                artifact_id: args.artifact_id,
                byte_offset: args.byte_offset,
                max_bytes: args.max_bytes.unwrap_or(HOST_TOOL_RESULT_DEFAULT_MAX_BYTES),
            })
            .await?;
        let mut content = output.content;
        if let Some(next) = output.next_byte_offset {
            content.push_str(&format!(
                "\n\n[Truncated. Continue with byteOffset={next}.]"
            ));
        }
        Ok(ToolResult::success(content)
            .with_metadata(BTreeMap::from([
                ("artifactId".into(), serde_json::json!(output.artifact_id)),
                ("bytes".into(), serde_json::json!(output.bytes)),
                ("byteOffset".into(), serde_json::json!(output.byte_offset)),
                (
                    "returnedBytes".into(),
                    serde_json::json!(output.returned_bytes),
                ),
                ("hasMore".into(), serde_json::json!(output.has_more)),
                (
                    "nextByteOffset".into(),
                    serde_json::json!(output.next_byte_offset),
                ),
            ]))
            .into())
    }
}

fn validate(args: &ReadToolResultArgs) -> Result<(), ExtensionError> {
    if args.artifact_id.trim().is_empty() {
        return Err(invalid_input(
            "artifactId cannot be empty",
            "follow the read_tool_result parameter schema",
        ));
    }
    if args.max_bytes.is_some_and(|value| {
        !(HOST_TOOL_RESULT_MIN_BYTES..=HOST_TOOL_RESULT_MAX_BYTES).contains(&value)
    }) {
        return Err(invalid_input(
            format!(
                "maxBytes must be between {HOST_TOOL_RESULT_MIN_BYTES} and \
                 {HOST_TOOL_RESULT_MAX_BYTES}"
            ),
            "follow the read_tool_result parameter schema",
        ));
    }
    Ok(())
}

pub(super) fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "read_tool_result".into(),
        description: concat!(
            "Read a persisted tool result by its session-scoped artifact ID.\n\n",
            "Use this only when a previous tool result says it was persisted. ",
            "Continue with byteOffset until hasMore is false."
        )
        .into(),
        strict: true,
        origin: ToolOrigin::Bundled,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "artifactId": { "type": "string", "description": "Artifact ID returned by a persisted tool result." },
                "byteOffset": { "type": "integer", "minimum": 0, "description": "UTF-8 byte offset returned by the previous page." },
                "maxBytes": {
                    "type": "integer",
                    "minimum": HOST_TOOL_RESULT_MIN_BYTES,
                    "maximum": HOST_TOOL_RESULT_MAX_BYTES,
                    "description": "Maximum UTF-8 bytes to return."
                }
            },
            "required": ["artifactId"],
            "additionalProperties": false
        }),
    }
}
