//! s5r 线缆消息类型（对齐 AstrBot `protocol/messages.py`）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::extension::{ExtensionEvent, HookMode};

/// s5r 协议当前版本。
pub const S5R_VERSION: &str = "2.0";

/// 协议 metadata 中的栈标识。
pub const S5R_STACK: &str = "astrcode";

/// Meta 能力：宿主调用 guest 注册的 handler。
pub const CAP_HANDLER_INVOKE: &str = "handler.invoke";

/// Peer-owned liveness probe; it never depends on an extension handler.
pub const CAP_RUNTIME_PING: &str = "s5r.runtime.ping";

pub const WIRE_CODEC_JSON: &str = "json";
pub const WIRE_CODEC_METADATA_KEY: &str = "wire_codec";
/// Wire feature: nested invokes carry the id of the inbound invoke that created them.
pub const WIRE_FEATURE_PARENT_INVOKE_ID: &str = "parent_invoke_id";

/// Handler 标识的线缆格式：`<extension_id>:<kind>:<name>`。
pub(crate) fn handler_id_for(extension_id: &str, kind: &str, name: &str) -> String {
    format!("{extension_id}:{kind}:{name}")
}

/// 五类线缆消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireMessage {
    Initialize(InitializeMsg),
    Result(ResultMsg),
    Invoke(InvokeMsg),
    Event(EventMsg),
    Cancel(CancelMsg),
}

impl WireMessage {
    pub fn id(&self) -> &str {
        match self {
            Self::Initialize(m) => &m.id,
            Self::Result(m) => &m.id,
            Self::Invoke(m) => &m.id,
            Self::Event(m) => &m.id,
            Self::Cancel(m) => &m.id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeMsg {
    pub id: String,
    pub protocol_version: String,
    pub peer: PeerInfo,
    #[serde(default)]
    pub handlers: Vec<HandlerDescriptor>,
    #[serde(default)]
    pub provided_capabilities: Vec<CapabilityDescriptor>,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeOutput {
    pub peer: PeerInfo,
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: Vec<CapabilityDescriptor>,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerInfo {
    pub name: String,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandlerDescriptor {
    pub handler_id: String,
    pub description: String,
    #[serde(default)]
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityDescriptor {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub input_schema: Value,
    #[serde(default)]
    pub output_schema: Value,
    #[serde(default)]
    pub supports_stream: bool,
    #[serde(default)]
    pub cancelable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultKind {
    InitializeResult,
    InvokeResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultMsg {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ResultKind>,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvokeMsg {
    pub id: String,
    pub capability: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_invoke_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventPhase {
    Started,
    Delta,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventMsg {
    pub id: String,
    pub phase: EventPhase,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub output: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelMsg {
    pub id: String,
    #[serde(default = "default_cancel_reason")]
    pub reason: String,
}

fn default_cancel_reason() -> String {
    "user_cancelled".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ErrorPayload {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            hint: None,
            retryable: false,
            details: None,
        }
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }
}

pub fn parse_wire_message(payload: &[u8]) -> Result<WireMessage, String> {
    serde_json::from_slice(payload).map_err(|e| format!("parse s5r message: {e}"))
}

pub fn encode_wire_message(msg: &WireMessage) -> Result<Vec<u8>, String> {
    serde_json::to_vec(msg).map_err(|e| format!("encode s5r message: {e}"))
}

/// s5r 事件名 → [`ExtensionEvent`]。
pub fn event_from_name(name: &str) -> Option<ExtensionEvent> {
    match name {
        "session_start" => Some(ExtensionEvent::SessionStart),
        "session_resume" => Some(ExtensionEvent::SessionResume),
        "session_shutdown" => Some(ExtensionEvent::SessionShutdown),
        "turn_start" => Some(ExtensionEvent::TurnStart),
        "turn_end" => Some(ExtensionEvent::TurnEnd),
        "turn_aborted" => Some(ExtensionEvent::TurnAborted),
        "step_start" => Some(ExtensionEvent::StepStart),
        "step_end" => Some(ExtensionEvent::StepEnd),
        "pre_tool_use" => Some(ExtensionEvent::PreToolUse),
        "post_tool_use" => Some(ExtensionEvent::PostToolUse),
        "before_provider_request" => Some(ExtensionEvent::BeforeProviderRequest),
        "after_provider_response" => Some(ExtensionEvent::AfterProviderResponse),
        "continue_after_stop" => Some(ExtensionEvent::ContinueAfterStop),
        "user_prompt_submit" => Some(ExtensionEvent::UserPromptSubmit),
        "user_message_envelope" => Some(ExtensionEvent::UserMessageEnvelope),
        "prompt_build" => Some(ExtensionEvent::PromptBuild),
        "pre_compact" => Some(ExtensionEvent::PreCompact),
        "post_compact" => Some(ExtensionEvent::PostCompact),
        "post_recap" => Some(ExtensionEvent::PostRecap),
        _ => None,
    }
}

pub fn mode_from_name(name: &str) -> Option<HookMode> {
    match name {
        "blocking" => Some(HookMode::Blocking),
        "non_blocking" => Some(HookMode::NonBlocking),
        "advisory" => Some(HookMode::Advisory),
        _ => None,
    }
}

pub fn mode_to_name(mode: HookMode) -> &'static str {
    match mode {
        HookMode::Blocking => "blocking",
        HookMode::NonBlocking => "non_blocking",
        HookMode::Advisory => "advisory",
    }
}

pub fn event_to_name(event: &ExtensionEvent) -> &'static str {
    match event {
        ExtensionEvent::SessionStart => "session_start",
        ExtensionEvent::SessionResume => "session_resume",
        ExtensionEvent::SessionShutdown => "session_shutdown",
        ExtensionEvent::TurnStart => "turn_start",
        ExtensionEvent::TurnEnd => "turn_end",
        ExtensionEvent::TurnAborted => "turn_aborted",
        ExtensionEvent::StepStart => "step_start",
        ExtensionEvent::StepEnd => "step_end",
        ExtensionEvent::PreToolUse => "pre_tool_use",
        ExtensionEvent::PostToolUse => "post_tool_use",
        ExtensionEvent::BeforeProviderRequest => "before_provider_request",
        ExtensionEvent::AfterProviderResponse => "after_provider_response",
        ExtensionEvent::ContinueAfterStop => "continue_after_stop",
        ExtensionEvent::UserPromptSubmit => "user_prompt_submit",
        ExtensionEvent::UserMessageEnvelope => "user_message_envelope",
        ExtensionEvent::PromptBuild => "prompt_build",
        ExtensionEvent::PreCompact => "pre_compact",
        ExtensionEvent::PostCompact => "post_compact",
        ExtensionEvent::PostRecap => "post_recap",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::ModelSelection,
        extension::{RuntimeHookCallContext, RuntimeLifecycleContext},
    };

    #[test]
    fn lifecycle_context_for_step_start_carries_sync_count() {
        let ctx = RuntimeLifecycleContext::new(
            RuntimeHookCallContext::new("s1", "/tmp", ModelSelection::simple("m"), None),
            None,
        );
        let step = ctx.for_step_start(2);
        assert_eq!(step.mid_turn_user_messages_synced(), 2);
    }

    #[test]
    fn event_and_mode_names_roundtrip() {
        assert_eq!(
            event_from_name("continue_after_stop"),
            Some(ExtensionEvent::ContinueAfterStop)
        );
        assert_eq!(
            event_to_name(&ExtensionEvent::ContinueAfterStop),
            "continue_after_stop"
        );
        assert_eq!(mode_from_name("advisory"), Some(HookMode::Advisory));
        assert_eq!(mode_to_name(HookMode::Advisory), "advisory");
    }

    #[test]
    fn wire_message_roundtrip() {
        let msg = WireMessage::Invoke(InvokeMsg {
            id: "req-1".into(),
            capability: "handler.invoke".into(),
            input: serde_json::json!({}),
            stream: false,
            parent_invoke_id: Some("req-parent".into()),
        });
        let json = serde_json::to_string(&msg).unwrap();
        let back: WireMessage = serde_json::from_str(&json).unwrap();
        let WireMessage::Invoke(back) = back else {
            panic!("expected invoke");
        };
        assert_eq!(back.parent_invoke_id.as_deref(), Some("req-parent"));
    }

    #[test]
    fn initialize_rejects_unknown_fields_at_each_typed_boundary() {
        let cases: [(&str, &[u8]); 3] = [
            (
                "initialize",
                br#"{"type":"initialize","id":"req-1","protocol_version":"2.0","peer":{"name":"worker","role":"plugin"},"unexpected":true}"#,
            ),
            (
                "handler",
                br#"{"type":"initialize","id":"req-1","protocol_version":"2.0","peer":{"name":"worker","role":"plugin"},"handlers":[{"handler_id":"ext:tool:test","description":"test","unexpected":true}]}"#,
            ),
            (
                "peer",
                br#"{"type":"initialize","id":"req-1","protocol_version":"2.0","peer":{"name":"worker","role":"plugin","unexpected":true}}"#,
            ),
        ];

        for (boundary, payload) in cases {
            let Err(error) = parse_wire_message(payload) else {
                panic!("{boundary} accepted an unknown field");
            };
            assert!(
                error.contains("unknown field") && error.contains("unexpected"),
                "{boundary} returned an unexpected parse error: {error}"
            );
        }
    }

    #[test]
    fn initialize_output_requires_the_negotiated_version_and_rejects_unknown_fields() {
        let valid = serde_json::json!({
            "peer": { "name": "host", "role": "host" },
            "protocol_version": S5R_VERSION,
            "capabilities": [],
            "metadata": {}
        });
        assert!(serde_json::from_value::<InitializeOutput>(valid.clone()).is_ok());

        let mut missing_version = valid.clone();
        missing_version
            .as_object_mut()
            .unwrap()
            .remove("protocol_version");
        assert!(serde_json::from_value::<InitializeOutput>(missing_version).is_err());

        let mut unknown_field = valid;
        unknown_field["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<InitializeOutput>(unknown_field).is_err());
    }

    #[test]
    fn detached_invoke_without_parent_id_decodes() {
        let message: WireMessage = serde_json::from_value(serde_json::json!({
            "type": "invoke",
            "id": "detached-1",
            "capability": "handler.invoke",
            "input": {},
            "stream": false
        }))
        .unwrap();

        let WireMessage::Invoke(invoke) = message else {
            panic!("expected invoke");
        };
        assert!(invoke.parent_invoke_id.is_none());
    }
}
