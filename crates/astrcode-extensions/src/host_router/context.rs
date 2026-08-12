//! Extension-scoped state and event capabilities.

use std::io::Read as _;

use astrcode_core::{
    config::defaults::extension_data_dir,
    event::{EventDeliveryReceipt, EventSendError},
};
use astrcode_extension_contract::WireErrorCode;
use astrcode_extension_sdk::{
    extension::ExtensionError,
    host::{
        Acknowledgement, HOST_SESSION_STATE_VALUE_MAX_BYTES, HostEventEmitOutput,
        HostEventEmitRequest, HostOperation, HostSessionStateReadOutput,
        HostSessionStateReadRequest, HostSessionStateWriteRequest, internal::HostOperationGroup,
    },
    s5r::ErrorPayload,
};
use serde_json::Value;

use super::{
    InvokeContext, acknowledgement, backend_unavailable, dispatch, emit_for_sink_confirmed,
    invalid_group_operation, io_error, run_blocking_io, run_blocking_io_to_completion,
};

#[derive(Default)]
pub(super) struct ContextGroup;

impl ContextGroup {
    pub(super) async fn invoke(
        &self,
        operation: HostOperation,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        match operation {
            HostOperation::SessionStateRead => {
                dispatch(operation, input, |request| read_state(request, ctx)).await
            },
            HostOperation::SessionStateWrite => {
                dispatch(operation, input, |request| write_state(request, ctx)).await
            },
            HostOperation::EventEmit => {
                dispatch(operation, input, |request| emit_event(request, ctx)).await
            },
            _ => Err(invalid_group_operation(
                operation,
                HostOperationGroup::Context,
            )),
        }
    }
}

async fn read_state(
    request: HostSessionStateReadRequest,
    ctx: &InvokeContext,
) -> Result<HostSessionStateReadOutput, ErrorPayload> {
    let key = request.key;
    let base = ctx
        .session_store_dir
        .as_ref()
        .cloned()
        .ok_or_else(|| backend_unavailable("session_store_dir missing"))?;
    let extension_id = ctx.extension_id.clone();
    let content = run_blocking_io(move || {
        let path = extension_data_dir(&base, &extension_id).join(key);
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(io_error(error));
            },
        };
        let mut bytes = Vec::new();
        file.take((HOST_SESSION_STATE_VALUE_MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(io_error)?;
        if bytes.len() > HOST_SESSION_STATE_VALUE_MAX_BYTES {
            return Err(ErrorPayload::new(
                WireErrorCode::StateTooLarge,
                format!("stored session state exceeds {HOST_SESSION_STATE_VALUE_MAX_BYTES} bytes"),
            ));
        }
        String::from_utf8(bytes).map(Some).map_err(io_error)
    })
    .await?;
    Ok(HostSessionStateReadOutput { content })
}

async fn write_state(
    request: HostSessionStateWriteRequest,
    ctx: &InvokeContext,
) -> Result<Acknowledgement, ErrorPayload> {
    let key = request.key;
    let base = ctx
        .session_store_dir
        .as_ref()
        .cloned()
        .ok_or_else(|| backend_unavailable("session_store_dir missing"))?;
    let content = request.content;
    let extension_id = ctx.extension_id.clone();
    run_blocking_io_to_completion(ctx.tasks.as_ref(), "session-state-write", move || {
        let dir = extension_data_dir(&base, &extension_id);
        std::fs::create_dir_all(&dir).map_err(io_error)?;
        let path = dir.join(key);
        std::fs::write(path, content).map_err(io_error)
    })
    .await?;
    Ok(acknowledgement())
}

async fn emit_event(
    request: HostEventEmitRequest,
    ctx: &InvokeContext,
) -> Result<HostEventEmitOutput, ErrorPayload> {
    let event_tx = ctx
        .event_tx
        .as_ref()
        .ok_or_else(|| backend_unavailable("event_tx not configured in context"))?;
    let receipt = emit_for_sink_confirmed(
        &ctx.extension_id,
        &ctx.event_declarations,
        event_tx,
        &request.event_type,
        request.schema_version,
        ctx.event_causation.clone(),
        request.payload,
    )
    .await
    .map_err(event_emit_error)?;
    Ok(match receipt {
        EventDeliveryReceipt::Accepted => HostEventEmitOutput::Accepted,
        EventDeliveryReceipt::LivePublished { event_id } => HostEventEmitOutput::LivePublished {
            event_id: event_id.into_string(),
        },
        EventDeliveryReceipt::Persisted { event_id, seq } => HostEventEmitOutput::Persisted {
            event_id: event_id.into_string(),
            seq,
        },
    })
}

fn event_emit_error(error: ExtensionError) -> ErrorPayload {
    match error {
        ExtensionError::EventSend(EventSendError::Full) => {
            ErrorPayload::new(WireErrorCode::PeerBusy, error.to_string()).retryable(true)
        },
        ExtensionError::EventSend(EventSendError::Closed) => {
            ErrorPayload::new(WireErrorCode::BackendUnavailable, error.to_string())
        },
        ExtensionError::EventSend(EventSendError::PublishFailed(_)) => {
            ErrorPayload::new(WireErrorCode::EmitFailed, error.to_string())
        },
        error => ErrorPayload::new(WireErrorCode::EmitFailed, error.to_string()),
    }
}
