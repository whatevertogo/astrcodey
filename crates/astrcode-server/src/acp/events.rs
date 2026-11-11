//! Event mapping: astrcode `EventPayload` → ACP `SessionUpdate`.

use agent_client_protocol::v1::schema::{
    ContentBlock, ContentChunk, SessionNotification, SessionUpdate, TextContent, ToolCall,
    ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use astrcode_core::event::{
    AssistantDelta, DurableEventPayload, EventPayload, LiveEventPayload, ToolCallOutcome,
    ToolOutputStream,
};

/// Convert an astrcode `EventPayload` into an ACP `SessionNotification`
/// for the given session. Returns `None` if the event has no ACP equivalent.
pub(super) fn to_session_notification(
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
        EventPayload::Durable(payload) => durable_session_update(payload),
        EventPayload::Live(payload) => live_session_update(payload),
    }
}

fn durable_session_update(payload: &DurableEventPayload) -> Option<SessionUpdate> {
    match payload {
        DurableEventPayload::ToolCallRequested {
            call_id,
            tool_name,
            arguments,
            ..
        } => Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId::new(call_id.as_str()),
            ToolCallUpdateFields::new()
                .title(Some(tool_name.clone()))
                .status(Some(ToolCallStatus::Pending))
                .raw_input(Some(
                    astrcode_core::tool::parse_tool_arguments(arguments)
                        .unwrap_or_else(|_| serde_json::Value::String(arguments.clone())),
                )),
        ))),

        DurableEventPayload::ToolCallSettled {
            call_id,
            result,
            outcome,
            ..
        } => Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId::new(call_id.as_str()),
            completed_tool_fields(
                if *outcome == ToolCallOutcome::Completed {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::Failed
                },
                serde_json::json!({
                    "content": result.content,
                    "is_error": result.is_error,
                    "error": result.error,
                    "metadata": result.metadata,
                    "duration_ms": result.duration_ms,
                    ACP_TOOL_CALL_CANCELLED_MARKER: *outcome == ToolCallOutcome::Cancelled,
                }),
            ),
        ))),

        DurableEventPayload::ErrorOccurred { message, .. } => {
            Some(text_chunk(format!("[Error] {message}")))
        },

        _ => None,
    }
}

fn live_session_update(payload: &LiveEventPayload) -> Option<SessionUpdate> {
    match payload {
        LiveEventPayload::AssistantMessageDelta {
            delta: AssistantDelta::Text(delta),
            ..
        } => Some(text_chunk(delta.clone())),

        LiveEventPayload::AssistantMessageDelta {
            delta: AssistantDelta::Reasoning(delta),
            ..
        } => Some(thought_chunk(delta.clone())),

        LiveEventPayload::ToolCallPreviewStarted {
            call_id, tool_name, ..
        } => Some(SessionUpdate::ToolCall(
            ToolCall::new(ToolCallId::new(call_id.as_str()), tool_name.clone())
                .status(ToolCallStatus::Pending),
        )),

        LiveEventPayload::ToolExecutionStarted { call_id, .. } => {
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new(call_id.as_str()),
                ToolCallUpdateFields::new().status(Some(ToolCallStatus::InProgress)),
            )))
        },
        LiveEventPayload::ToolCallPreviewDiscarded { call_id } => {
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new(call_id.as_str()),
                completed_tool_fields(
                    ToolCallStatus::Failed,
                    serde_json::json!({"previewDiscarded": true}),
                ),
            )))
        },

        LiveEventPayload::ToolOutputDelta {
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

        LiveEventPayload::ErrorOccurred { message, .. } => {
            Some(text_chunk(format!("[Error] {message}")))
        },

        // Events that don't have a direct ACP equivalent are silently ignored.
        _ => None,
    }
}

/// ACP schema 的 `ToolCallStatus` 没有 Cancelled；取消以 `Failed` + raw_output
/// 中的此标记表达（生命周期之外的结果语义字段不受影响）。
const ACP_TOOL_CALL_CANCELLED_MARKER: &str = "cancelled";

fn completed_tool_fields(
    status: ToolCallStatus,
    raw_output: serde_json::Value,
) -> ToolCallUpdateFields {
    ToolCallUpdateFields::new()
        .status(Some(status))
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
    use std::collections::BTreeMap;

    use astrcode_core::{
        event::{EventPayload, ToolOutputStream},
        tool::ToolResult,
        types::ToolCallId as CoreToolCallId,
    };

    use super::*;

    #[test]
    fn maps_tool_output_delta_to_tool_update() {
        let update = to_session_update(&EventPayload::Live(LiveEventPayload::ToolOutputDelta {
            call_id: CoreToolCallId::from("call-1"),
            stream: ToolOutputStream::Stdout,
            delta: "hello".into(),
        }))
        .unwrap();

        let SessionUpdate::ToolCallUpdate(update) = update else {
            panic!("expected tool call update");
        };

        assert_eq!(update.tool_call_id, ToolCallId::new("call-1"));
        assert_eq!(update.fields.status, Some(ToolCallStatus::InProgress));
        assert!(update.fields.raw_output.is_some());
    }

    #[test]
    fn maps_tool_terminal_events_to_lifecycle_statuses() {
        let cases = [
            (
                DurableEventPayload::ToolCallSettled {
                    call_id: "call-ok".into(),
                    result: ToolResult::success("done"),
                    outcome: astrcode_core::event::ToolCallOutcome::Completed,
                },
                ToolCallStatus::Completed,
                false,
            ),
            // `is_error` 是结果语义，不影响生命周期状态。
            (
                DurableEventPayload::ToolCallSettled {
                    call_id: "call-domain-error".into(),
                    result: ToolResult::error("domain error"),
                    outcome: astrcode_core::event::ToolCallOutcome::Completed,
                },
                ToolCallStatus::Completed,
                false,
            ),
            (
                DurableEventPayload::ToolCallSettled {
                    call_id: "call-failed".into(),
                    outcome: astrcode_core::event::ToolCallOutcome::Failed,
                    result: astrcode_core::tool::ToolResult::error("executor failed")
                        .with_metadata(BTreeMap::new())
                        .with_duration_ms(Some(7)),
                },
                ToolCallStatus::Failed,
                false,
            ),
            // ACP schema 无 Cancelled：取消以 Failed + raw_output 标记表达。
            (
                DurableEventPayload::ToolCallSettled {
                    call_id: "call-cancelled".into(),
                    outcome: astrcode_core::event::ToolCallOutcome::Cancelled,
                    result: astrcode_core::tool::ToolResult::error(format!(
                        "Tool cancelled: {}",
                        "turn aborted".to_owned()
                    ))
                    .with_duration_ms(Some(8)),
                },
                ToolCallStatus::Failed,
                true,
            ),
        ];

        for (payload, expected_status, expected_cancelled_marker) in cases {
            let update = to_session_update(&EventPayload::Durable(payload)).unwrap();
            let SessionUpdate::ToolCallUpdate(update) = update else {
                panic!("expected tool call update");
            };
            assert_eq!(update.fields.status, Some(expected_status));
            assert_eq!(
                update
                    .fields
                    .raw_output
                    .as_ref()
                    .and_then(|value| value[ACP_TOOL_CALL_CANCELLED_MARKER].as_bool())
                    .unwrap_or(false),
                expected_cancelled_marker,
            );
        }
    }
}
