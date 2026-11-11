use astrcode_core::event::{DurableEventPayload, ToolCallOutcome};

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
            provider_call_id: call.provider_call_id.clone(),
            tool_name: call.name.clone(),
            arguments: call.raw_arguments.clone(),
        })
        .await
}

pub(super) async fn finish_tool_call(
    publisher: &TurnEvents,
    call_id: &str,
    outcome: &ToolExecutionOutcome,
) -> Result<(), TurnError> {
    let (outcome, result) = match outcome {
        ToolExecutionOutcome::Completed(result) => {
            (ToolCallOutcome::Completed, result.result.clone())
        },
        ToolExecutionOutcome::Failed {
            error,
            metadata,
            duration_ms,
        } => (
            ToolCallOutcome::Failed,
            astrcode_core::tool::ToolResult::error(error.clone())
                .with_metadata(metadata.clone())
                .with_duration_ms(*duration_ms),
        ),
        ToolExecutionOutcome::Cancelled {
            reason,
            duration_ms,
        } => (
            ToolCallOutcome::Cancelled,
            astrcode_core::tool::ToolResult::error(format!("Tool cancelled: {reason}"))
                .with_duration_ms(*duration_ms),
        ),
    };
    let payload = DurableEventPayload::ToolCallSettled {
        call_id: call_id.into(),
        outcome,
        result,
    };
    publisher.durable(payload).await
}

/// release 下的兜底：`commit_tool_outcomes` 要求每个 declared call 必有 outcome，
/// 缺失即编排缺陷（调用方已用 debug_assert 暴露）。保留 failed 结果防止 release
/// 构建中调用悬挂。
pub(super) fn missing_tool_outcome(call: &PreparedToolInvocation) -> ToolExecutionOutcome {
    debug_assert!(
        false,
        "declared tool call `{}` reached commit without an outcome",
        call.call_id
    );
    ToolExecutionOutcome::failed(format!("Tool '{}' did not produce an outcome", call.name))
}
