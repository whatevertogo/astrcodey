//! 会话原子操作 API re-export。
//!
//! `SessionOperations` trait 定义在 `astrcode-core/src/tool.rs`，此处 re-export
//! 方便插件侧使用。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use crate::{
    extension::SessionToolSelection,
    tool::{
        CreateSessionRequest, SessionAccess, SessionAccessPair, SessionApiError, SessionHandle,
        SessionLifecycleState, SessionOperations, SessionReactivation, SessionState, SessionStatus,
        SubmitTurnRequest, SubmitTurnResult,
    },
};

/// 插件 session API 共用的工具选择线缆契约。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionToolSelectionDto {
    All {
        #[serde(default)]
        except: Vec<String>,
    },
    Only {
        #[serde(default)]
        names: Vec<String>,
    },
}

impl SessionToolSelectionDto {
    /// 使用全部工具。
    pub const fn all() -> Self {
        Self::All { except: Vec::new() }
    }

    /// 使用全部工具，但排除指定名称。
    pub fn all_except<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::All {
            except: names.into_iter().map(Into::into).collect(),
        }
    }

    /// 仅使用指定名称。
    pub fn only<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Only {
            names: names.into_iter().map(Into::into).collect(),
        }
    }

    /// 明确禁用全部工具。
    pub const fn no_tools() -> Self {
        Self::Only { names: Vec::new() }
    }

    /// 返回该线缆类型的 JSON Schema。
    pub fn wire_schema(description: &str) -> Value {
        json!({
            "type": "object",
            "description": description,
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "mode": { "const": "all" },
                        "except": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["mode"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "mode": { "const": "only" },
                        "names": { "type": "array", "items": { "type": "string" } }
                    },
                    "required": ["mode"],
                    "additionalProperties": false
                }
            ]
        })
    }
}

impl From<SessionToolSelection> for SessionToolSelectionDto {
    fn from(selection: SessionToolSelection) -> Self {
        match selection {
            SessionToolSelection::All { except } => Self::All { except },
            SessionToolSelection::Only { names } => Self::Only { names },
        }
    }
}

/// `astrcode.session.control.create` 的线缆请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostCreateSessionRequest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_preference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_selection: Option<SessionToolSelectionDto>,
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl HostCreateSessionRequest {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            working_dir: None,
            system_prompt: None,
            model_preference: None,
            tool_selection: None,
            ephemeral: false,
            tool_call_id: None,
        }
    }

    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "working_dir": { "type": "string" },
                "system_prompt": { "type": "string" },
                "model_preference": { "type": "string" },
                "ephemeral": { "type": "boolean" },
                "tool_call_id": { "type": "string" },
                "tool_selection": SessionToolSelectionDto::wire_schema(
                    "Child session tool visibility for subsequent turns."
                )
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }
}

/// `astrcode.session.control.create` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostCreateSessionOutput {
    pub session_id: String,
}

impl HostCreateSessionOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string" }
            },
            "required": ["session_id"],
            "additionalProperties": false
        })
    }
}

impl From<SessionHandle> for HostCreateSessionOutput {
    fn from(handle: SessionHandle) -> Self {
        Self {
            session_id: handle.session_id,
        }
    }
}

/// 插件 session control 操作的目标。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionTargetRequest {
    pub target_session_id: String,
}

impl HostSessionTargetRequest {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "target_session_id": { "type": "string" }
            },
            "required": ["target_session_id"],
            "additionalProperties": false
        })
    }
}

/// `astrcode.session.control.dispose` request. The operation recycles rather than deletes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostRecycleSessionRequest {
    pub session_id: String,
}

impl HostRecycleSessionRequest {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
        }
    }

    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string" }
            },
            "required": ["session_id"],
            "additionalProperties": false
        })
    }
}

/// Session 生命周期的稳定线缆表示。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionLifecycleStateDto {
    Active,
    Recycled,
}

impl From<SessionLifecycleState> for SessionLifecycleStateDto {
    fn from(state: SessionLifecycleState) -> Self {
        match state {
            SessionLifecycleState::Active => Self::Active,
            SessionLifecycleState::Recycled => Self::Recycled,
        }
    }
}

/// `astrcode.session.control.state` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionStateOutput {
    pub lifecycle: SessionLifecycleStateDto,
    pub phase: String,
    pub active_turn_id: Option<String>,
    pub queued_inputs: usize,
    pub message_count: usize,
}

impl HostSessionStateOutput {
    pub fn from_state(state: SessionState, phase: String) -> Self {
        Self {
            lifecycle: state.lifecycle.into(),
            phase,
            active_turn_id: state.active_turn_id,
            queued_inputs: state.queued_inputs,
            message_count: state.message_count,
        }
    }

    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "lifecycle": { "type": "string", "enum": ["active", "recycled"] },
                "phase": { "type": "string" },
                "active_turn_id": { "type": ["string", "null"] },
                "queued_inputs": { "type": "integer", "minimum": 0 },
                "message_count": { "type": "integer", "minimum": 0 }
            },
            "required": [
                "lifecycle",
                "phase",
                "active_turn_id",
                "queued_inputs",
                "message_count"
            ],
            "additionalProperties": false
        })
    }
}

/// `astrcode.session.control.reactivate` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionReactivateOutput {
    pub session_id: String,
    pub reactivated: bool,
}

impl HostSessionReactivateOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string" },
                "reactivated": { "type": "boolean" }
            },
            "required": ["session_id", "reactivated"],
            "additionalProperties": false
        })
    }

    pub fn from_result(session_id: String, result: SessionReactivation) -> Self {
        Self {
            session_id,
            reactivated: result.reactivated,
        }
    }
}

/// `astrcode.session.control.submit_turn` 的线缆请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSubmitTurnRequest {
    pub target_session_id: String,
    pub user_prompt: String,
    #[serde(default = "default_wait_for_result")]
    pub wait_for_result: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_parent_on_complete: Option<String>,
    #[serde(default)]
    pub recycle_on_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

const fn default_wait_for_result() -> bool {
    true
}

impl HostSubmitTurnRequest {
    /// 创建适用于外置 worker 的异步子会话 turn 请求。
    pub fn background(
        target_session_id: impl Into<String>,
        user_prompt: impl Into<String>,
    ) -> Self {
        Self {
            target_session_id: target_session_id.into(),
            user_prompt: user_prompt.into(),
            wait_for_result: false,
            notify_parent_on_complete: None,
            recycle_on_complete: true,
            tool_call_id: None,
        }
    }

    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "target_session_id": { "type": "string" },
                "user_prompt": { "type": "string" },
                "wait_for_result": { "type": "boolean" },
                "notify_parent_on_complete": { "type": "string" },
                "recycle_on_complete": { "type": "boolean" },
                "tool_call_id": { "type": "string" }
            },
            "required": ["target_session_id", "user_prompt"],
            "additionalProperties": false
        })
    }
}

/// Submit a turn to a top-level session owned by the calling source extension.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostRootSubmitTurnRequest {
    pub target_session_id: String,
    pub user_prompt: String,
    #[serde(default = "default_wait_for_result")]
    pub wait_for_result: bool,
}

impl HostRootSubmitTurnRequest {
    pub fn new(target_session_id: impl Into<String>, user_prompt: impl Into<String>) -> Self {
        Self {
            target_session_id: target_session_id.into(),
            user_prompt: user_prompt.into(),
            wait_for_result: true,
        }
    }

    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "target_session_id": { "type": "string" },
                "user_prompt": { "type": "string" },
                "wait_for_result": { "type": "boolean" }
            },
            "required": ["target_session_id", "user_prompt"],
            "additionalProperties": false
        })
    }
}

/// Cursor page request for `astrcode.session.read_events`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionEventsPageRequest {
    pub session_id: String,
    #[serde(default = "default_session_events_cursor")]
    pub cursor: String,
    #[serde(default = "default_session_events_limit")]
    pub limit: usize,
}

impl HostSessionEventsPageRequest {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            cursor: default_session_events_cursor(),
            limit: default_session_events_limit(),
        }
    }

    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string" },
                "cursor": { "type": "string", "default": "0" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 500, "default": 100 }
            },
            "required": ["session_id"],
            "additionalProperties": false
        })
    }
}

fn default_session_events_cursor() -> String {
    "0".into()
}

const fn default_session_events_limit() -> usize {
    100
}

/// Stable event envelope returned by `astrcode.session.read_events`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionEvent {
    pub seq: u64,
    pub id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub timestamp: String,
    pub payload: Value,
}

/// Cursor page response for `astrcode.session.read_events`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionEventsPageOutput {
    pub events: Vec<HostSessionEvent>,
    pub next_cursor: String,
    pub has_more: bool,
}

impl HostSessionEventsPageOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "events": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "seq": { "type": "integer", "minimum": 0 },
                            "id": { "type": "string" },
                            "session_id": { "type": "string" },
                            "turn_id": { "type": ["string", "null"] },
                            "timestamp": { "type": "string" },
                            "payload": { "type": "object" }
                        },
                        "required": ["seq", "id", "session_id", "timestamp", "payload"],
                        "additionalProperties": false
                    }
                },
                "next_cursor": { "type": "string" },
                "has_more": { "type": "boolean" }
            },
            "required": ["events", "next_cursor", "has_more"],
            "additionalProperties": false
        })
    }
}

/// `astrcode.session.control.submit_turn` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostSubmitTurnOutput {
    Completed { content: String },
    Backgrounded { task_id: String, session_id: String },
}

impl HostSubmitTurnOutput {
    pub fn wire_schema() -> Value {
        json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "status": { "const": "completed" },
                        "content": { "type": "string" }
                    },
                    "required": ["status", "content"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "status": { "const": "backgrounded" },
                        "task_id": { "type": "string" },
                        "session_id": { "type": "string" }
                    },
                    "required": ["status", "task_id", "session_id"],
                    "additionalProperties": false
                }
            ]
        })
    }
}

impl From<SubmitTurnResult> for HostSubmitTurnOutput {
    fn from(result: SubmitTurnResult) -> Self {
        match result {
            SubmitTurnResult::Completed { content } => Self::Completed { content },
            SubmitTurnResult::Backgrounded {
                task_id,
                session_id,
            } => Self::Backgrounded {
                task_id,
                session_id,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn tool_selection_constructors_preserve_explicit_wire_semantics() {
        assert_eq!(
            SessionToolSelectionDto::all(),
            SessionToolSelectionDto::All { except: Vec::new() }
        );
        assert_eq!(
            SessionToolSelectionDto::all_except(["agent", "write"]),
            SessionToolSelectionDto::All {
                except: vec!["agent".into(), "write".into()]
            }
        );
        assert_eq!(
            SessionToolSelectionDto::only(["read", "grep"]),
            SessionToolSelectionDto::Only {
                names: vec!["read".into(), "grep".into()]
            }
        );
        assert_eq!(
            SessionToolSelectionDto::no_tools(),
            SessionToolSelectionDto::Only { names: Vec::new() }
        );
    }

    #[test]
    fn session_control_schemas_cover_every_serialized_field() {
        let create = HostCreateSessionRequest {
            name: "reviewer".into(),
            working_dir: Some("workspace".into()),
            system_prompt: Some("review".into()),
            model_preference: Some("small".into()),
            tool_selection: Some(SessionToolSelectionDto::no_tools()),
            ephemeral: true,
            tool_call_id: Some("call-1".into()),
        };
        assert_schema_fields(&create, &HostCreateSessionRequest::wire_schema());
        assert_schema_fields(
            &HostCreateSessionOutput {
                session_id: "child-1".into(),
            },
            &HostCreateSessionOutput::wire_schema(),
        );
        assert_schema_fields(
            &HostSessionTargetRequest {
                target_session_id: "child-1".into(),
            },
            &HostSessionTargetRequest::wire_schema(),
        );
        assert_schema_fields(
            &HostRecycleSessionRequest::new("child-1"),
            &HostRecycleSessionRequest::wire_schema(),
        );
        assert_schema_fields(
            &HostSessionStateOutput {
                lifecycle: SessionLifecycleStateDto::Recycled,
                phase: "idle".into(),
                active_turn_id: None,
                queued_inputs: 0,
                message_count: 2,
            },
            &HostSessionStateOutput::wire_schema(),
        );
        assert_schema_fields(
            &HostSessionReactivateOutput {
                session_id: "child-1".into(),
                reactivated: true,
            },
            &HostSessionReactivateOutput::wire_schema(),
        );

        let submit = HostSubmitTurnRequest {
            target_session_id: "child-1".into(),
            user_prompt: "review".into(),
            wait_for_result: false,
            notify_parent_on_complete: Some("done".into()),
            recycle_on_complete: true,
            tool_call_id: Some("call-1".into()),
        };
        assert_schema_fields(&submit, &HostSubmitTurnRequest::wire_schema());
        assert_schema_fields(
            &HostRootSubmitTurnRequest::new("root-1", "review"),
            &HostRootSubmitTurnRequest::wire_schema(),
        );

        let events_request: HostSessionEventsPageRequest =
            serde_json::from_value(json!({ "session_id": "root-1" })).unwrap();
        assert_eq!(events_request.cursor, "0");
        assert_eq!(events_request.limit, 100);
        assert_schema_fields(
            &events_request,
            &HostSessionEventsPageRequest::wire_schema(),
        );
        assert_schema_fields(
            &HostSessionEventsPageOutput {
                events: vec![HostSessionEvent {
                    seq: 1,
                    id: "event-1".into(),
                    session_id: "root-1".into(),
                    turn_id: None,
                    timestamp: "2026-08-06T00:00:00Z".into(),
                    payload: json!({ "type": "turn_started" }),
                }],
                next_cursor: "1".into(),
                has_more: true,
            },
            &HostSessionEventsPageOutput::wire_schema(),
        );

        let output_schema = HostSubmitTurnOutput::wire_schema();
        for (output, variant) in [
            (
                HostSubmitTurnOutput::Completed {
                    content: "done".into(),
                },
                &output_schema["oneOf"][0],
            ),
            (
                HostSubmitTurnOutput::Backgrounded {
                    task_id: "turn-1".into(),
                    session_id: "child-1".into(),
                },
                &output_schema["oneOf"][1],
            ),
        ] {
            assert_schema_fields(&output, variant);
        }
    }

    fn assert_schema_fields<T: Serialize>(value: &T, schema: &Value) {
        let serialized = serde_json::to_value(value).unwrap();
        let serialized_fields = serialized
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let schema_fields = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        assert_eq!(serialized_fields, schema_fields);
    }
}
