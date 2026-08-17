//! S5R session wire contracts.

use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;

/// Projection-only provenance attached to a provider-visible session message.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionMessageOriginDto {
    TurnAborted,
    ToolCallFailed,
    ToolCallCancelled,
}

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
}

/// `astrcode.session.control.create` 的线缆请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostCreateSessionRequest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_preference: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
}

/// `astrcode.session.control.create` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostCreateSessionOutput {
    pub session_id: String,
}

/// 插件 session control 操作的目标。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionTargetRequest {
    pub target_session_id: String,
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
}

/// Session 生命周期的稳定线缆表示。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionLifecycleStateDto {
    Active,
    Recycled,
}

/// Session execution phase at the extension wire boundary.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhaseDto {
    Idle,
    Thinking,
    Streaming,
    CallingTool,
    Compacting,
    Error,
}

/// `astrcode.session.control.state` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionStateOutput {
    pub lifecycle: SessionLifecycleStateDto,
    pub phase: SessionPhaseDto,
    pub active_turn_id: Option<String>,
    pub queued_inputs: usize,
    pub message_count: usize,
}

/// `astrcode.session.control.reactivate` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionReactivateOutput {
    pub session_id: String,
    pub reactivated: bool,
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
}

/// Cursor page request for `astrcode.session.read_events`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionEventsPageRequest {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default = "default_session_events_limit")]
    pub limit: usize,
}

impl HostSessionEventsPageRequest {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            cursor: None,
            limit: default_session_events_limit(),
        }
    }
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

/// `astrcode.session.control.submit_turn` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostSubmitTurnOutput {
    Completed { content: String },
    Backgrounded { task_id: String, session_id: String },
}

#[cfg(test)]
mod tests {
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
    fn session_phase_dto_is_closed_and_uses_stable_wire_names() {
        let cases = [
            (SessionPhaseDto::Idle, "idle"),
            (SessionPhaseDto::Thinking, "thinking"),
            (SessionPhaseDto::Streaming, "streaming"),
            (SessionPhaseDto::CallingTool, "calling_tool"),
            (SessionPhaseDto::Compacting, "compacting"),
            (SessionPhaseDto::Error, "error"),
        ];

        for (phase, wire) in cases {
            assert_eq!(serde_json::to_value(phase).unwrap(), json!(wire));
        }
        assert!(serde_json::from_value::<SessionPhaseDto>(json!("future_phase")).is_err());
    }

    #[test]
    fn session_control_contracts_round_trip_and_reject_unknown_fields() {
        let create = HostCreateSessionRequest {
            name: "reviewer".into(),
            system_prompt: Some("review".into()),
            model_preference: Some("small".into()),
            tool_selection: Some(SessionToolSelectionDto::no_tools()),
            ephemeral: true,
        };
        assert_strict_round_trip(&create);
        assert_strict_round_trip(&HostCreateSessionOutput {
            session_id: "child-1".into(),
        });
        assert_strict_round_trip(&HostSessionTargetRequest {
            target_session_id: "child-1".into(),
        });
        assert_strict_round_trip(&HostRecycleSessionRequest::new("child-1"));
        assert_strict_round_trip(&HostSessionStateOutput {
            lifecycle: SessionLifecycleStateDto::Recycled,
            phase: SessionPhaseDto::Idle,
            active_turn_id: None,
            queued_inputs: 0,
            message_count: 2,
        });
        assert_strict_round_trip(&HostSessionReactivateOutput {
            session_id: "child-1".into(),
            reactivated: true,
        });

        let submit = HostSubmitTurnRequest {
            target_session_id: "child-1".into(),
            user_prompt: "review".into(),
            wait_for_result: false,
            notify_parent_on_complete: Some("done".into()),
            recycle_on_complete: true,
        };
        assert_strict_round_trip(&submit);
        assert_strict_round_trip(&HostRootSubmitTurnRequest::new("root-1", "review"));

        let events_request: HostSessionEventsPageRequest =
            serde_json::from_value(json!({ "session_id": "root-1" })).unwrap();
        assert_eq!(events_request.cursor, None);
        assert_eq!(events_request.limit, 100);
        assert_strict_round_trip(&events_request);
        assert_strict_round_trip(&HostSessionEventsPageOutput {
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
        });

        for output in [
            HostSubmitTurnOutput::Completed {
                content: "done".into(),
            },
            HostSubmitTurnOutput::Backgrounded {
                task_id: "turn-1".into(),
                session_id: "child-1".into(),
            },
        ] {
            assert_strict_round_trip(&output);
        }
    }

    #[test]
    fn optional_session_fields_accept_explicit_nulls() {
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
    }

    fn assert_strict_round_trip<T>(value: &T)
    where
        T: Serialize + DeserializeOwned,
    {
        let serialized = serde_json::to_value(value).unwrap();
        serde_json::from_value::<T>(serialized.clone()).unwrap();

        let mut invalid = serialized;
        invalid
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), Value::Bool(true));
        assert!(serde_json::from_value::<T>(invalid).is_err());
    }
}
