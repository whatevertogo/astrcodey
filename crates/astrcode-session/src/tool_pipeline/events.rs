use astrcode_core::{event::EventPayload, tool::ToolResult};

use crate::{
    tool_types::{DeclaredToolBatch, PreparedToolBatch, PreparedToolInvocation},
    turn_context::TurnError,
    turn_publish::TurnEvents,
};

// ─── Tool event & message helpers ────────────────────────────────────────

pub(super) async fn declare_tool_batch(
    publisher: &TurnEvents,
    batch: PreparedToolBatch,
) -> Result<DeclaredToolBatch, TurnError> {
    for call in &batch.prepared {
        declare_tool_call(publisher, call).await?;
    }
    Ok(DeclaredToolBatch {
        prepared: batch.prepared,
        pre_executed: batch.pre_executed,
    })
}

async fn declare_tool_call(
    publisher: &TurnEvents,
    call: &PreparedToolInvocation,
) -> Result<(), TurnError> {
    publisher
        .durable(EventPayload::ToolCallRequested {
            call_id: call.call_id.clone().into(),
            tool_name: call.name.clone(),
            arguments: call.tool_input.clone(),
        })
        .await
}

pub(super) async fn complete_tool_call(
    publisher: &TurnEvents,
    call_id: &str,
    tool_name: String,
    result: ToolResult,
    arguments: String,
    arguments_json: Option<serde_json::Value>,
) -> Result<(), TurnError> {
    publisher
        .durable(EventPayload::ToolCallCompleted {
            call_id: call_id.into(),
            tool_name,
            result,
            arguments,
            arguments_json,
        })
        .await
}

pub(super) fn missing_tool_result(call: &PreparedToolInvocation) -> ToolResult {
    let message = format!("Tool '{}' did not produce a result", call.name);
    ToolResult {
        call_id: call.call_id.clone(),
        content: message.clone(),
        is_error: true,
        error: Some(message),
        metadata: Default::default(),
        duration_ms: None,
    }
}

pub(super) fn tool_ui_response_error_result(call_id: &str, message: &str) -> ToolResult {
    ToolResult {
        call_id: call_id.to_string(),
        content: message.to_string(),
        is_error: true,
        error: Some(message.to_string()),
        metadata: Default::default(),
        duration_ms: None,
    }
}
