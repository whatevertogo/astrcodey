//! Extension-scoped state and event capabilities.

use astrcode_extension_sdk::{s5r::ErrorPayload, state};
use serde_json::{Value, json};

use super::{InvokeContext, capability::ContextCapability, emit_for_sink};

#[derive(Default)]
pub(super) struct ContextGroup;

impl ContextGroup {
    pub(super) fn invoke(
        &self,
        capability: ContextCapability,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        match capability {
            ContextCapability::StateRead => read_state(input, ctx),
            ContextCapability::StateWrite => write_state(input, ctx),
            ContextCapability::EmitEvent => emit_event(input, ctx),
        }
    }
}

fn read_state(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let base = ctx
        .session_store_dir
        .as_ref()
        .ok_or_else(|| ErrorPayload::new("backend_unavailable", "session_store_dir missing"))?;
    let key = input["key"]
        .as_str()
        .ok_or_else(|| ErrorPayload::new("invalid_input", "key required"))?;
    let path = state::session_data_dir(base, &ctx.extension_id).join(safe_filename(key));
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(ErrorPayload::new("io_error", error.to_string())),
    };
    Ok(json!({ "content": content }))
}

fn write_state(input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
    let base = ctx
        .session_store_dir
        .as_ref()
        .ok_or_else(|| ErrorPayload::new("backend_unavailable", "session_store_dir missing"))?;
    let key = input["key"]
        .as_str()
        .ok_or_else(|| ErrorPayload::new("invalid_input", "key required"))?;
    let content = input["content"].as_str().unwrap_or("");
    let dir = state::session_data_dir(base, &ctx.extension_id);
    std::fs::create_dir_all(&dir)
        .map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
    let path = dir.join(safe_filename(key));
    std::fs::write(&path, content)
        .map_err(|error| ErrorPayload::new("io_error", error.to_string()))?;
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
