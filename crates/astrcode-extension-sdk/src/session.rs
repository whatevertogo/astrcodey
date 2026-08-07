//! 会话原子操作 API re-export。
//!
//! `SessionOperations` trait 定义在 `astrcode-core/src/tool.rs`，此处 re-export
//! 方便插件侧使用。

use astrcode_core::event::Phase;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;

use crate::host::schema::{
    create_session_tool_selection_schema, derived_wire_schema, json_object_schema,
    nullable_string_schema,
};
pub use crate::{
    extension::SessionToolSelection,
    tool::{
        CreateSessionRequest, SessionAccess, SessionAccessPair, SessionApiError, SessionHandle,
        SessionLifecycleState, SessionOperations, SessionReactivation, SessionState, SessionStatus,
        SubmitTurnRequest, SubmitTurnResult,
    },
};

/// 插件 session API 共用的工具选择线缆契约。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
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

    /// 返回该线缆类型的 JSON Schema。description 随调用点语境变化,derive 生成后在
    /// 边界注入描述,而不是为每个调用点重复手写。
    pub fn wire_schema(description: &str) -> Value {
        let mut schema = derived_wire_schema::<Self>();
        schema["type"] = Value::String("object".into());
        schema["description"] = Value::String(description.into());
        schema
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostCreateSessionRequest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_preference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "create_session_tool_selection_schema")]
    pub tool_selection: Option<SessionToolSelectionDto>,
    #[serde(default)]
    pub ephemeral: bool,
}

impl HostCreateSessionRequest {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            system_prompt: None,
            model_preference: None,
            tool_selection: None,
            ephemeral: false,
        }
    }

    pub fn wire_schema() -> Value {
        derived_wire_schema::<Self>()
    }
}

/// `astrcode.session.control.create` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostCreateSessionOutput {
    pub session_id: String,
}

impl HostCreateSessionOutput {
    pub fn wire_schema() -> Value {
        derived_wire_schema::<Self>()
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
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionTargetRequest {
    pub target_session_id: String,
}

impl HostSessionTargetRequest {
    pub fn wire_schema() -> Value {
        derived_wire_schema::<Self>()
    }
}

/// `astrcode.session.control.dispose` request. The operation recycles rather than deletes.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
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
        derived_wire_schema::<Self>()
    }
}

/// Session 生命周期的稳定线缆表示。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
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

/// Session execution phase at the extension wire boundary.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhaseDto {
    Idle,
    Thinking,
    Streaming,
    CallingTool,
    Compacting,
    Error,
}

impl SessionPhaseDto {
    pub fn wire_schema() -> Value {
        derived_wire_schema::<Self>()
    }
}

impl From<Phase> for SessionPhaseDto {
    fn from(phase: Phase) -> Self {
        match phase {
            Phase::Idle => Self::Idle,
            Phase::Thinking => Self::Thinking,
            Phase::Streaming => Self::Streaming,
            Phase::CallingTool => Self::CallingTool,
            Phase::Compacting => Self::Compacting,
            Phase::Error => Self::Error,
        }
    }
}

/// `astrcode.session.control.state` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionStateOutput {
    pub lifecycle: SessionLifecycleStateDto,
    pub phase: SessionPhaseDto,
    #[schemars(required, schema_with = "nullable_string_schema")]
    pub active_turn_id: Option<String>,
    pub queued_inputs: usize,
    pub message_count: usize,
}

impl HostSessionStateOutput {
    pub fn from_state(state: SessionState) -> Self {
        Self {
            lifecycle: state.lifecycle.into(),
            phase: state.phase.into(),
            active_turn_id: state.active_turn_id,
            queued_inputs: state.queued_inputs,
            message_count: state.message_count,
        }
    }

    pub fn wire_schema() -> Value {
        derived_wire_schema::<Self>()
    }
}

/// `astrcode.session.control.reactivate` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionReactivateOutput {
    pub session_id: String,
    pub reactivated: bool,
}

impl HostSessionReactivateOutput {
    pub fn wire_schema() -> Value {
        derived_wire_schema::<Self>()
    }

    pub fn from_result(session_id: String, result: SessionReactivation) -> Self {
        Self {
            session_id,
            reactivated: result.reactivated,
        }
    }
}

/// `astrcode.session.control.submit_turn` 的线缆请求。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
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
        }
    }

    pub fn wire_schema() -> Value {
        derived_wire_schema::<Self>()
    }
}

/// Submit a turn to a top-level session owned by the calling source extension.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
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
        derived_wire_schema::<Self>()
    }
}

/// Cursor page request for `astrcode.session.read_events`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionEventsPageRequest {
    pub session_id: String,
    #[serde(default = "default_session_events_cursor")]
    pub cursor: String,
    #[serde(default = "default_session_events_limit")]
    #[schemars(range(min = 1, max = 500))]
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
        derived_wire_schema::<Self>()
    }
}

fn default_session_events_cursor() -> String {
    "0".into()
}

const fn default_session_events_limit() -> usize {
    100
}

/// Stable event envelope returned by `astrcode.session.read_events`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionEvent {
    pub seq: u64,
    pub id: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub timestamp: String,
    #[schemars(schema_with = "json_object_schema")]
    pub payload: Value,
}

/// Cursor page response for `astrcode.session.read_events`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionEventsPageOutput {
    pub events: Vec<HostSessionEvent>,
    pub next_cursor: String,
    pub has_more: bool,
}

impl HostSessionEventsPageOutput {
    pub fn wire_schema() -> Value {
        derived_wire_schema::<Self>()
    }
}

/// `astrcode.session.control.submit_turn` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostSubmitTurnOutput {
    Completed { content: String },
    Backgrounded { task_id: String, session_id: String },
}

impl HostSubmitTurnOutput {
    pub fn wire_schema() -> Value {
        derived_wire_schema::<Self>()
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

    use serde::de::DeserializeOwned;

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
    fn session_phase_dto_is_closed_and_tracks_every_core_phase() {
        let cases = [
            (Phase::Idle, SessionPhaseDto::Idle, "idle"),
            (Phase::Thinking, SessionPhaseDto::Thinking, "thinking"),
            (Phase::Streaming, SessionPhaseDto::Streaming, "streaming"),
            (
                Phase::CallingTool,
                SessionPhaseDto::CallingTool,
                "calling_tool",
            ),
            (Phase::Compacting, SessionPhaseDto::Compacting, "compacting"),
            (Phase::Error, SessionPhaseDto::Error, "error"),
        ];

        for (phase, expected, wire) in cases {
            let actual = SessionPhaseDto::from(phase);
            assert_eq!(actual, expected);
            assert_eq!(serde_json::to_value(actual).unwrap(), json!(wire));
        }
        assert!(serde_json::from_value::<SessionPhaseDto>(json!("future_phase")).is_err());
    }

    #[test]
    fn session_control_schemas_cover_every_serialized_field() {
        let create = HostCreateSessionRequest {
            name: "reviewer".into(),
            system_prompt: Some("review".into()),
            model_preference: Some("small".into()),
            tool_selection: Some(SessionToolSelectionDto::no_tools()),
            ephemeral: true,
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
                phase: SessionPhaseDto::Idle,
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

    #[test]
    fn optional_session_fields_accept_explicit_nulls_in_schema_and_serde() {
        let create: HostCreateSessionRequest = serde_json::from_value(json!({
            "name": "reviewer",
            "system_prompt": null,
            "model_preference": null,
            "tool_selection": null
        }))
        .unwrap();
        assert_eq!(create.system_prompt, None);
        assert_eq!(create.model_preference, None);
        assert_eq!(create.tool_selection, None);

        let submit: HostSubmitTurnRequest = serde_json::from_value(json!({
            "target_session_id": "child-1",
            "user_prompt": "review",
            "notify_parent_on_complete": null
        }))
        .unwrap();
        assert_eq!(submit.notify_parent_on_complete, None);

        let create_schema = HostCreateSessionRequest::wire_schema();
        for field in ["system_prompt", "model_preference"] {
            assert_eq!(
                create_schema["properties"][field]["type"],
                json!(["string", "null"])
            );
        }
        assert!(
            create_schema["properties"]["tool_selection"]["anyOf"]
                .as_array()
                .unwrap()
                .iter()
                .any(|schema| schema["type"] == "null")
        );
        assert_eq!(
            HostSubmitTurnRequest::wire_schema()["properties"]["notify_parent_on_complete"]["type"],
            json!(["string", "null"])
        );
    }

    fn assert_schema_fields<T>(value: &T, schema: &Value)
    where
        T: Serialize + DeserializeOwned,
    {
        let serialized = serde_json::to_value(value).unwrap();
        serde_json::from_value::<T>(serialized.clone()).unwrap();
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
        assert_eq!(schema["additionalProperties"], false);

        let mut invalid = serialized;
        invalid
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), Value::Bool(true));
        assert!(serde_json::from_value::<T>(invalid).is_err());
    }
}
