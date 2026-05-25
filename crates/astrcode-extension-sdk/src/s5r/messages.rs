//! s5r 线缆消息类型。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::extension::{ExtensionEvent, HookMode};

/// s5r 协议当前版本。
pub const S5R_VERSION: &str = "1.0";

/// 协议 metadata 中的栈标识。
pub const S5R_STACK: &str = "astrcode";

/// Meta 能力：宿主调用 guest 注册的 handler。
pub const CAP_HANDLER_INVOKE: &str = "handler.invoke";

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
pub struct InitializeMsg {
    pub id: String,
    pub peer: PeerInfo,
    #[serde(default)]
    pub handlers: Vec<HandlerDescriptor>,
    #[serde(default)]
    pub provided_capabilities: Vec<CapabilityDescriptor>,
    #[serde(default)]
    pub requested_capabilities: Vec<String>,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeOutput {
    pub peer: PeerInfo,
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: Vec<CapabilityDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub name: String,
    pub role: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub kind: ResultKind,
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
    pub caller_extension_id: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelMsg {
    pub id: String,
    #[serde(default)]
    pub reason: String,
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
        "post_tool_use_failure" => Some(ExtensionEvent::PostToolUseFailure),
        "before_provider_request" => Some(ExtensionEvent::BeforeProviderRequest),
        "after_provider_response" => Some(ExtensionEvent::AfterProviderResponse),
        "user_prompt_submit" => Some(ExtensionEvent::UserPromptSubmit),
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
        ExtensionEvent::PostToolUseFailure => "post_tool_use_failure",
        ExtensionEvent::BeforeProviderRequest => "before_provider_request",
        ExtensionEvent::AfterProviderResponse => "after_provider_response",
        ExtensionEvent::UserPromptSubmit => "user_prompt_submit",
        ExtensionEvent::PromptBuild => "prompt_build",
        ExtensionEvent::PreCompact => "pre_compact",
        ExtensionEvent::PostCompact => "post_compact",
        ExtensionEvent::PostRecap => "post_recap",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_message_roundtrip() {
        let msg = WireMessage::Invoke(InvokeMsg {
            id: "req-1".into(),
            capability: "handler.invoke".into(),
            input: serde_json::json!({}),
            stream: false,
            caller_extension_id: Some("ext".into()),
        });
        let json = serde_json::to_string(&msg).unwrap();
        let back: WireMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, WireMessage::Invoke(_)));
    }
}
