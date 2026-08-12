use std::{
    collections::BTreeSet,
    fmt::{self, Display},
};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{WireErrorCode, manifest::InitializeManifest};

pub const S5R_VERSION: &str = "3.0";
pub const S5R_STACK: &str = "astrcode";
pub const WIRE_CODEC_JSON: &str = "json";

pub const FEATURE_NESTED_INVOKE_V1: &str = "nested_invoke_v1";
pub const FEATURE_MODEL_STREAM_V1: &str = "model_stream_v1";
pub const FEATURE_CUSTOM_EVENT_V1: &str = "custom_event_v1";
pub const CAP_HANDLER_INVOKE: &str = "handler.invoke";
pub const CAP_RUNTIME_PING: &str = "s5r.runtime.ping";
pub const CONFORMANCE_UNARY: &str = "s5r.conformance.unary";
pub const CONFORMANCE_STREAM: &str = "s5r.conformance.stream";
pub const CONFORMANCE_NESTED: &str = "s5r.conformance.nested";
pub const CONFORMANCE_WAIT_FOR_CANCEL: &str = "s5r.conformance.wait_for_cancel";
pub const CONFORMANCE_UNKNOWN_ERROR: &str = "s5r.conformance.unknown_error";
pub const CONFORMANCE_HOST_ECHO: &str = "s5r.conformance.host_echo";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct HandlerId(String);

impl HandlerId {
    pub fn new(extension_id: &str, kind: HandlerKind, name: &str) -> Result<Self, String> {
        let wire = format!("{extension_id}:{}:{name}", kind.as_str());
        Self::parse(&wire).ok_or_else(|| format!("invalid handler id {wire:?}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parse(wire: &str) -> Option<Self> {
        Self::split(wire).map(|_| Self(wire.to_owned()))
    }

    pub fn split(wire: &str) -> Option<(&str, HandlerKind, &str)> {
        let (extension_id, remainder) = wire.split_once(':')?;
        let (kind, name) = remainder.split_once(':')?;
        let kind = HandlerKind::parse(kind)?;
        if extension_id.is_empty() || name.is_empty() {
            return None;
        }
        Some((extension_id, kind, name))
    }

    pub fn parts(&self) -> (&str, HandlerKind, &str) {
        Self::split(&self.0).expect("HandlerId constructors preserve the parsed invariant")
    }
}

impl From<HandlerId> for String {
    fn from(id: HandlerId) -> Self {
        id.0
    }
}

impl Display for HandlerId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for HandlerId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = String::deserialize(deserializer)?;
        Self::parse(&wire)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid handler id {wire:?}")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerKind {
    Tool,
    Hook,
    Command,
    Http,
    Event,
}

impl HandlerKind {
    pub const fn as_str(self) -> &'static str {
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct FeatureName(String);

impl FeatureName {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if valid_feature_name(&value) {
            Ok(Self(value))
        } else {
            Err(format!("invalid feature name {value:?}"))
        }
    }

    pub fn nested_invoke_v1() -> Self {
        Self(FEATURE_NESTED_INVOKE_V1.into())
    }

    pub fn model_stream_v1() -> Self {
        Self(FEATURE_MODEL_STREAM_V1.into())
    }

    pub fn custom_event_v1() -> Self {
        Self(FEATURE_CUSTOM_EVENT_V1.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for FeatureName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl Display for FeatureName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn valid_feature_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PeerInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl PeerInfo {
    pub fn validate(&self) -> Result<(), ErrorPayload> {
        if self.name.is_empty() {
            return Err(ErrorPayload::new(
                WireErrorCode::InvalidRequest,
                "peer name must not be empty",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HandlerInvokeRequest {
    pub handler_id: HandlerId,
    #[serde(default)]
    pub event: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeMsg {
    pub id: String,
    pub protocol_version: String,
    pub host: PeerInfo,
    pub extension_id: String,
    #[serde(default)]
    pub supported_features: Vec<FeatureName>,
    #[serde(default)]
    pub required_features: Vec<FeatureName>,
    #[serde(default)]
    pub host_operations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeOutput {
    pub worker: PeerInfo,
    pub protocol_version: String,
    #[serde(default)]
    pub supported_features: Vec<FeatureName>,
    #[serde(default)]
    pub required_features: Vec<FeatureName>,
    pub negotiated_features: Vec<FeatureName>,
    pub manifest: InitializeManifest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivateMsg {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivateOutput {}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResultKind {
    Initialize,
    Activate,
    Invoke,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResultMsg {
    Success {
        id: String,
        kind: ResultKind,
        output: Value,
    },
    Failure {
        id: String,
        kind: ResultKind,
        error: ErrorPayload,
    },
}

impl ResultMsg {
    pub fn success(id: String, kind: ResultKind, output: Value) -> Self {
        Self::Success { id, kind, output }
    }

    pub fn failure(id: String, kind: ResultKind, error: ErrorPayload) -> Self {
        Self::Failure { id, kind, error }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Success { id, .. } | Self::Failure { id, .. } => id,
        }
    }

    pub const fn kind(&self) -> ResultKind {
        match self {
            Self::Success { kind, .. } | Self::Failure { kind, .. } => *kind,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvokeMsg {
    pub id: String,
    pub operation: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_invoke_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamMsg {
    pub id: String,
    pub event: ModelStreamEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelStreamEvent {
    Started,
    Retrying {
        attempt: u32,
        delay_ms: u64,
    },
    Recovered {
        attempt: u32,
    },
    ContentDelta {
        content: String,
    },
    ThinkingDelta {
        content: String,
    },
    ToolCallStart {
        tool_call_id: String,
        name: String,
        arguments: String,
    },
    ToolCallDelta {
        tool_call_id: String,
        delta: String,
    },
    ToolCallCompleted {
        tool_call_id: String,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
    },
    Completed {
        output: Value,
    },
    Failed {
        error: ErrorPayload,
    },
}

impl ModelStreamEvent {
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed { .. } | Self::Failed { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelMsg {
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WireMessage {
    Initialize(InitializeMsg),
    Activate(ActivateMsg),
    Result(ResultMsg),
    Invoke(InvokeMsg),
    Stream(StreamMsg),
    Cancel(CancelMsg),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
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
    pub fn new(code: WireErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: code.as_str().into(),
            message: message.into(),
            hint: None,
            retryable: false,
            details: None,
        }
    }

    pub fn code_enum(&self) -> Option<WireErrorCode> {
        WireErrorCode::parse(&self.code)
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub const fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }
}

impl fmt::Display for ErrorPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

pub fn encode_wire_message(message: &WireMessage) -> Result<Vec<u8>, ProtocolError> {
    serde_json::to_vec(message).map_err(ProtocolError::Encode)
}

pub fn parse_wire_message(payload: &[u8]) -> Result<WireMessage, ProtocolError> {
    serde_json::from_slice(payload).map_err(ProtocolError::Decode)
}

pub fn negotiate_features(
    local_supported: &BTreeSet<FeatureName>,
    remote_supported: &[FeatureName],
    remote_required: &[FeatureName],
) -> Result<BTreeSet<FeatureName>, ErrorPayload> {
    ensure_unique(remote_supported, "supported_features")?;
    ensure_unique(remote_required, "required_features")?;

    let remote_supported: BTreeSet<_> = remote_supported.iter().cloned().collect();
    for feature in remote_required {
        if !remote_supported.contains(feature) {
            return Err(ErrorPayload::new(
                WireErrorCode::InvalidRequest,
                format!("required feature {feature} is not declared as supported"),
            ));
        }
        if !local_supported.contains(feature) {
            return Err(ErrorPayload::new(
                WireErrorCode::UnsupportedFeature,
                format!("required feature {feature} is not supported by this peer"),
            ));
        }
    }
    Ok(local_supported
        .intersection(&remote_supported)
        .cloned()
        .collect())
}

fn ensure_unique(features: &[FeatureName], field: &str) -> Result<(), ErrorPayload> {
    let unique: BTreeSet<_> = features.iter().collect();
    if unique.len() == features.len() {
        Ok(())
    } else {
        Err(ErrorPayload::new(
            WireErrorCode::InvalidRequest,
            format!("{field} contains duplicate values"),
        ))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("encode S5R message: {0}")]
    Encode(serde_json::Error),
    #[error("decode S5R message: {0}")]
    Decode(serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_negotiation_is_sorted_strict_and_forward_compatible() {
        let local = BTreeSet::from([
            FeatureName::model_stream_v1(),
            FeatureName::nested_invoke_v1(),
        ]);
        let unknown = FeatureName::parse("future_feature_v1").unwrap();
        let negotiated = negotiate_features(
            &local,
            &[unknown, FeatureName::nested_invoke_v1()],
            &[FeatureName::nested_invoke_v1()],
        )
        .unwrap();
        assert_eq!(
            negotiated,
            BTreeSet::from([FeatureName::nested_invoke_v1()])
        );

        let error = negotiate_features(
            &local,
            &[FeatureName::custom_event_v1()],
            &[FeatureName::custom_event_v1()],
        )
        .unwrap_err();
        assert_eq!(error.code_enum(), Some(WireErrorCode::UnsupportedFeature));
    }

    #[test]
    fn envelope_and_nested_payloads_reject_unknown_fields() {
        let cases = [
            br#"{"type":"initialize","id":"1","protocol_version":"3.0","host":{"name":"host"},"extension_id":"worker","host_operations":[],"unknown":true}"#.as_slice(),
            br#"{"type":"initialize","id":"1","protocol_version":"3.0","host":{"name":"host","unknown":true},"extension_id":"worker","host_operations":[]}"#.as_slice(),
            br#"{"type":"activate","id":"1","unknown":true}"#.as_slice(),
            br#"{"type":"cancel","id":"1","reason":"reload","unknown":true}"#.as_slice(),
            br#"{"type":"stream","id":"1","event":{"type":"content_delta","content":"delta","unknown":true}}"#.as_slice(),
        ];
        for payload in cases {
            assert!(parse_wire_message(payload).is_err());
        }

        assert!(
            serde_json::from_value::<InitializeOutput>(serde_json::json!({
                "worker": {"name": "worker"},
                "protocol_version": "3.0",
                "negotiated_features": [],
                "manifest": {"unknown": true}
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ActivateOutput>(serde_json::json!({"unknown": true})).is_err()
        );
    }

    #[test]
    fn unknown_error_codes_are_preserved_losslessly() {
        let error: ErrorPayload = serde_json::from_value(serde_json::json!({
            "code": "future_remote_failure",
            "message": "new peer error"
        }))
        .unwrap();
        assert_eq!(error.code, "future_remote_failure");
        assert_eq!(error.code_enum(), None);
    }
}
