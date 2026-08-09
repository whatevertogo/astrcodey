//! s5r 线缆消息类型（对齐 AstrBot `protocol/messages.py`）。

use astrcode_core::wire::WireErrorCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::extension::{CompactEvent, HookMode, LifecycleEvent};

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
///
/// 构造（注册/描述符）与解析（归属校验）都走这一类型，格式不可能在两处漂移。
/// wire 上始终是稳定字符串（[`HandlerId::as_str`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandlerId(String);

impl HandlerId {
    pub fn new(extension_id: &str, kind: HandlerKind, name: &str) -> Self {
        Self(format!("{extension_id}:{}:{name}", kind.as_str()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 解析 wire 字符串：校验 kind 白名单与 name 非空，非法返回 `None`。
    pub fn parse(wire: &str) -> Option<Self> {
        Self::split(wire).map(|_| Self(wire.to_owned()))
    }

    /// 分解为 `(extension_id, kind, name)`，全部借用输入；由 [`Self::parse`] 构造的
    /// 标识经 [`Self::parts`] 恒为 `Some`。
    pub fn split(wire: &str) -> Option<(&str, HandlerKind, &str)> {
        let (extension_id, remainder) = wire.split_once(':')?;
        let (kind, name) = remainder.split_once(':')?;
        let kind = HandlerKind::parse(kind)?;
        if name.is_empty() {
            return None;
        }
        Some((extension_id, kind, name))
    }

    /// 分解为 `(extension_id, kind, name)`；由 [`Self::parse`] 构造的标识恒为 `Some`。
    pub fn parts(&self) -> Option<(&str, HandlerKind, &str)> {
        Self::split(&self.0)
    }
}

impl From<HandlerId> for String {
    fn from(id: HandlerId) -> Self {
        id.0
    }
}

/// Handler 的种类；wire 上是稳定字符串（[`HandlerKind::as_str`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerKind {
    Tool,
    Hook,
    Command,
    Http,
    Event,
}

impl HandlerKind {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Hook => "hook",
            Self::Command => "command",
            Self::Http => "http",
            Self::Event => "event",
        }
    }

    pub fn parse(kind: &str) -> Option<Self> {
        Some(match kind {
            "tool" => Self::Tool,
            "hook" => Self::Hook,
            "command" => Self::Command,
            "http" => Self::Http,
            "event" => Self::Event,
            _ => return None,
        })
    }
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
    /// 以类型化错误码构造；wire 上序列化为 [`"as_str"`]。
    pub fn new(code: WireErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.as_str().to_owned(),
            message: message.into(),
            hint: None,
            retryable: false,
            details: None,
        }
    }

    /// 返回类型化错误码；未知码（旧宿主/新扩展）返回 `None`。
    pub fn code_enum(&self) -> Option<WireErrorCode> {
        WireErrorCode::parse(&self.code)
    }

    pub fn io_error(error: impl std::fmt::Display) -> Self {
        Self::new(WireErrorCode::IoError, error.to_string())
    }

    pub fn backend_unavailable(message: impl Into<String>) -> Self {
        Self::new(WireErrorCode::BackendUnavailable, message)
    }

    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::new(WireErrorCode::Cancelled, message)
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

/// s5r 事件名 → [`LifecycleEvent`]。
pub fn event_from_name(name: &str) -> Option<LifecycleEvent> {
    match name {
        "session_start" => Some(LifecycleEvent::SessionStart),
        "session_resume" => Some(LifecycleEvent::SessionResume),
        "session_shutdown" => Some(LifecycleEvent::SessionShutdown),
        "turn_start" => Some(LifecycleEvent::TurnStart),
        "turn_end" => Some(LifecycleEvent::TurnEnd),
        "turn_aborted" => Some(LifecycleEvent::TurnAborted),
        "step_start" => Some(LifecycleEvent::StepStart),
        "step_end" => Some(LifecycleEvent::StepEnd),
        "pre_tool_use" => Some(LifecycleEvent::PreToolUse),
        "post_tool_use" => Some(LifecycleEvent::PostToolUse),
        "before_provider_request" => Some(LifecycleEvent::BeforeProviderRequest),
        "after_provider_response" => Some(LifecycleEvent::AfterProviderResponse),
        "continue_after_stop" => Some(LifecycleEvent::ContinueAfterStop),
        "user_prompt_submit" => Some(LifecycleEvent::UserPromptSubmit),
        "user_message_envelope" => Some(LifecycleEvent::UserMessageEnvelope),
        "prompt_build" => Some(LifecycleEvent::PromptBuild),
        "post_recap" => Some(LifecycleEvent::PostRecap),
        _ => None,
    }
}

/// s5r compact hook 名 → [`CompactEvent`]。
pub fn compact_event_from_name(name: &str) -> Option<CompactEvent> {
    match name {
        "pre_compact" => Some(CompactEvent::PreCompact),
        "post_compact" => Some(CompactEvent::PostCompact),
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

pub fn event_to_name(event: &LifecycleEvent) -> &'static str {
    match event {
        LifecycleEvent::SessionStart => "session_start",
        LifecycleEvent::SessionResume => "session_resume",
        LifecycleEvent::SessionShutdown => "session_shutdown",
        LifecycleEvent::TurnStart => "turn_start",
        LifecycleEvent::TurnEnd => "turn_end",
        LifecycleEvent::TurnAborted => "turn_aborted",
        LifecycleEvent::StepStart => "step_start",
        LifecycleEvent::StepEnd => "step_end",
        LifecycleEvent::PreToolUse => "pre_tool_use",
        LifecycleEvent::PostToolUse => "post_tool_use",
        LifecycleEvent::BeforeProviderRequest => "before_provider_request",
        LifecycleEvent::AfterProviderResponse => "after_provider_response",
        LifecycleEvent::ContinueAfterStop => "continue_after_stop",
        LifecycleEvent::UserPromptSubmit => "user_prompt_submit",
        LifecycleEvent::UserMessageEnvelope => "user_message_envelope",
        LifecycleEvent::PromptBuild => "prompt_build",
        LifecycleEvent::PostRecap => "post_recap",
    }
}

pub fn compact_event_to_name(event: CompactEvent) -> &'static str {
    match event {
        CompactEvent::PreCompact => "pre_compact",
        CompactEvent::PostCompact => "post_compact",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::ModelSelection,
        extension::{LifecyclePayload, RuntimeHookCallContext, RuntimeLifecycleContext},
    };

    #[test]
    fn lifecycle_context_for_step_start_carries_sync_count() {
        let ctx = RuntimeLifecycleContext::new(
            RuntimeHookCallContext::new("s1", "/tmp", ModelSelection::simple("m"), None),
            LifecyclePayload::new(None),
        );
        let step = ctx.map_payload(|payload| payload.for_step_start(2));
        assert_eq!(step.mid_turn_user_messages_synced(), 2);
    }

    #[test]
    fn event_and_mode_names_roundtrip() {
        assert_eq!(
            event_from_name("continue_after_stop"),
            Some(LifecycleEvent::ContinueAfterStop)
        );
        assert_eq!(
            event_to_name(&LifecycleEvent::ContinueAfterStop),
            "continue_after_stop"
        );
        assert_eq!(mode_from_name("advisory"), Some(HookMode::Advisory));
        assert_eq!(mode_to_name(HookMode::Advisory), "advisory");
        assert_eq!(event_from_name("pre_compact"), None);
        assert_eq!(
            compact_event_from_name("pre_compact"),
            Some(CompactEvent::PreCompact)
        );
        assert_eq!(
            compact_event_to_name(CompactEvent::PostCompact),
            "post_compact"
        );
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
