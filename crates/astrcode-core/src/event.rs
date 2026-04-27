//! Unified runtime and durable event types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{tool::ToolResult, types::*};

/// Projected execution phase for a session.
///
/// This is derived from the event stream by reducers. It is intentionally not
/// an event-log source of truth because tool concurrency needs reducer state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    Thinking,
    Streaming,
    CallingTool,
    Compacting,
    Error,
}

/// A stream inside a running tool call.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputStream {
    Stdout,
    Stderr,
}

/// The payload of a unified astrcode event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventPayload {
    SessionStarted {
        working_dir: String,
        model_id: String,
    },
    SessionDeleted,

    AgentRunStarted,
    AgentRunCompleted {
        reason: String,
    },

    TurnStarted,
    TurnCompleted {
        finish_reason: String,
    },

    UserMessage {
        message_id: MessageId,
        text: String,
    },

    AssistantMessageStarted {
        message_id: MessageId,
    },
    AssistantTextDelta {
        message_id: MessageId,
        delta: String,
    },
    AssistantMessageCompleted {
        message_id: MessageId,
        text: String,
    },

    ThinkingDelta {
        delta: String,
    },

    ToolCallStarted {
        call_id: ToolCallId,
        tool_name: String,
    },
    ToolCallArgumentsDelta {
        call_id: ToolCallId,
        delta: String,
    },
    ToolCallRequested {
        call_id: ToolCallId,
        tool_name: String,
        arguments: serde_json::Value,
    },
    ToolOutputDelta {
        call_id: ToolCallId,
        stream: ToolOutputStream,
        delta: String,
    },
    ToolCallCompleted {
        call_id: ToolCallId,
        tool_name: String,
        result: ToolResult,
    },

    CompactionStarted,
    CompactionCompleted {
        pre_tokens: usize,
        post_tokens: usize,
        summary: String,
    },

    ErrorOccurred {
        code: i32,
        message: String,
        recoverable: bool,
    },
    Custom {
        name: String,
        data: serde_json::Value,
    },
}

impl EventPayload {
    /// Whether this event should be persisted in the session JSONL log.
    pub fn is_durable(&self) -> bool {
        !matches!(
            self,
            Self::AssistantTextDelta { .. }
                | Self::ThinkingDelta { .. }
                | Self::ToolCallArgumentsDelta { .. }
                | Self::ToolOutputDelta { .. }
                | Self::CompactionStarted
                | Self::AgentRunStarted
                | Self::AgentRunCompleted { .. }
        )
    }
}

/// Event envelope carrying session/turn identity and storage sequencing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    pub id: EventId,
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    pub timestamp: DateTime<Utc>,
    #[serde(flatten)]
    pub payload: EventPayload,
}

impl Event {
    pub fn new(session_id: SessionId, turn_id: Option<TurnId>, payload: EventPayload) -> Self {
        Self {
            seq: None,
            id: new_event_id(),
            session_id,
            turn_id,
            timestamp: Utc::now(),
            payload,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn event_serializes_as_flat_json() {
        let event = Event {
            seq: Some(0),
            id: "event-1".into(),
            session_id: "session-1".into(),
            turn_id: Some("turn-1".into()),
            timestamp: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            payload: EventPayload::UserMessage {
                message_id: "message-1".into(),
                text: "hello".into(),
            },
        };

        let json = serde_json::to_value(event).unwrap();
        assert_eq!(json["seq"], 0);
        assert_eq!(json["type"], "user_message");
        assert_eq!(json["session_id"], "session-1");
        assert_eq!(json["message_id"], "message-1");
        assert!(json.get("payload").is_none());
    }

    #[test]
    fn durable_classification_matches_event_log_policy() {
        assert!(
            !EventPayload::AssistantTextDelta {
                message_id: "m1".into(),
                delta: "hi".into(),
            }
            .is_durable()
        );
        assert!(
            !EventPayload::ToolCallArgumentsDelta {
                call_id: "c1".into(),
                delta: "{}".into(),
            }
            .is_durable()
        );
        assert!(
            EventPayload::ToolCallRequested {
                call_id: "c1".into(),
                tool_name: "shell".into(),
                arguments: serde_json::json!({"cmd": "pwd"}),
            }
            .is_durable()
        );
        assert!(
            EventPayload::ToolCallCompleted {
                call_id: "c1".into(),
                tool_name: "shell".into(),
                result: ToolResult {
                    call_id: "c1".into(),
                    content: "ok".into(),
                    is_error: false,
                    error: None,
                    metadata: BTreeMap::new(),
                    duration_ms: Some(10),
                },
            }
            .is_durable()
        );
    }

    #[test]
    fn tool_call_start_and_request_have_separate_meaning() {
        let start = EventPayload::ToolCallStarted {
            call_id: "c1".into(),
            tool_name: "shell".into(),
        };
        let request = EventPayload::ToolCallRequested {
            call_id: "c1".into(),
            tool_name: "shell".into(),
            arguments: serde_json::json!({"cmd": "pwd"}),
        };

        assert!(start.is_durable());
        assert!(request.is_durable());
        assert_ne!(start, request);
    }
}
