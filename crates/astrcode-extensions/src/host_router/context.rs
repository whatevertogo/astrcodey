//! Extension-scoped state and event capabilities.

use std::path::{Path, PathBuf};

use astrcode_extension_sdk::s5r::ErrorPayload;
use serde_json::{Value, json};

use super::{
    InvokeContext, capability::ContextCapability, emit_for_sink, run_blocking_io,
    run_blocking_io_to_completion,
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
    let base = ctx
        .session_store_dir
        .as_ref()
        .cloned()
        .ok_or_else(|| ErrorPayload::new("backend_unavailable", "session_store_dir missing"))?;
    let key = input["key"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| ErrorPayload::new("invalid_input", "key required"))?;
    let extension_id = ctx.extension_id.clone();
    let content = run_blocking_io(move || {
        let path = extension_state_dir(&base, &extension_id).join(safe_filename(&key));
        match std::fs::read_to_string(path) {
            Ok(content) => Ok(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(ErrorPayload::new("io_error", error.to_string())),
        }
    })
    .await?;
    Ok(json!({ "content": content }))
}

async fn write_state(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let base = ctx
        .session_store_dir
        .as_ref()
        .cloned()
        .ok_or_else(|| ErrorPayload::new("backend_unavailable", "session_store_dir missing"))?;
    let key = input["key"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| ErrorPayload::new("invalid_input", "key required"))?;
    let content = input["content"].as_str().unwrap_or("").to_owned();
    let extension_id = ctx.extension_id.clone();
    run_blocking_io_to_completion(ctx.tasks.as_ref(), "session-state-write", move || {
        let dir = extension_state_dir(&base, &extension_id);
        std::fs::create_dir_all(&dir)
            .map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
        let path = dir.join(safe_filename(&key));
        std::fs::write(path, content)
            .map_err(|error| ErrorPayload::new("io_error", error.to_string()))
    })
    .await?;
    Ok(json!({ "ok": true }))
}

fn emit_event(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let event_type = input["event_type"]
        .as_str()
        .ok_or_else(|| ErrorPayload::new("invalid_input", "event_type required"))?;
    let schema_version = input["schema_version"].as_u64().unwrap_or(1) as u32;
    let payload = input.get("payload").cloned().unwrap_or(Value::Null);
    let event_tx = ctx.event_tx.as_ref().ok_or_else(|| {
        ErrorPayload::new("backend_unavailable", "event_tx not configured in context")
    })?;
    emit_for_sink(
        &ctx.extension_id,
        &ctx.event_declarations,
        event_tx,
        event_type,
        schema_version,
        payload,
    )
    .map_err(|error| ErrorPayload::new("emit_failed", error.to_string()))?;
    Ok(json!({ "ok": true }))
}

fn extension_state_dir(session_store_dir: &Path, extension_id: &str) -> PathBuf {
    session_store_dir.join("extension_data").join(extension_id)
}

fn safe_filename(key: &str) -> String {
    key.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric()
                || character == '-'
                || character == '_'
                || character == '.'
            {
                character
            } else {
                '_'
            }
        })
        .collect()
}
