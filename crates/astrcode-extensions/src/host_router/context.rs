//! Extension-scoped state and event capabilities.

use std::io::Read as _;

use astrcode_core::{
    config::defaults::extension_data_dir, event::EventSendError, wire::WireErrorCode,
};
use astrcode_extension_sdk::{
    extension::ExtensionError,
    host::{
        HOST_SESSION_STATE_VALUE_MAX_BYTES, HostAcknowledgement, HostEventEmitOutput,
        HostEventEmitRequest, HostSessionStateReadOutput, HostSessionStateReadRequest,
        HostSessionStateWriteRequest,
    },
    s5r::ErrorPayload,
};
use serde_json::Value;

use super::{
    InvokeContext, backend_unavailable, capability::ContextCapability, emit_for_sink_confirmed,
    io_error, parse_wire_request, run_blocking_io, run_blocking_io_to_completion,
    serialize_wire_response,
};

#[derive(Default)]
pub(super) struct ContextGroup;

impl ContextGroup {
    pub(super) async fn invoke(
        &self,
        capability: ContextCapability,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        match capability {
            ContextCapability::StateRead => read_state(input, ctx).await,
            ContextCapability::StateWrite => write_state(input, ctx).await,
            ContextCapability::EmitEvent => emit_event(input, ctx).await,
        }
    }

    pub(super) fn is_available(&self, capability: ContextCapability, ctx: &InvokeContext) -> bool {
        match capability {
            ContextCapability::StateRead => ctx.session_store_dir.is_some(),
            ContextCapability::StateWrite => ctx.session_store_dir.is_some() && ctx.tasks.is_some(),
            ContextCapability::EmitEvent => ctx.event_tx.is_some(),
        }
    }
}

async fn read_state(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let request: HostSessionStateReadRequest = parse_wire_request(input, "session.state.read")?;
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
    serialize_wire_response(HostSessionStateReadOutput { content }, "session.state.read")
}

async fn write_state(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let request: HostSessionStateWriteRequest = parse_wire_request(input, "session.state.write")?;
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
    serialize_wire_response(HostAcknowledgement::accepted(), "session.state.write")
}

async fn emit_event(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let request: HostEventEmitRequest = parse_wire_request(input, "session.emit_event")?;
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
        request.payload,
    )
    .await
    .map_err(event_emit_error)?;
    serialize_wire_response(HostEventEmitOutput::from(receipt), "session.emit_event")
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
