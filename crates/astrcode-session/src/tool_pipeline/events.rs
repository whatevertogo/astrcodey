use astrcode_core::{event::DurableEventPayload, tool::ToolResult};

use crate::{
    tool_types::{PreparedToolInvocation, ToolBatch, ToolExecutionOutcome},
    turn_context::TurnError,
    turn_publish::TurnEvents,
};

// ─── Tool event & message helpers ────────────────────────────────────────

pub(super) async fn declare_tool_batch(
    publisher: &TurnEvents,
    batch: &ToolBatch,
) -> Result<(), TurnError> {
    for call in &batch.calls {
        declare_tool_call(publisher, call).await?;
    }
    Ok(())
}

async fn declare_tool_call(
    publisher: &TurnEvents,
    call: &PreparedToolInvocation,
) -> Result<(), TurnError> {
    publisher
        .durable(DurableEventPayload::ToolCallRequested {
            call_id: call.call_id.clone().into(),
            tool_name: call.name.clone(),
            arguments: call.tool_input.clone(),
            raw_arguments: call.raw_arguments.clone(),
        })
        .await
}

pub(super) async fn finish_tool_call(
    publisher: &TurnEvents,
    call_id: &str,
    tool_name: String,
    outcome: &ToolExecutionOutcome,
    arguments: String,
    arguments_json: Option<serde_json::Value>,
) -> Result<(), TurnError> {
    let payload = match outcome {
        ToolExecutionOutcome::Completed(result) => DurableEventPayload::ToolCallCompleted {
            call_id: call_id.into(),
            tool_name,
            result: result.result.clone(),
            arguments,
            arguments_json,
        },
        ToolExecutionOutcome::Failed {
            error,
            metadata,
            duration_ms,
        } => DurableEventPayload::ToolCallFailed {
            call_id: call_id.into(),
            tool_name,
            error: error.clone(),
            metadata: metadata.clone(),
            duration_ms: *duration_ms,
            arguments,
            arguments_json,
        },
        ToolExecutionOutcome::Cancelled {
            reason,
            duration_ms,
        } => DurableEventPayload::ToolCallCancelled {
            call_id: call_id.into(),
            tool_name,
            reason: reason.clone(),
            duration_ms: *duration_ms,
            arguments,
            arguments_json,
        },
    };
    publisher.durable(payload).await
}

pub(super) fn tool_result_for_output(outcome: &ToolExecutionOutcome) -> ToolResult {
    match outcome {
        ToolExecutionOutcome::Completed(result) => result.result.clone(),
        ToolExecutionOutcome::Failed {
            error,
            metadata,
            duration_ms,
        } => ToolResult::error(error.clone())
            .with_metadata(metadata.clone())
            .with_duration_ms(*duration_ms),
        ToolExecutionOutcome::Cancelled {
            reason,
            duration_ms,
        } => ToolResult::error(format!("Tool cancelled: {reason}")).with_duration_ms(*duration_ms),
    }
}

pub(super) fn missing_tool_outcome(call: &PreparedToolInvocation) -> ToolExecutionOutcome {
    ToolExecutionOutcome::failed(format!("Tool '{}' did not produce an outcome", call.name))
}
