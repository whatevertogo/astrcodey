//! Event mapping: astrcode `EventPayload` → ACP `SessionUpdate`.

use agent_client_protocol::schema::{
    ContentBlock, ContentChunk, SessionNotification, SessionUpdate, TextContent, ToolCall,
    ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use astrcode_core::event::{EventPayload, ToolOutputStream};

/// Convert an astrcode `EventPayload` into an ACP `SessionNotification`
/// for the given session. Returns `None` if the event has no ACP equivalent.
pub fn to_session_notification(
    session_id: &str,
    payload: &EventPayload,
) -> Option<SessionNotification> {
    let update = to_session_update(payload)?;
    Some(SessionNotification::new(session_id.to_string(), update))
}

fn text_chunk(delta: String) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
        delta,
    ))))
}

fn thought_chunk(delta: String) -> SessionUpdate {
    SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
        delta,
    ))))
}

fn to_session_update(payload: &EventPayload) -> Option<SessionUpdate> {
    match payload {
        EventPayload::AssistantTextDelta { delta, .. } => Some(text_chunk(delta.clone())),

        EventPayload::ThinkingDelta { delta, .. } => Some(thought_chunk(delta.clone())),

        EventPayload::ToolCallStarted { call_id, tool_name } => Some(SessionUpdate::ToolCall(
            ToolCall::new(ToolCallId::new(call_id.as_str()), tool_name.clone())
                .status(ToolCallStatus::InProgress),
        )),

        EventPayload::ToolCallRequested {
            call_id,
            tool_name,
            arguments,
        } => Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId::new(call_id.as_str()),
            ToolCallUpdateFields::new()
                .title(Some(tool_name.clone()))
                .status(Some(ToolCallStatus::InProgress))
                .raw_input(Some(arguments.clone())),
        ))),

        EventPayload::ToolCallCompleted {
            call_id, result, ..
        } => Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId::new(call_id.as_str()),
            completed_tool_fields(
                result.is_error,
                serde_json::json!({
                    "content": result.content,
                    "is_error": result.is_error,
                    "error": result.error,
                    "metadata": result.metadata,
                    "duration_ms": result.duration_ms,
                }),
            ),
        ))),

        EventPayload::ToolOutputDelta {
            call_id,
            stream,
            delta,
        } => Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId::new(call_id.as_str()),
            ToolCallUpdateFields::new()
                .status(Some(ToolCallStatus::InProgress))
                .content(Some(vec![ToolCallContent::from(format!(
                    "{}: {delta}",
                    stream_name(*stream)
                ))]))
                .raw_output(Some(serde_json::json!({
                    "stream": stream_name(*stream),
                    "delta": delta,
                }))),
        ))),

        EventPayload::ErrorOccurred { message, .. } => {
            Some(text_chunk(format!("[Error] {message}")))
        },

        // Events that don't have a direct ACP equivalent are silently ignored.
        _ => None,
    }
}

fn completed_tool_fields(is_error: bool, raw_output: serde_json::Value) -> ToolCallUpdateFields {
    ToolCallUpdateFields::new()
        .status(Some(if is_error {
            ToolCallStatus::Failed
        } else {
            ToolCallStatus::Completed
        }))
        .raw_output(Some(raw_output))
}

fn stream_name(stream: ToolOutputStream) -> &'static str {
    match stream {
        ToolOutputStream::Stdout => "stdout",
        ToolOutputStream::Stderr => "stderr",
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        event::{EventPayload, ToolOutputStream},
        types::ToolCallId as CoreToolCallId,
    };

    use super::*;

    #[test]
    fn maps_tool_output_delta_to_tool_update() {
        let update = to_session_update(&EventPayload::ToolOutputDelta {
            call_id: CoreToolCallId::from("call-1"),
            stream: ToolOutputStream::Stdout,
            delta: "hello".into(),
        })
        .unwrap();

        let SessionUpdate::ToolCallUpdate(update) = update else {
            panic!("expected tool call update");
        };

        assert_eq!(update.tool_call_id, ToolCallId::new("call-1"));
        assert_eq!(update.fields.status, Some(ToolCallStatus::InProgress));
        assert!(update.fields.raw_output.is_some());
    }
}
