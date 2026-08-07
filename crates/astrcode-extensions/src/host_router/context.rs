//! Extension-scoped state and event capabilities.

use std::{
    io::Read as _,
    path::{Path, PathBuf},
};

use astrcode_extension_sdk::{
    host::{
        HOST_ERROR_CODE_BACKEND_UNAVAILABLE, HOST_ERROR_CODE_SERIALIZATION_FAILED,
        HOST_SESSION_STATE_VALUE_MAX_BYTES, HostAcknowledgement, HostEventEmitRequest,
        HostSessionStateReadOutput, HostSessionStateReadRequest, HostSessionStateWriteRequest,
    },
    s5r::ErrorPayload,
};
use serde_json::Value;

use super::{
    InvokeContext, capability::ContextCapability, decode_host_input, emit_for_sink,
    run_blocking_io, run_blocking_io_to_completion,
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
            ContextCapability::EmitEvent => emit_event(input, ctx),
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
    let request: HostSessionStateReadRequest = decode_host_input(input.clone())?;
    let key = request.key;
    let base = ctx.session_store_dir.as_ref().cloned().ok_or_else(|| {
        ErrorPayload::new(
            HOST_ERROR_CODE_BACKEND_UNAVAILABLE,
            "session_store_dir missing",
        )
    })?;
    let extension_id = ctx.extension_id.clone();
    let content = run_blocking_io(move || {
        let path = extension_state_dir(&base, &extension_id).join(key);
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(ErrorPayload::new("io_error", error.to_string())),
        };
        let mut bytes = Vec::new();
        file.take((HOST_SESSION_STATE_VALUE_MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
        if bytes.len() > HOST_SESSION_STATE_VALUE_MAX_BYTES {
            return Err(ErrorPayload::new(
                "state_too_large",
                format!("stored session state exceeds {HOST_SESSION_STATE_VALUE_MAX_BYTES} bytes"),
            ));
        }
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|error| ErrorPayload::new("io_error", error.to_string()))
    })
    .await?;
    serde_json::to_value(HostSessionStateReadOutput { content }).map_err(|error| {
        ErrorPayload::new(
            HOST_ERROR_CODE_SERIALIZATION_FAILED,
            format!("serialize session state response: {error}"),
        )
    })
}

async fn write_state(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let request: HostSessionStateWriteRequest = decode_host_input(input.clone())?;
    let key = request.key;
    let base = ctx.session_store_dir.as_ref().cloned().ok_or_else(|| {
        ErrorPayload::new(
            HOST_ERROR_CODE_BACKEND_UNAVAILABLE,
            "session_store_dir missing",
        )
    })?;
    let content = request.content;
    let extension_id = ctx.extension_id.clone();
    run_blocking_io_to_completion(ctx.tasks.as_ref(), "session-state-write", move || {
        let dir = extension_state_dir(&base, &extension_id);
        std::fs::create_dir_all(&dir)
            .map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
        let path = dir.join(key);
        std::fs::write(path, content)
            .map_err(|error| ErrorPayload::new("io_error", error.to_string()))
    })
    .await?;
    acknowledgement_response()
}

fn emit_event(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let request: HostEventEmitRequest = decode_host_input(input.clone())?;
    let event_tx = ctx.event_tx.as_ref().ok_or_else(|| {
        ErrorPayload::new(
            HOST_ERROR_CODE_BACKEND_UNAVAILABLE,
            "event_tx not configured in context",
        )
    })?;
    emit_for_sink(
        &ctx.extension_id,
        &ctx.event_declarations,
        event_tx,
        &request.event_type,
        request.schema_version,
        request.payload,
    )
    .map_err(|error| ErrorPayload::new("emit_failed", error.to_string()))?;
    acknowledgement_response()
}

fn acknowledgement_response() -> Result<Value, ErrorPayload> {
    serde_json::to_value(HostAcknowledgement::accepted()).map_err(|error| {
        ErrorPayload::new(
            HOST_ERROR_CODE_SERIALIZATION_FAILED,
            format!("serialize host acknowledgement: {error}"),
        )
    })
}

fn extension_state_dir(session_store_dir: &Path, extension_id: &str) -> PathBuf {
    session_store_dir.join("extension_data").join(extension_id)
}
