//! `session_inspect` 插件边界契约。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;

use crate::{
    host::deserialize_non_empty_string,
    session::{SessionLifecycleStateDto, SessionPhaseDto, SessionToolSelectionDto},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostSessionInspectRequest {
    #[serde(deserialize_with = "deserialize_non_empty_session_id")]
    pub session_id: String,
}

fn deserialize_non_empty_session_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_non_empty_string(deserializer, "sessionId")
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
        #[serde(rename = "mediaType")]
        media_type: String,
        filename: Option<String>,
    },
    ToolCall {
        #[serde(rename = "callId")]
        call_id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        content: String,
        #[serde(rename = "isError")]
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

/// `astrcode.session.history.snapshot` 的作用域受限只读响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionHistorySnapshotOutput {
    pub lifecycle: SessionLifecycleStateDto,
    pub read_model: SessionInspectReadModel,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
        let request = json!({ "sessionId": "session-1" });
        assert_unknown_fields_rejected::<HostSessionInspectRequest>(&request, &[""]);
        assert!(serde_json::from_value::<HostSessionInspectRequest>(json!({})).is_err());
        assert!(
            serde_json::from_value::<HostSessionInspectRequest>(json!({ "sessionId": "" }))
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
