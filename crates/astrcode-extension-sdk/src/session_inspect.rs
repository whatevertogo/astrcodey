//! `session_inspect` 插件边界契约。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::session::{SessionLifecycleStateDto, SessionPhaseDto, SessionToolSelectionDto};

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
    let session_id = String::deserialize(deserializer)?;
    if session_id.is_empty() {
        Err(serde::de::Error::custom("session_id must not be empty"))
    } else {
        Ok(session_id)
    }
}

impl HostSessionInspectRequest {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "minLength": 1 }
            },
            "required": ["session_id"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionInspectListOutput {
    pub sessions: Vec<SessionInspectListItem>,
}

impl SessionInspectListOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "sessions": { "type": "array", "items": session_inspect_list_item_schema() }
            },
            "required": ["sessions"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionInspectSnapshotOutput {
    pub snapshot: SessionInspectSnapshot,
}

impl SessionInspectSnapshotOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": { "snapshot": session_inspect_snapshot_schema() },
            "required": ["snapshot"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionInspectMessage {
    pub role: String,
    pub content: Vec<SessionInspectContent>,
    pub name: Option<String>,
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionInspectSequencedMessage {
    pub message: SessionInspectMessage,
    pub updated_seq: u64,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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

impl SessionInspectAgentStatusDto {
    fn wire_schema() -> Value {
        json!({
            "type": "string",
            "enum": ["running", "completed", "failed"]
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionInspectReadModelOutput {
    pub read_model: SessionInspectReadModel,
}

impl SessionInspectReadModelOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": { "readModel": session_inspect_read_model_schema() },
            "required": ["readModel"],
            "additionalProperties": false
        })
    }
}

/// `astrcode.session.history.snapshot` 的作用域受限只读响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionHistorySnapshotOutput {
    pub lifecycle: SessionLifecycleStateDto,
    pub read_model: SessionInspectReadModel,
}

impl SessionHistorySnapshotOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "lifecycle": { "type": "string", "enum": ["active", "recycled"] },
                "readModel": session_inspect_read_model_schema()
            },
            "required": ["lifecycle", "readModel"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionInspectProviderMessagesOutput {
    pub messages: Vec<SessionInspectMessage>,
}

impl SessionInspectProviderMessagesOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "messages": { "type": "array", "items": session_inspect_message_schema() }
            },
            "required": ["messages"],
            "additionalProperties": false
        })
    }
}

fn session_inspect_list_item_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "sessionId": { "type": "string" },
            "workingDir": { "type": "string" },
            "modelId": { "type": "string" },
            "parentSessionId": { "type": ["string", "null"] },
            "sourceExtension": { "type": ["string", "null"] },
            "createdAt": { "type": "string" },
            "updatedAt": { "type": "string" },
            "phase": SessionPhaseDto::wire_schema(),
            "latestCursor": { "type": "string" },
            "firstUserMessage": { "type": ["string", "null"] }
        },
        "required": [
            "sessionId",
            "workingDir",
            "modelId",
            "parentSessionId",
            "sourceExtension",
            "createdAt",
            "updatedAt",
            "phase",
            "latestCursor",
            "firstUserMessage"
        ],
        "additionalProperties": false
    })
}

fn session_inspect_snapshot_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "sessionId": { "type": "string" },
            "cursor": { "type": "string" },
            "workingDir": { "type": "string" },
            "modelId": { "type": "string" },
            "phase": SessionPhaseDto::wire_schema(),
            "parentSessionId": { "type": ["string", "null"] },
            "sourceExtension": { "type": ["string", "null"] },
            "messageCount": { "type": "integer", "minimum": 0 },
            "pendingToolCallIds": { "type": "array", "items": { "type": "string" } },
            "agentSessionCount": { "type": "integer", "minimum": 0 }
        },
        "required": [
            "sessionId",
            "cursor",
            "workingDir",
            "modelId",
            "phase",
            "parentSessionId",
            "sourceExtension",
            "messageCount",
            "pendingToolCallIds",
            "agentSessionCount"
        ],
        "additionalProperties": false
    })
}

fn session_inspect_read_model_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "sessionId": { "type": "string" },
            "messages": { "type": "array", "items": session_inspect_sequenced_message_schema() },
            "workingDir": { "type": "string" },
            "modelId": { "type": "string" },
            "phase": SessionPhaseDto::wire_schema(),
            "systemPrompt": { "type": ["string", "null"] },
            "extraSystemPrompt": { "type": ["string", "null"] },
            "systemPromptFingerprint": { "type": ["string", "null"] },
            "pendingToolCallIds": { "type": "array", "items": { "type": "string" } },
            "pendingToolApprovals": {
                "type": "object",
                "additionalProperties": session_inspect_pending_approval_schema()
            },
            "createdAt": { "type": "string" },
            "updatedAt": { "type": "string" },
            "parentSessionId": { "type": ["string", "null"] },
            "toolSelection": {
                "anyOf": [
                    SessionToolSelectionDto::wire_schema("Effective session tool visibility."),
                    { "type": "null" }
                ]
            },
            "sourceExtension": { "type": ["string", "null"] },
            "agentSessions": { "type": "array", "items": session_inspect_agent_session_schema() },
            "compactions": { "type": "array", "items": session_inspect_compaction_schema() },
            "latestSeq": { "type": ["integer", "null"], "minimum": 0 }
        },
        "required": [
            "sessionId",
            "messages",
            "workingDir",
            "modelId",
            "phase",
            "systemPrompt",
            "extraSystemPrompt",
            "systemPromptFingerprint",
            "pendingToolCallIds",
            "pendingToolApprovals",
            "createdAt",
            "updatedAt",
            "parentSessionId",
            "toolSelection",
            "sourceExtension",
            "agentSessions",
            "compactions",
            "latestSeq"
        ],
        "additionalProperties": false
    })
}

fn session_inspect_sequenced_message_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "message": session_inspect_message_schema(),
            "updatedSeq": { "type": "integer", "minimum": 0 },
            "source": { "type": ["string", "null"] }
        },
        "required": ["message", "updatedSeq", "source"],
        "additionalProperties": false
    })
}

fn session_inspect_message_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "role": { "type": "string" },
            "content": { "type": "array", "items": session_inspect_content_schema() },
            "name": { "type": ["string", "null"] },
            "reasoningContent": { "type": ["string", "null"] }
        },
        "required": ["role", "content", "name", "reasoningContent"],
        "additionalProperties": false
    })
}

fn session_inspect_content_schema() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "type": { "const": "text" },
                    "text": { "type": "string" }
                },
                "required": ["type", "text"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "type": { "const": "image" },
                    "base64": { "type": "string" },
                    "mediaType": { "type": "string" },
                    "filename": { "type": ["string", "null"] }
                },
                "required": ["type", "base64", "mediaType", "filename"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "type": { "const": "tool_call" },
                    "callId": { "type": "string" },
                    "name": { "type": "string" },
                    "arguments": {}
                },
                "required": ["type", "callId", "name", "arguments"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "type": { "const": "tool_result" },
                    "toolCallId": { "type": "string" },
                    "content": { "type": "string" },
                    "isError": { "type": "boolean" }
                },
                "required": ["type", "toolCallId", "content", "isError"],
                "additionalProperties": false
            }
        ]
    })
}

fn session_inspect_pending_approval_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "prompt": { "type": "string" },
            "ruleKey": { "type": ["string", "null"] }
        },
        "required": ["prompt", "ruleKey"],
        "additionalProperties": false
    })
}

fn session_inspect_agent_session_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "childSessionId": { "type": "string" },
            "toolCallId": { "type": ["string", "null"] },
            "agentName": { "type": "string" },
            "task": { "type": "string" },
            "status": SessionInspectAgentStatusDto::wire_schema(),
            "finalSessionId": { "type": ["string", "null"] },
            "summary": { "type": ["string", "null"] },
            "error": { "type": ["string", "null"] }
        },
        "required": [
            "childSessionId",
            "toolCallId",
            "agentName",
            "task",
            "status",
            "finalSessionId",
            "summary",
            "error"
        ],
        "additionalProperties": false
    })
}

fn session_inspect_compaction_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "trigger": { "type": "string" },
            "preTokens": { "type": "integer", "minimum": 0 },
            "postTokens": { "type": "integer", "minimum": 0 },
            "summary": { "type": "string" },
            "transcriptPath": { "type": ["string", "null"] },
            "seq": { "type": "integer", "minimum": 0 },
            "sourceSeq": { "type": "integer", "minimum": 0 },
            "strategy": { "type": "string" },
            "keepRecentTurns": { "type": ["integer", "null"], "minimum": 0 }
        },
        "required": [
            "trigger",
            "preTokens",
            "postTokens",
            "summary",
            "transcriptPath",
            "seq",
            "sourceSeq",
            "strategy",
            "keepRecentTurns"
        ],
        "additionalProperties": false
    })
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
                    "mediaType": "image/png",
                    "filename": "image.png"
                },
                {
                    "type": "tool_call",
                    "callId": "call-1",
                    "name": "read",
                    "arguments": { "path": "notes.txt" }
                },
                {
                    "type": "tool_result",
                    "toolCallId": "call-1",
                    "content": "hello",
                    "isError": false
                }
            ],
            "name": null,
            "reasoningContent": null
        })
    }

    fn read_model_wire() -> Value {
        json!({
            "sessionId": "session-1",
            "messages": [{
                "message": message_wire(),
                "updatedSeq": 1,
                "source": null
            }],
            "workingDir": "/workspace",
            "modelId": "main",
            "phase": "idle",
            "systemPrompt": null,
            "extraSystemPrompt": null,
            "systemPromptFingerprint": null,
            "pendingToolCallIds": [],
            "pendingToolApprovals": {
                "call-1": { "prompt": "approve", "ruleKey": null }
            },
            "createdAt": "2026-08-06T00:00:00Z",
            "updatedAt": "2026-08-06T00:00:01Z",
            "parentSessionId": null,
            "toolSelection": null,
            "sourceExtension": null,
            "agentSessions": [{
                "childSessionId": "child-1",
                "toolCallId": null,
                "agentName": "reviewer",
                "task": "review",
                "status": "completed",
                "finalSessionId": null,
                "summary": null,
                "error": null
            }],
            "compactions": [{
                "trigger": "manual",
                "preTokens": 100,
                "postTokens": 20,
                "summary": "summary",
                "transcriptPath": null,
                "seq": 2,
                "sourceSeq": 1,
                "strategy": "summary",
                "keepRecentTurns": null
            }],
            "latestSeq": 2
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
                "sessionId": "session-1",
                "workingDir": "/workspace",
                "modelId": "main",
                "parentSessionId": null,
                "sourceExtension": null,
                "createdAt": "2026-08-06T00:00:00Z",
                "updatedAt": "2026-08-06T00:00:01Z",
                "phase": "idle",
                "latestCursor": "2",
                "firstUserMessage": "hello"
            }]
        });
        assert_unknown_fields_rejected::<SessionInspectListOutput>(&list, &["", "/sessions/0"]);

        let snapshot = json!({
            "snapshot": {
                "sessionId": "session-1",
                "cursor": "2",
                "workingDir": "/workspace",
                "modelId": "main",
                "phase": "idle",
                "parentSessionId": null,
                "sourceExtension": null,
                "messageCount": 1,
                "pendingToolCallIds": [],
                "agentSessionCount": 1
            }
        });
        assert_unknown_fields_rejected::<SessionInspectSnapshotOutput>(
            &snapshot,
            &["", "/snapshot"],
        );

        let read_model = json!({ "readModel": read_model_wire() });
        assert_unknown_fields_rejected::<SessionInspectReadModelOutput>(
            &read_model,
            &[
                "",
                "/readModel",
                "/readModel/messages/0",
                "/readModel/messages/0/message",
                "/readModel/messages/0/message/content/0",
                "/readModel/messages/0/message/content/1",
                "/readModel/messages/0/message/content/2",
                "/readModel/messages/0/message/content/3",
                "/readModel/pendingToolApprovals/call-1",
                "/readModel/agentSessions/0",
                "/readModel/compactions/0",
            ],
        );
        let mut unknown_agent_status = read_model.clone();
        unknown_agent_status["readModel"]["agentSessions"][0]["status"] = json!("paused");
        assert!(
            serde_json::from_value::<SessionInspectReadModelOutput>(unknown_agent_status).is_err()
        );
        for removed_field in ["phase", "currentTool"] {
            let mut stale_agent_shape = read_model.clone();
            stale_agent_shape["readModel"]["agentSessions"][0][removed_field] = Value::Null;
            assert!(
                serde_json::from_value::<SessionInspectReadModelOutput>(stale_agent_shape).is_err(),
                "removed agent-session field {removed_field} was accepted"
            );
        }

        let history = json!({ "lifecycle": "active", "readModel": read_model_wire() });
        assert_unknown_fields_rejected::<SessionHistorySnapshotOutput>(&history, &[""]);

        let provider_messages = json!({ "messages": [message_wire()] });
        assert_unknown_fields_rejected::<SessionInspectProviderMessagesOutput>(
            &provider_messages,
            &[""],
        );
    }
}
