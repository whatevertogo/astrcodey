//! Session-scoped persisted tool-result reads.

use astrcode_core::tool::ToolResultArtifactError;
use astrcode_extension_sdk::{
    host::{
        HostOperation, HostToolResultReadOutput, HostToolResultReadRequest,
        internal::HostOperationGroup,
    },
    wire::{ErrorPayload, WireErrorCode},
};
use serde_json::Value;

use super::{
    InvokeContext, backend_unavailable, invalid_group_operation, parse_wire_request,
    serialize_wire_response,
};

pub(super) async fn invoke(
    operation: HostOperation,
    input: Value,
    context: &InvokeContext,
) -> Result<Value, ErrorPayload> {
    if operation != HostOperation::ToolResultRead {
        return Err(invalid_group_operation(
            operation,
            HostOperationGroup::ToolResult,
        ));
    }
    let request: HostToolResultReadRequest = parse_wire_request(&input, operation.wire_name())?;
    let session_id = context.session_id.as_deref().ok_or_else(|| {
        ErrorPayload::new(
            WireErrorCode::ContextUnavailable,
            "tool-result reads require a session",
        )
    })?;
    let reader = context
        .tool_result_reader
        .as_ref()
        .ok_or_else(|| backend_unavailable("tool-result artifact reader is unavailable"))?;
    let slice = reader
        .read_tool_result_artifact(
            &astrcode_core::types::SessionId::new(session_id),
            &request.artifact_id,
            request.byte_offset,
            request.max_bytes,
        )
        .await
        .map_err(map_artifact_error)?;
    serialize_wire_response(
        HostToolResultReadOutput {
            artifact_id: slice.artifact_id,
            bytes: slice.bytes,
            byte_offset: slice.byte_offset,
            returned_bytes: slice.returned_bytes,
            next_byte_offset: slice.next_byte_offset,
            has_more: slice.has_more,
            content: slice.content,
        },
        operation.wire_name(),
    )
}

fn map_artifact_error(error: ToolResultArtifactError) -> ErrorPayload {
    let code = match error {
        ToolResultArtifactError::InvalidId(_) | ToolResultArtifactError::InvalidRequest(_) => {
            WireErrorCode::InvalidInput
        },
        ToolResultArtifactError::NotFound(_) => WireErrorCode::ReadFailed,
        ToolResultArtifactError::Unsupported(_) => WireErrorCode::Unsupported,
        ToolResultArtifactError::Read(_) => WireErrorCode::ReadFailed,
    };
    ErrorPayload::new(code, error.to_string())
}
