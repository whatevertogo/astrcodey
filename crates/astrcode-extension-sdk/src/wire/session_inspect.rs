//! `session_inspect` 插件边界契约。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;

use crate::wire::{
    host::deserialize_non_empty_string,
    session::{
        SessionLifecycleStateDto, SessionMessageOriginDto, SessionPhaseDto, SessionToolSelectionDto,
    },
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionInspectRequest {
    #[serde(deserialize_with = "deserialize_non_empty_session_id")]
    pub session_id: String,
}

fn deserialize_non_empty_session_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_non_empty_string(deserializer, "session_id")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionInspectListItem {
    pub session_id: String,
    pub working_dir: String,
    pub model_id: String,
    pub parent_session_id: Option<String>,
    pub source_extension: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub phase: SessionPhaseDto,
    pub latest_cursor: String,
    pub first_user_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionInspectListOutput {
    pub sessions: Vec<SessionInspectListItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionInspectSnapshot {
    pub session_id: String,
    pub cursor: String,
    pub working_dir: String,
    pub model_id: String,
    pub phase: SessionPhaseDto,
    pub parent_session_id: Option<String>,
    pub source_extension: Option<String>,
    pub message_count: usize,
    pub pending_tool_call_ids: Vec<String>,
    pub agent_session_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionInspectSnapshotOutput {
    pub snapshot: SessionInspectSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionInspectMessage {
    pub role: String,
    pub content: Vec<SessionInspectContent>,
    pub name: Option<String>,
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionInspectContent {
    Text {
        text: String,
    },
    Image {
        base64: String,
        media_type: String,
        filename: Option<String>,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        tool_call_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionInspectSequencedMessage {
    pub message: SessionInspectMessage,
    pub updated_seq: u64,
    pub origin: Option<SessionMessageOriginDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionInspectPendingApproval {
    pub prompt: String,
    pub rule_key: Option<String>,
}

/// Child-agent lifecycle state at the session-inspect wire boundary.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionInspectAgentStatusDto {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionInspectAgentSession {
    pub child_session_id: String,
    pub tool_call_id: Option<String>,
    pub agent_name: String,
    pub task: String,
    pub status: SessionInspectAgentStatusDto,
    pub final_session_id: Option<String>,
    pub summary: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionInspectCompaction {
    pub trigger: String,
    pub pre_tokens: usize,
    pub post_tokens: usize,
    pub summary: String,
    pub transcript_path: Option<String>,
    pub seq: u64,
    pub source_seq: u64,
    pub strategy: String,
    pub keep_recent_turns: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionInspectReadModel {
    pub session_id: String,
    pub messages: Vec<SessionInspectSequencedMessage>,
    pub working_dir: String,
    pub model_id: String,
    pub phase: SessionPhaseDto,
    pub system_prompt: Option<String>,
    pub extra_system_prompt: Option<String>,
    pub system_prompt_fingerprint: Option<String>,
    pub pending_tool_call_ids: Vec<String>,
    pub pending_tool_approvals: BTreeMap<String, SessionInspectPendingApproval>,
    pub created_at: String,
    pub updated_at: String,
    pub parent_session_id: Option<String>,
    pub tool_selection: Option<SessionToolSelectionDto>,
    pub source_extension: Option<String>,
    pub agent_sessions: Vec<SessionInspectAgentSession>,
    pub compactions: Vec<SessionInspectCompaction>,
    pub latest_seq: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionInspectReadModelOutput {
    pub read_model: SessionInspectReadModel,
}

/// `astrcode.session.history.snapshot` 的作用域受限只读响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionHistorySnapshotOutput {
    pub lifecycle: SessionLifecycleStateDto,
    pub read_model: SessionInspectReadModel,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionInspectProviderMessagesOutput {
    pub messages: Vec<SessionInspectMessage>,
}

#[cfg(test)]
mod tests {
    use serde::de::DeserializeOwned;

    use super::*;

    fn assert_unknown_fields_rejected<T>(valid: &Value, pointers: &[&str])
    where
        T: DeserializeOwned,
    {
        serde_json::from_value::<T>(valid.clone()).unwrap();
        for pointer in pointers {
            let mut invalid = valid.clone();
            let object = if pointer.is_empty() {
                invalid.as_object_mut()
            } else {
                invalid.pointer_mut(pointer).and_then(Value::as_object_mut)
            }
            .unwrap_or_else(|| panic!("missing object at JSON pointer {pointer}"));
            object.insert("unexpected".into(), Value::Bool(true));
            assert!(
                serde_json::from_value::<T>(invalid).is_err(),
                "{} accepted an unknown field at {pointer}",
                std::any::type_name::<T>()
            );
        }
    }

    fn message_wire() -> Value {
        json!({
            "role": "assistant",
            "content": [
                { "type": "text", "text": "hello" },
                {
                    "type": "image",
                    "base64": "aW1hZ2U=",
                    "media_type": "image/png",
                    "filename": "image.png"
                },
                {
                    "type": "tool_call",
                    "call_id": "call-1",
                    "name": "read",
                    "arguments": { "path": "notes.txt" }
                },
                {
                    "type": "tool_result",
                    "tool_call_id": "call-1",
                    "content": "hello",
                    "is_error": false
                }
            ],
            "name": null,
            "reasoning_content": null
        })
    }

    fn read_model_wire() -> Value {
        json!({
            "session_id": "session-1",
            "messages": [{
                "message": message_wire(),
                "updated_seq": 1,
                "origin": null
            }],
            "working_dir": "/workspace",
            "model_id": "main",
            "phase": "idle",
            "system_prompt": null,
            "extra_system_prompt": null,
            "system_prompt_fingerprint": null,
            "pending_tool_call_ids": [],
            "pending_tool_approvals": {
                "call-1": { "prompt": "approve", "rule_key": null }
            },
            "created_at": "2026-08-06T00:00:00Z",
            "updated_at": "2026-08-06T00:00:01Z",
            "parent_session_id": null,
            "tool_selection": null,
            "source_extension": null,
            "agent_sessions": [{
                "child_session_id": "child-1",
                "tool_call_id": null,
                "agent_name": "reviewer",
                "task": "review",
                "status": "completed",
                "final_session_id": null,
                "summary": null,
                "error": null
            }],
            "compactions": [{
                "trigger": "manual",
                "pre_tokens": 100,
                "post_tokens": 20,
                "summary": "summary",
                "transcript_path": null,
                "seq": 2,
                "source_seq": 1,
                "strategy": "summary",
                "keep_recent_turns": null
            }],
            "latest_seq": 2
        })
    }

    #[test]
    fn session_inspect_wire_contracts_are_nested_strict() {
        let request = json!({ "session_id": "session-1" });
        assert_unknown_fields_rejected::<HostSessionInspectRequest>(&request, &[""]);
        assert!(serde_json::from_value::<HostSessionInspectRequest>(json!({})).is_err());
        assert!(
            serde_json::from_value::<HostSessionInspectRequest>(json!({ "session_id": "" }))
                .is_err()
        );

        let list = json!({
            "sessions": [{
                "session_id": "session-1",
                "working_dir": "/workspace",
                "model_id": "main",
                "parent_session_id": null,
                "source_extension": null,
                "created_at": "2026-08-06T00:00:00Z",
                "updated_at": "2026-08-06T00:00:01Z",
                "phase": "idle",
                "latest_cursor": "2",
                "first_user_message": "hello"
            }]
        });
        assert_unknown_fields_rejected::<SessionInspectListOutput>(&list, &["", "/sessions/0"]);

        let snapshot = json!({
            "snapshot": {
                "session_id": "session-1",
                "cursor": "2",
                "working_dir": "/workspace",
                "model_id": "main",
                "phase": "idle",
                "parent_session_id": null,
                "source_extension": null,
                "message_count": 1,
                "pending_tool_call_ids": [],
                "agent_session_count": 1
            }
        });
        assert_unknown_fields_rejected::<SessionInspectSnapshotOutput>(
            &snapshot,
            &["", "/snapshot"],
        );

        let read_model = json!({ "read_model": read_model_wire() });
        assert_unknown_fields_rejected::<SessionInspectReadModelOutput>(
            &read_model,
            &[
                "",
                "/read_model",
                "/read_model/messages/0",
                "/read_model/messages/0/message",
                "/read_model/messages/0/message/content/0",
                "/read_model/messages/0/message/content/1",
                "/read_model/messages/0/message/content/2",
                "/read_model/messages/0/message/content/3",
                "/read_model/pending_tool_approvals/call-1",
                "/read_model/agent_sessions/0",
                "/read_model/compactions/0",
            ],
        );
        let mut unknown_agent_status = read_model.clone();
        unknown_agent_status["read_model"]["agent_sessions"][0]["status"] = json!("paused");
        assert!(
            serde_json::from_value::<SessionInspectReadModelOutput>(unknown_agent_status).is_err()
        );
        for removed_field in ["phase", "currentTool"] {
            let mut stale_agent_shape = read_model.clone();
            stale_agent_shape["read_model"]["agent_sessions"][0][removed_field] = Value::Null;
            assert!(
                serde_json::from_value::<SessionInspectReadModelOutput>(stale_agent_shape).is_err(),
                "removed agent-session field {removed_field} was accepted"
            );
        }

        let history = json!({ "lifecycle": "active", "read_model": read_model_wire() });
        assert_unknown_fields_rejected::<SessionHistorySnapshotOutput>(&history, &[""]);

        let provider_messages = json!({ "messages": [message_wire()] });
        assert_unknown_fields_rejected::<SessionInspectProviderMessagesOutput>(
            &provider_messages,
            &[""],
        );
    }
}
