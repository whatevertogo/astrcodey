use std::collections::BTreeMap;

use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    llm::{LlmContent, LlmMessage, LlmRole},
    session::{SessionPhaseDto, SessionToolSelectionDto},
    types::SessionId,
};

pub const HOST_PROCESS_DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const HOST_PROCESS_MAX_TIMEOUT_MS: u64 = 120_000;
pub const HOST_PROCESS_MAX_STDIN_BYTES: usize = 1024 * 1024;
pub const HOST_NETWORK_MAX_BYTES: usize = 10 * 1024 * 1024;
pub const HOST_NETWORK_MAX_REQUEST_BODY_BYTES: usize = 10 * 1024 * 1024;
pub const HOST_NETWORK_DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const HOST_NETWORK_MAX_TIMEOUT_MS: u64 = 60_000;
pub const HOST_SESSION_STATE_KEY_MAX_LENGTH: usize = 128;
pub const HOST_SESSION_STATE_VALUE_MAX_BYTES: usize = 1024 * 1024;
pub const HOST_WORKSPACE_MAX_FILE_BYTES: usize = 1024 * 1024;
pub const HOST_WORKSPACE_LIST_DEFAULT_DEPTH: usize = 1;
pub const HOST_WORKSPACE_LIST_MAX_DEPTH: usize = 32;
pub const HOST_WORKSPACE_LIST_DEFAULT_LIMIT: usize = 500;
pub const HOST_WORKSPACE_LIST_MAX_ENTRIES: usize = 500;
pub const HOST_WORKSPACE_GREP_DEFAULT_MAX_MATCHES: usize = 100;
pub const HOST_WORKSPACE_GREP_DEFAULT_MAX_BYTES: usize = 64 * 1024;
pub const HOST_WORKSPACE_GREP_DEFAULT_MAX_LINE_CHARS: usize = 500;
pub const HOST_WORKSPACE_GLOB_DEFAULT_MAX_MATCHES: usize = 200;
pub const HOST_WORKSPACE_SEARCH_MAX_MATCHES: usize = 1_000;
pub const HOST_WORKSPACE_SEARCH_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
pub const HOST_WORKSPACE_SEARCH_MAX_LINE_CHARS: usize = 2_000;

const HOST_NETWORK_MAX_REQUEST_BODY_WIRE_CHARS: usize =
    HOST_NETWORK_MAX_REQUEST_BODY_BYTES.div_ceil(3) * 4;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostAcknowledgement {
    #[serde(rename = "ok", deserialize_with = "deserialize_true")]
    _ok: bool,
}

impl HostAcknowledgement {
    pub const fn accepted() -> Self {
        Self { _ok: true }
    }

    pub(crate) fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": { "ok": { "const": true } },
            "required": ["ok"],
            "additionalProperties": false
        })
    }
}

fn deserialize_true<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = bool::deserialize(deserializer)?;
    if value {
        Ok(true)
    } else {
        Err(serde::de::Error::custom(
            "acknowledgement `ok` must be true",
        ))
    }
}

/// Typed request used by worker extensions to emit a declared event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostEventEmitRequest {
    #[serde(deserialize_with = "deserialize_non_empty_event_type")]
    pub event_type: String,
    #[serde(deserialize_with = "deserialize_positive_schema_version")]
    pub schema_version: u32,
    pub payload: Value,
}

fn deserialize_non_empty_event_type<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let event_type = String::deserialize(deserializer)?;
    if event_type.is_empty() {
        Err(serde::de::Error::custom("event_type must not be empty"))
    } else {
        Ok(event_type)
    }
}

fn deserialize_positive_schema_version<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let schema_version = u32::deserialize(deserializer)?;
    if schema_version == 0 {
        Err(serde::de::Error::custom(
            "schema_version must be greater than zero",
        ))
    } else {
        Ok(schema_version)
    }
}

impl HostEventEmitRequest {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "event_type": { "type": "string", "minLength": 1 },
                "schema_version": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": u32::MAX
                },
                "payload": {}
            },
            "required": ["event_type", "schema_version", "payload"],
            "additionalProperties": false
        })
    }
}

/// Request for extension-namespaced state in the current session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionStateReadRequest {
    #[serde(deserialize_with = "deserialize_session_state_key")]
    pub key: String,
}

impl HostSessionStateReadRequest {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": { "key": session_state_key_property_schema() },
            "required": ["key"],
            "additionalProperties": false
        })
    }
}

/// Value stored under an extension-namespaced key in the current session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionStateReadOutput {
    pub content: Option<String>,
}

impl HostSessionStateReadOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "oneOf": [
                        { "type": "string" },
                        { "type": "null" }
                    ]
                }
            },
            "required": ["content"],
            "additionalProperties": false
        })
    }
}

/// Write request for extension-namespaced state in the current session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionStateWriteRequest {
    #[serde(deserialize_with = "deserialize_session_state_key")]
    pub key: String,
    #[serde(
        serialize_with = "serialize_session_state_content",
        deserialize_with = "deserialize_session_state_content"
    )]
    pub content: String,
}

impl HostSessionStateWriteRequest {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": session_state_key_property_schema(),
                "content": {
                    "type": "string",
                    "maxLength": HOST_SESSION_STATE_VALUE_MAX_BYTES,
                    "description": "Limited to this many UTF-8 bytes; the host byte limit is authoritative."
                }
            },
            "required": ["key", "content"],
            "additionalProperties": false
        })
    }
}

fn session_state_key_property_schema() -> Value {
    json!({
        "type": "string",
        "minLength": 1,
        "maxLength": HOST_SESSION_STATE_KEY_MAX_LENGTH,
        "pattern": "^[A-Za-z0-9_.-]+$",
        "not": { "enum": [".", ".."] }
    })
}

fn deserialize_session_state_key<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let key = String::deserialize(deserializer)?;
    if valid_session_state_key(&key) {
        Ok(key)
    } else {
        Err(serde::de::Error::custom("invalid session state key"))
    }
}

fn valid_session_state_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= HOST_SESSION_STATE_KEY_MAX_LENGTH
        && !matches!(key, "." | "..")
        && key.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character == '-'
                || character == '_'
                || character == '.'
        })
}

fn serialize_session_state_content<S>(content: &str, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if content.len() > HOST_SESSION_STATE_VALUE_MAX_BYTES {
        return Err(serde::ser::Error::custom(format_args!(
            "session state content exceeds {HOST_SESSION_STATE_VALUE_MAX_BYTES} UTF-8 bytes"
        )));
    }
    serializer.serialize_str(content)
}

fn deserialize_session_state_content<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let content = String::deserialize(deserializer)?;
    if content.len() > HOST_SESSION_STATE_VALUE_MAX_BYTES {
        Err(serde::de::Error::custom(format_args!(
            "session state content exceeds {HOST_SESSION_STATE_VALUE_MAX_BYTES} UTF-8 bytes"
        )))
    } else {
        Ok(content)
    }
}

/// Typed request shared by bundled and worker model clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostLlmChatRequest {
    pub messages: Vec<HostLlmMessage>,
}

impl HostLlmChatRequest {
    pub fn new(messages: Vec<LlmMessage>) -> Self {
        Self {
            messages: messages.into_iter().map(HostLlmMessage::from).collect(),
        }
    }

    pub fn into_messages(self) -> Vec<LlmMessage> {
        self.messages.into_iter().map(LlmMessage::from).collect()
    }

    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "messages": {
                    "type": "array",
                    "items": HostLlmMessage::wire_schema(),
                    "minItems": 1
                }
            },
            "required": ["messages"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostLlmRole {
    System,
    User,
    Assistant,
    Tool,
}

impl From<LlmRole> for HostLlmRole {
    fn from(role: LlmRole) -> Self {
        match role {
            LlmRole::System => Self::System,
            LlmRole::User => Self::User,
            LlmRole::Assistant => Self::Assistant,
            LlmRole::Tool => Self::Tool,
        }
    }
}

impl From<HostLlmRole> for LlmRole {
    fn from(role: HostLlmRole) -> Self {
        match role {
            HostLlmRole::System => Self::System,
            HostLlmRole::User => Self::User,
            HostLlmRole::Assistant => Self::Assistant,
            HostLlmRole::Tool => Self::Tool,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostLlmContent {
    Text {
        text: String,
    },
    Image {
        base64: String,
        media_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
    },
    ToolCall {
        call_id: String,
        name: String,
        arguments: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_arguments: Option<String>,
    },
    ToolResult {
        tool_call_id: String,
        content: String,
        is_error: bool,
    },
}

impl From<LlmContent> for HostLlmContent {
    fn from(content: LlmContent) -> Self {
        match content {
            LlmContent::Text { text } => Self::Text { text },
            LlmContent::Image {
                base64,
                media_type,
                filename,
            } => Self::Image {
                base64,
                media_type,
                filename,
            },
            LlmContent::ToolCall {
                call_id,
                name,
                arguments,
                raw_arguments,
            } => Self::ToolCall {
                call_id,
                name,
                arguments,
                raw_arguments,
            },
            LlmContent::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => Self::ToolResult {
                tool_call_id,
                content,
                is_error,
            },
        }
    }
}

impl From<HostLlmContent> for LlmContent {
    fn from(content: HostLlmContent) -> Self {
        match content {
            HostLlmContent::Text { text } => Self::Text { text },
            HostLlmContent::Image {
                base64,
                media_type,
                filename,
            } => Self::Image {
                base64,
                media_type,
                filename,
            },
            HostLlmContent::ToolCall {
                call_id,
                name,
                arguments,
                raw_arguments,
            } => Self::ToolCall {
                call_id,
                name,
                arguments,
                raw_arguments,
            },
            HostLlmContent::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => Self::ToolResult {
                tool_call_id,
                content,
                is_error,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostLlmMessage {
    pub role: HostLlmRole,
    pub content: Vec<HostLlmContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

impl HostLlmMessage {
    pub fn wire_schema() -> Value {
        host_llm_message_schema()
    }
}

impl From<LlmMessage> for HostLlmMessage {
    fn from(message: LlmMessage) -> Self {
        Self {
            role: message.role.into(),
            content: message
                .content
                .into_iter()
                .map(HostLlmContent::from)
                .collect(),
            name: message.name,
            reasoning_content: message.reasoning_content,
        }
    }
}

impl From<HostLlmMessage> for LlmMessage {
    fn from(message: HostLlmMessage) -> Self {
        Self {
            role: message.role.into(),
            content: message.content.into_iter().map(LlmContent::from).collect(),
            name: message.name,
            reasoning_content: message.reasoning_content,
        }
    }
}

/// Completed non-streaming model response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostLlmChatOutput {
    pub content: String,
    pub model: String,
}

impl HostLlmChatOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": { "type": "string" },
                "model": { "type": "string" }
            },
            "required": ["content", "model"],
            "additionalProperties": false
        })
    }
}

/// One ordered text delta emitted by a model stream.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostLlmTextDelta {
    pub delta: String,
}

impl HostLlmTextDelta {
    fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": { "delta": { "type": "string" } },
            "required": ["delta"],
            "additionalProperties": false
        })
    }
}

/// Explicit collected-stream response used by the current unified host invoker.
///
/// Deltas preserve provider order, but the call completes before this value is returned. The
/// transport may later expose progressive delivery without changing the non-streaming output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostLlmCollectedStreamOutput {
    pub content: String,
    pub model: String,
    pub chunks: Vec<HostLlmTextDelta>,
}

impl HostLlmCollectedStreamOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": { "type": "string" },
                "model": { "type": "string" },
                "chunks": {
                    "type": "array",
                    "items": HostLlmTextDelta::wire_schema()
                }
            },
            "required": ["content", "model", "chunks"],
            "additionalProperties": false
        })
    }
}

pub(crate) fn host_llm_chat_response_schema() -> Value {
    json!({
        "oneOf": [
            HostLlmChatOutput::wire_schema(),
            HostLlmCollectedStreamOutput::wire_schema()
        ]
    })
}

/// Stable summary returned by the narrow session-history domain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionSummary {
    pub session_id: SessionId,
    pub parent_session_id: Option<SessionId>,
    pub source_extension: Option<String>,
    pub working_dir: String,
    pub model_id: String,
    pub created_at: String,
    pub updated_at: String,
    pub latest_cursor: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionSummariesOutput {
    pub sessions: Vec<HostSessionSummary>,
}

impl HostSessionSummariesOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "sessions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "session_id": { "type": "string" },
                            "parent_session_id": { "type": ["string", "null"] },
                            "source_extension": { "type": ["string", "null"] },
                            "working_dir": { "type": "string" },
                            "model_id": { "type": "string" },
                            "created_at": { "type": "string" },
                            "updated_at": { "type": "string" },
                            "latest_cursor": { "type": "string" }
                        },
                        "required": [
                            "session_id",
                            "parent_session_id",
                            "source_extension",
                            "working_dir",
                            "model_id",
                            "created_at",
                            "updated_at",
                            "latest_cursor"
                        ],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["sessions"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionTranscriptMessage {
    pub message: HostLlmMessage,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionTranscript {
    pub session_id: SessionId,
    pub messages: Vec<HostSessionTranscriptMessage>,
}

impl HostSessionTranscript {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string" },
                "messages": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "message": HostLlmMessage::wire_schema(),
                            "source": { "type": ["string", "null"] }
                        },
                        "required": ["message", "source"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["session_id", "messages"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionProviderMessagesOutput {
    pub session_id: SessionId,
    pub messages: Vec<HostLlmMessage>,
}

impl HostSessionProviderMessagesOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string" },
                "messages": {
                    "type": "array",
                    "items": HostLlmMessage::wire_schema()
                }
            },
            "required": ["session_id", "messages"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionTokenUsage {
    pub total_tokens: u64,
    pub model_context_window: Option<usize>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionTokenUsageOutput {
    pub usage: Option<HostSessionTokenUsage>,
}

impl HostSessionTokenUsageOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "usage": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {
                                "total_tokens": { "type": "integer", "minimum": 0 },
                                "model_context_window": { "type": ["integer", "null"], "minimum": 0 }
                            },
                            "required": ["total_tokens", "model_context_window"],
                            "additionalProperties": false
                        },
                        { "type": "null" }
                    ]
                }
            },
            "required": ["usage"],
            "additionalProperties": false
        })
    }
}

/// `astrcode.process.spawn` 的线缆请求。
///
/// `stdin` 最大为 [`HOST_PROCESS_MAX_STDIN_BYTES`] 个 UTF-8 字节，`timeout_ms` 必须位于
/// `1..=HOST_PROCESS_MAX_TIMEOUT_MS`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostProcessRequest {
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_process_stdin",
        deserialize_with = "deserialize_process_stdin"
    )]
    pub stdin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl HostProcessRequest {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            stdin: None,
            timeout_ms: None,
        }
    }

    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "minLength": 1 },
                "args": { "type": "array", "items": { "type": "string" } },
                "cwd": { "type": ["string", "null"] },
                "stdin": {
                    "type": ["string", "null"],
                    "maxLength": HOST_PROCESS_MAX_STDIN_BYTES,
                    "description": "Limited to this many UTF-8 bytes; the host byte limit is authoritative."
                },
                "timeout_ms": {
                    "type": ["integer", "null"],
                    "minimum": 1,
                    "maximum": HOST_PROCESS_MAX_TIMEOUT_MS
                }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }
}

fn serialize_process_stdin<S>(stdin: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if stdin
        .as_ref()
        .is_some_and(|value| value.len() > HOST_PROCESS_MAX_STDIN_BYTES)
    {
        return Err(serde::ser::Error::custom(format_args!(
            "stdin exceeds {HOST_PROCESS_MAX_STDIN_BYTES} UTF-8 bytes"
        )));
    }
    stdin.serialize(serializer)
}

fn deserialize_process_stdin<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let stdin = Option::<String>::deserialize(deserializer)?;
    if stdin
        .as_ref()
        .is_some_and(|value| value.len() > HOST_PROCESS_MAX_STDIN_BYTES)
    {
        Err(serde::de::Error::custom(format_args!(
            "stdin exceeds {HOST_PROCESS_MAX_STDIN_BYTES} UTF-8 bytes"
        )))
    } else {
        Ok(stdin)
    }
}

/// `astrcode.process.spawn` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostProcessOutput {
    pub status: Option<i32>,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub combined: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub combined_truncated: bool,
}

impl HostProcessOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": { "type": ["integer", "null"] },
                "success": { "type": "boolean" },
                "stdout": { "type": "string" },
                "stderr": { "type": "string" },
                "combined": { "type": "string" },
                "stdout_truncated": { "type": "boolean" },
                "stderr_truncated": { "type": "boolean" },
                "combined_truncated": { "type": "boolean" }
            },
            "required": [
                "status",
                "success",
                "stdout",
                "stderr",
                "combined",
                "stdout_truncated",
                "stderr_truncated",
                "combined_truncated"
            ],
            "additionalProperties": false
        })
    }
}

/// Redirect behavior for `astrcode.network.client`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostNetworkRedirectPolicy {
    #[default]
    Follow,
    Manual,
}

/// `astrcode.network.client` 的线缆请求。
///
/// `body` 最大为 [`HOST_NETWORK_MAX_REQUEST_BODY_BYTES`]，`max_bytes` 最大为
/// [`HOST_NETWORK_MAX_BYTES`]，`timeout_ms` 必须位于 `1..=HOST_NETWORK_MAX_TIMEOUT_MS`。
/// `Manual` 重定向仍返回受大小限制的原始响应体。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostNetworkRequest {
    pub url: String,
    #[serde(default = "default_network_method")]
    pub method: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        with = "bounded_request_body"
    )]
    pub body: Vec<u8>,
    #[serde(default = "default_network_max_bytes")]
    pub max_bytes: usize,
    #[serde(default = "default_network_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub redirect_policy: HostNetworkRedirectPolicy,
}

impl HostNetworkRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            method: default_network_method(),
            headers: BTreeMap::new(),
            body: Vec::new(),
            max_bytes: default_network_max_bytes(),
            timeout_ms: default_network_timeout_ms(),
            redirect_policy: HostNetworkRedirectPolicy::default(),
        }
    }

    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" },
                "method": { "type": "string" },
                "headers": { "type": "object", "additionalProperties": { "type": "string" } },
                "body": {
                    "type": "string",
                    "contentEncoding": "base64",
                    "maxLength": HOST_NETWORK_MAX_REQUEST_BODY_WIRE_CHARS,
                    "description": "Decoded body is byte-limited; the host decoded-byte limit is authoritative."
                },
                "max_bytes": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": HOST_NETWORK_MAX_BYTES
                },
                "timeout_ms": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": HOST_NETWORK_MAX_TIMEOUT_MS
                },
                "redirect_policy": { "type": "string", "enum": ["follow", "manual"] }
            },
            "required": ["url"],
            "additionalProperties": false
        })
    }
}

fn default_network_method() -> String {
    "GET".into()
}

const fn default_network_max_bytes() -> usize {
    HOST_NETWORK_MAX_BYTES
}

const fn default_network_timeout_ms() -> u64 {
    HOST_NETWORK_DEFAULT_TIMEOUT_MS
}

/// `astrcode.network.client` 的线缆响应。
///
/// `body` 在线缆上使用 base64，作者 API 始终接收原始字节。`headers` 不保留同名响应头
/// 的重复值。宿主限制全局共享并发，但线缆协议不承诺 extension 级公平配额。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostNetworkResponse {
    /// 完成所有受限重定向后的最终 URL。
    pub final_url: String,
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    #[serde(with = "base64_bytes")]
    pub body: Vec<u8>,
}

impl HostNetworkResponse {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "final_url": { "type": "string" },
                "status": { "type": "integer", "minimum": 0, "maximum": u16::MAX },
                "headers": { "type": "object", "additionalProperties": { "type": "string" } },
                "body": { "type": "string", "contentEncoding": "base64" }
            },
            "required": ["final_url", "status", "headers", "body"],
            "additionalProperties": false
        })
    }
}

mod base64_bytes {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    use super::STANDARD;

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        decode(&encoded).map_err(D::Error::custom)
    }

    pub(super) fn decode(encoded: &str) -> Result<Vec<u8>, base64::DecodeError> {
        STANDARD.decode(encoded)
    }
}

mod bounded_request_body {
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _, ser::Error as _};

    use super::{
        HOST_NETWORK_MAX_REQUEST_BODY_BYTES, HOST_NETWORK_MAX_REQUEST_BODY_WIRE_CHARS, base64_bytes,
    };

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if bytes.len() > HOST_NETWORK_MAX_REQUEST_BODY_BYTES {
            return Err(S::Error::custom(format_args!(
                "network request body exceeds {HOST_NETWORK_MAX_REQUEST_BODY_BYTES} bytes"
            )));
        }
        base64_bytes::serialize(bytes, serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        if encoded.len() > HOST_NETWORK_MAX_REQUEST_BODY_WIRE_CHARS {
            return Err(D::Error::custom(format_args!(
                "encoded network request body exceeds {HOST_NETWORK_MAX_REQUEST_BODY_WIRE_CHARS} \
                 characters"
            )));
        }
        let bytes = base64_bytes::decode(&encoded).map_err(D::Error::custom)?;
        if bytes.len() > HOST_NETWORK_MAX_REQUEST_BODY_BYTES {
            return Err(D::Error::custom(format_args!(
                "network request body exceeds {HOST_NETWORK_MAX_REQUEST_BODY_BYTES} bytes"
            )));
        }
        Ok(bytes)
    }
}

/// `astrcode.workspace.read` 的线缆请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceReadRequest {
    pub path: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_workspace_read_max_bytes"
    )]
    pub max_bytes: Option<u64>,
}

impl HostWorkspaceReadRequest {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "minLength": 1 },
                "max_bytes": {
                    "type": ["integer", "null"],
                    "minimum": 0,
                    "maximum": HOST_WORKSPACE_MAX_FILE_BYTES,
                    "default": HOST_WORKSPACE_MAX_FILE_BYTES
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }
}

fn deserialize_workspace_read_max_bytes<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<u64>::deserialize(deserializer)?;
    match value {
        Some(value) if value > HOST_WORKSPACE_MAX_FILE_BYTES as u64 => {
            Err(serde::de::Error::custom(format_args!(
                "max_bytes must not exceed {HOST_WORKSPACE_MAX_FILE_BYTES}"
            )))
        },
        value => Ok(value),
    }
}

/// `astrcode.workspace.read` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceReadOutput {
    pub content: String,
}

impl HostWorkspaceReadOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": { "content": { "type": "string" } },
            "required": ["content"],
            "additionalProperties": false
        })
    }
}

/// `astrcode.workspace.write` 的线缆请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceWriteRequest {
    pub path: String,
    pub content: String,
}

impl HostWorkspaceWriteRequest {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "minLength": 1 },
                "content": { "type": "string" }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }
}

/// `astrcode.workspace.write` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceWriteOutput {
    pub path: String,
    pub bytes_written: usize,
    pub parent_created: bool,
}

impl HostWorkspaceWriteOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "bytes_written": { "type": "integer", "minimum": 0 },
                "parent_created": { "type": "boolean" }
            },
            "required": ["path", "bytes_written", "parent_created"],
            "additionalProperties": false
        })
    }
}

/// `astrcode.workspace.edit` 的线缆请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceEditRequest {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    #[serde(default)]
    pub replace_all: bool,
}

impl HostWorkspaceEditRequest {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "minLength": 1 },
                "old_text": { "type": "string", "minLength": 1 },
                "new_text": { "type": "string" },
                "replace_all": { "type": "boolean" }
            },
            "required": ["path", "old_text", "new_text"],
            "additionalProperties": false
        })
    }
}

/// `astrcode.workspace.edit` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceEditOutput {
    pub path: String,
    pub replacements: usize,
    pub bytes_written: usize,
}

impl HostWorkspaceEditOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "replacements": { "type": "integer", "minimum": 0 },
                "bytes_written": { "type": "integer", "minimum": 0 }
            },
            "required": ["path", "replacements", "bytes_written"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceListRequest {
    pub path: String,
    #[serde(
        default = "default_workspace_list_depth",
        deserialize_with = "deserialize_workspace_list_depth"
    )]
    pub depth: usize,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_workspace_list_limit"
    )]
    pub limit: Option<usize>,
}

impl HostWorkspaceListRequest {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "depth": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": HOST_WORKSPACE_LIST_MAX_DEPTH,
                    "default": HOST_WORKSPACE_LIST_DEFAULT_DEPTH
                },
                "limit": {
                    "type": ["integer", "null"],
                    "minimum": 1,
                    "maximum": HOST_WORKSPACE_LIST_MAX_ENTRIES,
                    "default": HOST_WORKSPACE_LIST_DEFAULT_LIMIT
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }
}

const fn default_workspace_list_depth() -> usize {
    HOST_WORKSPACE_LIST_DEFAULT_DEPTH
}

fn deserialize_workspace_list_depth<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_usize(deserializer, 1, HOST_WORKSPACE_LIST_MAX_DEPTH, "depth")
}

fn deserialize_workspace_list_limit<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_optional_bounded_usize(deserializer, 1, HOST_WORKSPACE_LIST_MAX_ENTRIES, "limit")
}

fn deserialize_workspace_search_max_matches<'de, D>(
    deserializer: D,
) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_optional_bounded_usize(
        deserializer,
        1,
        HOST_WORKSPACE_SEARCH_MAX_MATCHES,
        "max_matches",
    )
}

fn deserialize_workspace_search_max_bytes<'de, D>(
    deserializer: D,
) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_optional_bounded_usize(
        deserializer,
        1,
        HOST_WORKSPACE_SEARCH_MAX_OUTPUT_BYTES,
        "max_bytes",
    )
}

fn deserialize_workspace_search_max_line_chars<'de, D>(
    deserializer: D,
) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_optional_bounded_usize(
        deserializer,
        1,
        HOST_WORKSPACE_SEARCH_MAX_LINE_CHARS,
        "max_line_chars",
    )
}

fn deserialize_bounded_usize<'de, D>(
    deserializer: D,
    min: usize,
    max: usize,
    field: &str,
) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = usize::deserialize(deserializer)?;
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(format_args!(
            "{field} must be between {min} and {max}"
        )))
    }
}

fn deserialize_optional_bounded_usize<'de, D>(
    deserializer: D,
    min: usize,
    max: usize,
    field: &str,
) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<usize>::deserialize(deserializer)?;
    match value {
        Some(value) if !(min..=max).contains(&value) => Err(serde::de::Error::custom(
            format_args!("{field} must be between {min} and {max}"),
        )),
        value => Ok(value),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceListEntry {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceListOutput {
    pub path: String,
    pub entries: Vec<HostWorkspaceListEntry>,
    pub returned_entries: usize,
    pub truncated: bool,
}

impl HostWorkspaceListOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "entries": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "path": { "type": "string" },
                            "kind": { "type": "string" },
                            "bytes": { "type": ["integer", "null"], "minimum": 0 }
                        },
                        "required": ["name", "path", "kind", "bytes"],
                        "additionalProperties": false
                    }
                },
                "returned_entries": { "type": "integer", "minimum": 0 },
                "truncated": { "type": "boolean" }
            },
            "required": ["path", "entries", "returned_entries", "truncated"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceGrepRequest {
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_workspace_search_max_matches"
    )]
    pub max_matches: Option<usize>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_workspace_search_max_bytes"
    )]
    pub max_bytes: Option<usize>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_workspace_search_max_line_chars"
    )]
    pub max_line_chars: Option<usize>,
}

impl HostWorkspaceGrepRequest {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "minLength": 1 },
                "path": { "type": ["string", "null"] },
                "max_matches": {
                    "type": ["integer", "null"],
                    "minimum": 1,
                    "maximum": HOST_WORKSPACE_SEARCH_MAX_MATCHES,
                    "default": HOST_WORKSPACE_GREP_DEFAULT_MAX_MATCHES
                },
                "max_bytes": {
                    "type": ["integer", "null"],
                    "minimum": 1,
                    "maximum": HOST_WORKSPACE_SEARCH_MAX_OUTPUT_BYTES,
                    "default": HOST_WORKSPACE_GREP_DEFAULT_MAX_BYTES
                },
                "max_line_chars": {
                    "type": ["integer", "null"],
                    "minimum": 1,
                    "maximum": HOST_WORKSPACE_SEARCH_MAX_LINE_CHARS,
                    "default": HOST_WORKSPACE_GREP_DEFAULT_MAX_LINE_CHARS
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceGrepMatch {
    pub path: String,
    pub line_number: usize,
    pub line: String,
    pub line_truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceGrepOutput {
    pub pattern: String,
    pub root: String,
    pub matches: Vec<HostWorkspaceGrepMatch>,
    pub truncated: bool,
}

impl HostWorkspaceGrepOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "root": { "type": "string" },
                "matches": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" },
                            "line_number": { "type": "integer", "minimum": 1 },
                            "line": { "type": "string" },
                            "line_truncated": { "type": "boolean" }
                        },
                        "required": ["path", "line_number", "line", "line_truncated"],
                        "additionalProperties": false
                    }
                },
                "truncated": { "type": "boolean" }
            },
            "required": ["pattern", "root", "matches", "truncated"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceGlobRequest {
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_workspace_search_max_matches"
    )]
    pub max_matches: Option<usize>,
    #[serde(default)]
    pub include_ignored: bool,
}

impl HostWorkspaceGlobRequest {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "minLength": 1 },
                "root": { "type": ["string", "null"] },
                "max_matches": {
                    "type": ["integer", "null"],
                    "minimum": 1,
                    "maximum": HOST_WORKSPACE_SEARCH_MAX_MATCHES,
                    "default": HOST_WORKSPACE_GLOB_DEFAULT_MAX_MATCHES
                },
                "include_ignored": { "type": "boolean" }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceGlobOutput {
    pub pattern: String,
    pub root: String,
    pub paths: Vec<String>,
    pub truncated: bool,
}

impl HostWorkspaceGlobOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "root": { "type": "string" },
                "paths": { "type": "array", "items": { "type": "string" } },
                "truncated": { "type": "boolean" }
            },
            "required": ["pattern", "root", "paths", "truncated"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionInputRequest {
    pub target_session_id: String,
    pub content: String,
}

impl HostSessionInputRequest {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "target_session_id": { "type": "string", "minLength": 1 },
                "content": { "type": "string", "minLength": 1 }
            },
            "required": ["target_session_id", "content"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostSessionDeliveryOutput {
    Started { turn_id: String },
    Injected { turn_id: String },
    Queued { queue_len: usize },
}

impl HostSessionDeliveryOutput {
    pub fn wire_schema() -> Value {
        json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "status": { "const": "started" },
                        "turn_id": { "type": "string" }
                    },
                    "required": ["status", "turn_id"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "status": { "const": "injected" },
                        "turn_id": { "type": "string" }
                    },
                    "required": ["status", "turn_id"],
                    "additionalProperties": false
                },
                {
                    "type": "object",
                    "properties": {
                        "status": { "const": "queued" },
                        "queue_len": { "type": "integer", "minimum": 0 }
                    },
                    "required": ["status", "queue_len"],
                    "additionalProperties": false
                }
            ]
        })
    }
}

/// Result of idempotently requesting cancellation of the active turn.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionCancelOutput {
    pub cancelled: bool,
}

impl HostSessionCancelOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": { "cancelled": { "type": "boolean" } },
            "required": ["cancelled"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionExecutionView {
    pub phase: SessionPhaseDto,
    pub active_turn_id: Option<String>,
    pub queued_inputs: usize,
}

impl HostSessionExecutionView {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "phase": SessionPhaseDto::wire_schema(),
                "active_turn_id": { "type": ["string", "null"] },
                "queued_inputs": { "type": "integer", "minimum": 0 }
            },
            "required": ["phase", "active_turn_id", "queued_inputs"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostConfigureSessionToolsRequest {
    pub session_id: String,
    pub selection: SessionToolSelectionDto,
}

impl HostConfigureSessionToolsRequest {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "minLength": 1 },
                "selection": SessionToolSelectionDto::wire_schema(
                    "Session tool visibility for subsequent turns."
                )
            },
            "required": ["session_id", "selection"],
            "additionalProperties": false
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostConfigureSessionToolsOutput {
    pub selection: SessionToolSelectionDto,
}

impl HostConfigureSessionToolsOutput {
    pub fn wire_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "selection": SessionToolSelectionDto::wire_schema(
                    "Effective session tool visibility for subsequent turns."
                )
            },
            "required": ["selection"],
            "additionalProperties": false
        })
    }
}

fn host_llm_message_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "role": { "type": "string", "enum": ["system", "user", "assistant", "tool"] },
            "content": {
                "type": "array",
                "items": {
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
                                "media_type": { "type": "string" },
                                "filename": { "type": ["string", "null"] }
                            },
                            "required": ["type", "base64", "media_type"],
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "properties": {
                                "type": { "const": "tool_call" },
                                "call_id": { "type": "string" },
                                "name": { "type": "string" },
                                "arguments": {},
                                "raw_arguments": { "type": ["string", "null"] }
                            },
                            "required": ["type", "call_id", "name", "arguments"],
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "properties": {
                                "type": { "const": "tool_result" },
                                "tool_call_id": { "type": "string" },
                                "content": { "type": "string" },
                                "is_error": { "type": "boolean" }
                            },
                            "required": ["type", "tool_call_id", "content", "is_error"],
                            "additionalProperties": false
                        }
                    ]
                }
            },
            "name": { "type": ["string", "null"] },
            "reasoning_content": { "type": ["string", "null"] }
        },
        "required": ["role", "content"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use serde::{Serialize, de::DeserializeOwned};

    use super::*;

    fn assert_strict_round_trip<T>(contract: T)
    where
        T: Serialize + DeserializeOwned,
    {
        let type_name = std::any::type_name::<T>();
        let wire = serde_json::to_value(&contract)
            .unwrap_or_else(|error| panic!("failed to serialize {type_name}: {error}"));
        let decoded = serde_json::from_value::<T>(wire.clone())
            .unwrap_or_else(|error| panic!("failed to deserialize {type_name}: {error}"));
        assert_eq!(
            serde_json::to_value(decoded).unwrap(),
            wire,
            "{type_name} did not round-trip"
        );

        let mut unknown_field = wire;
        let Value::Object(fields) = &mut unknown_field else {
            panic!("{type_name} must serialize as an object");
        };
        fields.insert("unexpected".into(), json!(true));
        assert!(
            serde_json::from_value::<T>(unknown_field).is_err(),
            "{type_name} accepted an unknown top-level field"
        );
    }

    fn assert_closed_object_schema(schema: Value) {
        if let Some(variants) = schema.get("oneOf").and_then(Value::as_array) {
            for variant in variants {
                assert_eq!(variant["additionalProperties"], false, "{variant}");
            }
            return;
        }
        assert_eq!(schema["additionalProperties"], false, "{schema}");
    }

    macro_rules! assert_contracts {
        ($($contract:expr),+ $(,)?) => {
            $(assert_strict_round_trip($contract);)+
        };
    }

    #[test]
    fn host_client_contracts_are_strict_and_round_trip() {
        let session_id = || crate::types::SessionId::new("session-1");

        for schema in [
            HostAcknowledgement::wire_schema(),
            HostEventEmitRequest::wire_schema(),
            HostSessionStateReadRequest::wire_schema(),
            HostSessionStateReadOutput::wire_schema(),
            HostSessionStateWriteRequest::wire_schema(),
            HostLlmChatRequest::wire_schema(),
            HostLlmChatOutput::wire_schema(),
            HostLlmCollectedStreamOutput::wire_schema(),
            HostSessionSummariesOutput::wire_schema(),
            HostSessionTranscript::wire_schema(),
            HostSessionProviderMessagesOutput::wire_schema(),
            HostSessionTokenUsageOutput::wire_schema(),
            HostProcessRequest::wire_schema(),
            HostProcessOutput::wire_schema(),
            HostNetworkRequest::wire_schema(),
            HostNetworkResponse::wire_schema(),
            HostWorkspaceReadRequest::wire_schema(),
            HostWorkspaceReadOutput::wire_schema(),
            HostWorkspaceWriteRequest::wire_schema(),
            HostWorkspaceWriteOutput::wire_schema(),
            HostWorkspaceEditRequest::wire_schema(),
            HostWorkspaceEditOutput::wire_schema(),
            HostWorkspaceListRequest::wire_schema(),
            HostWorkspaceListOutput::wire_schema(),
            HostWorkspaceGrepRequest::wire_schema(),
            HostWorkspaceGrepOutput::wire_schema(),
            HostWorkspaceGlobRequest::wire_schema(),
            HostWorkspaceGlobOutput::wire_schema(),
            HostSessionInputRequest::wire_schema(),
            HostSessionDeliveryOutput::wire_schema(),
            HostSessionCancelOutput::wire_schema(),
            HostSessionExecutionView::wire_schema(),
            HostConfigureSessionToolsRequest::wire_schema(),
            HostConfigureSessionToolsOutput::wire_schema(),
        ] {
            assert_closed_object_schema(schema);
        }

        assert_contracts!(
            HostAcknowledgement::accepted(),
            HostEventEmitRequest {
                event_type: "review.completed".into(),
                schema_version: 1,
                payload: json!({ "status": "ok" }),
            },
            HostSessionStateReadRequest { key: "goal".into() },
            HostSessionStateReadOutput {
                content: Some("active".into()),
            },
            HostSessionStateReadOutput { content: None },
            HostSessionStateWriteRequest {
                key: "goal".into(),
                content: "active".into(),
            },
            HostLlmChatRequest::new(vec![LlmMessage::user("hello")]),
            HostLlmChatOutput {
                content: "hello".into(),
                model: "main".into(),
            },
            HostLlmCollectedStreamOutput {
                content: "hello".into(),
                model: "main".into(),
                chunks: vec![HostLlmTextDelta {
                    delta: "hello".into(),
                }],
            },
            HostSessionSummariesOutput {
                sessions: vec![HostSessionSummary {
                    session_id: session_id(),
                    parent_session_id: None,
                    source_extension: Some("example".into()),
                    working_dir: "/workspace".into(),
                    model_id: "main".into(),
                    created_at: "2026-08-06T00:00:00Z".into(),
                    updated_at: "2026-08-06T00:00:01Z".into(),
                    latest_cursor: "7".into(),
                }],
            },
            HostSessionTranscript {
                session_id: session_id(),
                messages: vec![HostSessionTranscriptMessage {
                    message: LlmMessage::user("hello").into(),
                    source: Some("user".into()),
                }],
            },
            HostSessionProviderMessagesOutput {
                session_id: session_id(),
                messages: vec![LlmMessage::user("hello").into()],
            },
            HostSessionTokenUsageOutput {
                usage: Some(HostSessionTokenUsage {
                    total_tokens: 42,
                    model_context_window: Some(128_000),
                }),
            },
            HostProcessRequest {
                command: "printf".into(),
                args: vec!["hello".into()],
                cwd: Some(".".into()),
                stdin: Some("input".into()),
                timeout_ms: Some(1_000),
            },
            HostProcessOutput {
                status: Some(0),
                success: true,
                stdout: "hello".into(),
                stderr: String::new(),
                combined: "hello".into(),
                stdout_truncated: false,
                stderr_truncated: false,
                combined_truncated: false,
            },
            HostNetworkRequest {
                url: "https://example.com".into(),
                method: "POST".into(),
                headers: BTreeMap::from([("content-type".into(), "text/plain".into())]),
                body: b"hello".to_vec(),
                max_bytes: 4_096,
                timeout_ms: 1_000,
                redirect_policy: HostNetworkRedirectPolicy::Manual,
            },
            HostNetworkResponse {
                final_url: "https://example.com/final".into(),
                status: 200,
                headers: BTreeMap::from([("content-type".into(), "text/plain".into())]),
                body: b"hello".to_vec(),
            },
            HostWorkspaceReadRequest {
                path: "notes.txt".into(),
                max_bytes: Some(4_096),
            },
            HostWorkspaceReadOutput {
                content: "hello".into(),
            },
            HostWorkspaceWriteRequest {
                path: "notes.txt".into(),
                content: "hello".into(),
            },
            HostWorkspaceWriteOutput {
                path: "notes.txt".into(),
                bytes_written: 5,
                parent_created: false,
            },
            HostWorkspaceEditRequest {
                path: "notes.txt".into(),
                old_text: "hello".into(),
                new_text: "hi".into(),
                replace_all: true,
            },
            HostWorkspaceEditOutput {
                path: "notes.txt".into(),
                replacements: 1,
                bytes_written: 2,
            },
            HostWorkspaceListRequest {
                path: ".".into(),
                depth: 2,
                limit: Some(20),
            },
            HostWorkspaceListOutput {
                path: ".".into(),
                entries: vec![HostWorkspaceListEntry {
                    name: "notes.txt".into(),
                    path: "notes.txt".into(),
                    kind: "file".into(),
                    bytes: Some(5),
                }],
                returned_entries: 1,
                truncated: false,
            },
            HostWorkspaceGrepRequest {
                pattern: "hello".into(),
                path: Some("notes.txt".into()),
                max_matches: Some(10),
                max_bytes: Some(4_096),
                max_line_chars: Some(200),
            },
            HostWorkspaceGrepOutput {
                pattern: "hello".into(),
                root: ".".into(),
                matches: vec![HostWorkspaceGrepMatch {
                    path: "notes.txt".into(),
                    line_number: 1,
                    line: "hello".into(),
                    line_truncated: false,
                }],
                truncated: false,
            },
            HostWorkspaceGlobRequest {
                pattern: "**/*.txt".into(),
                root: Some(".".into()),
                max_matches: Some(10),
                include_ignored: true,
            },
            HostWorkspaceGlobOutput {
                pattern: "**/*.txt".into(),
                root: ".".into(),
                paths: vec!["notes.txt".into()],
                truncated: false,
            },
            HostSessionInputRequest {
                target_session_id: "session-1".into(),
                content: "continue".into(),
            },
            HostSessionDeliveryOutput::Started {
                turn_id: "turn-1".into(),
            },
            HostSessionDeliveryOutput::Injected {
                turn_id: "turn-1".into(),
            },
            HostSessionDeliveryOutput::Queued { queue_len: 2 },
            HostSessionCancelOutput { cancelled: true },
            HostSessionExecutionView {
                phase: SessionPhaseDto::Streaming,
                active_turn_id: Some("turn-1".into()),
                queued_inputs: 2,
            },
            HostConfigureSessionToolsRequest {
                session_id: "session-1".into(),
                selection: SessionToolSelectionDto::no_tools(),
            },
            HostConfigureSessionToolsOutput {
                selection: SessionToolSelectionDto::no_tools(),
            },
        );
    }

    #[test]
    fn unit_and_context_contracts_reject_invalid_shapes() {
        for value in [
            json!({}),
            json!({ "ok": false }),
            json!({ "ok": true, "extra": 1 }),
        ] {
            assert!(
                serde_json::from_value::<HostAcknowledgement>(value.clone()).is_err(),
                "accepted invalid acknowledgement: {value}"
            );
        }
        assert!(serde_json::from_value::<HostAcknowledgement>(json!({ "ok": true })).is_ok());

        for value in [
            json!({ "event_type": "review.completed", "payload": {} }),
            json!({ "event_type": "review.completed", "schema_version": 1 }),
            json!({ "event_type": "", "schema_version": 1, "payload": {} }),
            json!({ "event_type": "review.completed", "schema_version": 0, "payload": {} }),
            json!({
                "event_type": "review.completed",
                "schema_version": u64::from(u32::MAX) + 1,
                "payload": {}
            }),
            json!({
                "event_type": "review.completed",
                "schema_version": 1,
                "payload": {},
                "extra": true
            }),
        ] {
            assert!(
                serde_json::from_value::<HostEventEmitRequest>(value.clone()).is_err(),
                "accepted invalid event emit request: {value}"
            );
        }

        for value in [
            json!({}),
            json!({ "key": "goal", "content": "active", "extra": true }),
        ] {
            assert!(
                serde_json::from_value::<HostSessionStateWriteRequest>(value.clone()).is_err(),
                "accepted invalid session state write: {value}"
            );
        }

        let invalid_keys = [
            String::new(),
            ".".into(),
            "..".into(),
            "nested/key".into(),
            "x".repeat(HOST_SESSION_STATE_KEY_MAX_LENGTH + 1),
        ];
        for key in invalid_keys {
            assert!(
                serde_json::from_value::<HostSessionStateReadRequest>(json!({ "key": key }))
                    .is_err(),
                "accepted invalid session state key"
            );
        }

        let key_schema = &HostSessionStateReadRequest::wire_schema()["properties"]["key"];
        assert_eq!(key_schema["maxLength"], HOST_SESSION_STATE_KEY_MAX_LENGTH);
        assert_eq!(key_schema["not"]["enum"], json!([".", ".."]))
    }

    #[test]
    fn workspace_request_bounds_match_schema_and_serde() {
        assert!(
            serde_json::from_value::<HostWorkspaceReadRequest>(json!({
                "path": "notes.txt",
                "max_bytes": HOST_WORKSPACE_MAX_FILE_BYTES as u64 + 1
            }))
            .is_err()
        );
        for max_bytes in [0, HOST_WORKSPACE_MAX_FILE_BYTES as u64] {
            let request: HostWorkspaceReadRequest = serde_json::from_value(json!({
                "path": "notes.txt",
                "max_bytes": max_bytes
            }))
            .unwrap();
            assert_eq!(request.max_bytes, Some(max_bytes));
        }

        for value in [
            json!({ "path": ".", "depth": 0 }),
            json!({ "path": ".", "depth": HOST_WORKSPACE_LIST_MAX_DEPTH + 1 }),
            json!({ "path": ".", "limit": 0 }),
            json!({ "path": ".", "limit": HOST_WORKSPACE_LIST_MAX_ENTRIES + 1 }),
        ] {
            assert!(
                serde_json::from_value::<HostWorkspaceListRequest>(value.clone()).is_err(),
                "accepted invalid workspace list request: {value}"
            );
        }
        for value in [
            json!({ "pattern": "x", "max_matches": 0 }),
            json!({
                "pattern": "x",
                "max_matches": HOST_WORKSPACE_SEARCH_MAX_MATCHES + 1
            }),
            json!({ "pattern": "x", "max_bytes": 0 }),
            json!({
                "pattern": "x",
                "max_bytes": HOST_WORKSPACE_SEARCH_MAX_OUTPUT_BYTES + 1
            }),
            json!({ "pattern": "x", "max_line_chars": 0 }),
            json!({
                "pattern": "x",
                "max_line_chars": HOST_WORKSPACE_SEARCH_MAX_LINE_CHARS + 1
            }),
        ] {
            assert!(
                serde_json::from_value::<HostWorkspaceGrepRequest>(value.clone()).is_err(),
                "accepted invalid workspace grep request: {value}"
            );
        }
        for value in [
            json!({ "pattern": "*.rs", "max_matches": 0 }),
            json!({
                "pattern": "*.rs",
                "max_matches": HOST_WORKSPACE_SEARCH_MAX_MATCHES + 1
            }),
        ] {
            assert!(
                serde_json::from_value::<HostWorkspaceGlobRequest>(value.clone()).is_err(),
                "accepted invalid workspace glob request: {value}"
            );
        }

        let list: HostWorkspaceListRequest = serde_json::from_value(json!({
            "path": ".",
            "depth": HOST_WORKSPACE_LIST_MAX_DEPTH,
            "limit": HOST_WORKSPACE_LIST_MAX_ENTRIES
        }))
        .unwrap();
        assert_eq!(list.depth, HOST_WORKSPACE_LIST_MAX_DEPTH);
        assert_eq!(list.limit, Some(HOST_WORKSPACE_LIST_MAX_ENTRIES));
        let grep: HostWorkspaceGrepRequest = serde_json::from_value(json!({
            "pattern": "x",
            "max_matches": HOST_WORKSPACE_SEARCH_MAX_MATCHES,
            "max_bytes": HOST_WORKSPACE_SEARCH_MAX_OUTPUT_BYTES,
            "max_line_chars": HOST_WORKSPACE_SEARCH_MAX_LINE_CHARS
        }))
        .unwrap();
        assert_eq!(grep.max_matches, Some(HOST_WORKSPACE_SEARCH_MAX_MATCHES));
        assert_eq!(grep.max_bytes, Some(HOST_WORKSPACE_SEARCH_MAX_OUTPUT_BYTES));
        assert_eq!(
            grep.max_line_chars,
            Some(HOST_WORKSPACE_SEARCH_MAX_LINE_CHARS)
        );
        let glob: HostWorkspaceGlobRequest = serde_json::from_value(json!({
            "pattern": "*.rs",
            "max_matches": null
        }))
        .unwrap();
        assert_eq!(glob.max_matches, None);

        let list_schema = HostWorkspaceListRequest::wire_schema();
        assert_eq!(
            HostWorkspaceReadRequest::wire_schema()["properties"]["max_bytes"]["maximum"],
            HOST_WORKSPACE_MAX_FILE_BYTES
        );
        assert_eq!(
            HostWorkspaceReadRequest::wire_schema()["properties"]["max_bytes"]["default"],
            HOST_WORKSPACE_MAX_FILE_BYTES
        );
        assert_eq!(
            list_schema["properties"]["depth"]["maximum"],
            HOST_WORKSPACE_LIST_MAX_DEPTH
        );
        assert_eq!(
            list_schema["properties"]["limit"]["maximum"],
            HOST_WORKSPACE_LIST_MAX_ENTRIES
        );
        assert_eq!(
            list_schema["properties"]["depth"]["default"],
            HOST_WORKSPACE_LIST_DEFAULT_DEPTH
        );
        assert_eq!(
            list_schema["properties"]["limit"]["default"],
            HOST_WORKSPACE_LIST_DEFAULT_LIMIT
        );
        let grep_schema = HostWorkspaceGrepRequest::wire_schema();
        assert_eq!(
            grep_schema["properties"]["max_matches"]["maximum"],
            HOST_WORKSPACE_SEARCH_MAX_MATCHES
        );
        assert_eq!(
            grep_schema["properties"]["max_bytes"]["maximum"],
            HOST_WORKSPACE_SEARCH_MAX_OUTPUT_BYTES
        );
        assert_eq!(
            grep_schema["properties"]["max_line_chars"]["maximum"],
            HOST_WORKSPACE_SEARCH_MAX_LINE_CHARS
        );
        assert_eq!(
            grep_schema["properties"]["max_matches"]["default"],
            HOST_WORKSPACE_GREP_DEFAULT_MAX_MATCHES
        );
        assert_eq!(
            grep_schema["properties"]["max_bytes"]["default"],
            HOST_WORKSPACE_GREP_DEFAULT_MAX_BYTES
        );
        assert_eq!(
            grep_schema["properties"]["max_line_chars"]["default"],
            HOST_WORKSPACE_GREP_DEFAULT_MAX_LINE_CHARS
        );
        assert_eq!(
            HostWorkspaceGlobRequest::wire_schema()["properties"]["max_matches"]["maximum"],
            HOST_WORKSPACE_SEARCH_MAX_MATCHES
        );
        assert_eq!(
            HostWorkspaceGlobRequest::wire_schema()["properties"]["max_matches"]["default"],
            HOST_WORKSPACE_GLOB_DEFAULT_MAX_MATCHES
        );
    }

    #[test]
    fn bounded_payload_contracts_enforce_byte_limits() {
        let mut process = HostProcessRequest::new("printf");
        process.stdin = Some("x".repeat(HOST_PROCESS_MAX_STDIN_BYTES));
        assert!(serde_json::to_value(&process).is_ok());
        process.stdin = Some("x".repeat(HOST_PROCESS_MAX_STDIN_BYTES + 1));
        assert!(serde_json::to_value(&process).is_err());
        for stdin in [
            "x".repeat(HOST_PROCESS_MAX_STDIN_BYTES + 1),
            "é".repeat(HOST_PROCESS_MAX_STDIN_BYTES / 2 + 1),
        ] {
            assert!(
                serde_json::from_value::<HostProcessRequest>(json!({
                    "command": "printf",
                    "stdin": stdin
                }))
                .is_err()
            );
        }

        let max_state_content = "x".repeat(HOST_SESSION_STATE_VALUE_MAX_BYTES);
        assert!(
            serde_json::from_value::<HostSessionStateWriteRequest>(json!({
                "key": "goal",
                "content": max_state_content
            }))
            .is_ok()
        );
        for content in [
            "x".repeat(HOST_SESSION_STATE_VALUE_MAX_BYTES + 1),
            "é".repeat(HOST_SESSION_STATE_VALUE_MAX_BYTES / 2 + 1),
        ] {
            assert!(
                serde_json::from_value::<HostSessionStateWriteRequest>(json!({
                    "key": "goal",
                    "content": content
                }))
                .is_err()
            );
        }
        assert!(
            serde_json::to_value(HostSessionStateWriteRequest {
                key: "goal".into(),
                content: "x".repeat(HOST_SESSION_STATE_VALUE_MAX_BYTES + 1),
            })
            .is_err()
        );

        let mut network = HostNetworkRequest::get("https://example.com");
        network.body = vec![0; HOST_NETWORK_MAX_REQUEST_BODY_BYTES];
        assert!(serde_json::to_value(&network).is_ok());
        network.body.push(0);
        assert!(serde_json::to_value(&network).is_err());

        let encoded_over_limit =
            base64::Engine::encode(&STANDARD, vec![0; HOST_NETWORK_MAX_REQUEST_BODY_BYTES + 1]);
        assert!(encoded_over_limit.len() <= HOST_NETWORK_MAX_REQUEST_BODY_WIRE_CHARS);
        assert!(
            serde_json::from_value::<HostNetworkRequest>(json!({
                "url": "https://example.com",
                "body": encoded_over_limit
            }))
            .is_err()
        );

        assert_eq!(
            HostProcessRequest::wire_schema()["properties"]["stdin"]["maxLength"],
            HOST_PROCESS_MAX_STDIN_BYTES
        );
        assert_eq!(
            HostSessionStateWriteRequest::wire_schema()["properties"]["content"]["maxLength"],
            HOST_SESSION_STATE_VALUE_MAX_BYTES
        );
        assert_eq!(
            HostNetworkRequest::wire_schema()["properties"]["body"]["maxLength"],
            HOST_NETWORK_MAX_REQUEST_BODY_WIRE_CHARS
        );
    }

    #[test]
    fn host_llm_wire_mapping_is_lossless_and_nested_strict() {
        let message = LlmMessage {
            role: LlmRole::Assistant,
            content: vec![
                LlmContent::Text {
                    text: "hello".into(),
                },
                LlmContent::Image {
                    base64: "aW1hZ2U=".into(),
                    media_type: "image/png".into(),
                    filename: Some("image.png".into()),
                },
                LlmContent::ToolCall {
                    call_id: "call-1".into(),
                    name: "read".into(),
                    arguments: json!({ "path": "notes.txt" }),
                    raw_arguments: Some("{path:notes.txt}".into()),
                },
                LlmContent::ToolResult {
                    tool_call_id: "call-1".into(),
                    content: "hello".into(),
                    is_error: false,
                },
            ],
            name: Some("assistant".into()),
            reasoning_content: Some("reasoning".into()),
        };
        let wire_message = HostLlmMessage::from(message.clone());
        assert_eq!(LlmMessage::from(wire_message.clone()), message);
        assert_eq!(
            HostLlmChatRequest::new(vec![message.clone()]).into_messages(),
            vec![message]
        );

        let valid = serde_json::to_value(wire_message).unwrap();
        let mut explicit_nulls = valid.clone();
        explicit_nulls["name"] = Value::Null;
        explicit_nulls["reasoning_content"] = Value::Null;
        explicit_nulls["content"][1]["filename"] = Value::Null;
        explicit_nulls["content"][2]["raw_arguments"] = Value::Null;
        assert!(serde_json::from_value::<HostLlmMessage>(explicit_nulls).is_ok());

        let schema = HostLlmMessage::wire_schema();
        assert_eq!(
            schema["properties"]["name"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(
            schema["properties"]["reasoning_content"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(
            schema["properties"]["content"]["items"]["oneOf"][1]["properties"]["filename"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(
            schema["properties"]["content"]["items"]["oneOf"][2]["properties"]["raw_arguments"]
                ["type"],
            json!(["string", "null"])
        );

        for pointer in ["", "/content/0", "/content/1", "/content/2", "/content/3"] {
            let mut invalid = valid.clone();
            let object = if pointer.is_empty() {
                invalid.as_object_mut()
            } else {
                invalid.pointer_mut(pointer).and_then(Value::as_object_mut)
            }
            .unwrap();
            object.insert("unexpected".into(), Value::Bool(true));
            assert!(
                serde_json::from_value::<HostLlmMessage>(invalid).is_err(),
                "HostLlmMessage accepted an unknown field at {pointer}"
            );
        }
    }

    #[test]
    fn session_delivery_rejects_invalid_tag_shapes() {
        let invalid = [
            json!({ "turn_id": "turn-1" }),
            json!({ "status": "unknown", "turn_id": "turn-1" }),
            json!({ "status": "started" }),
            json!({ "status": "injected" }),
            json!({ "status": "queued" }),
            json!({ "status": "started", "turn_id": "turn-1", "queue_len": 1 }),
            json!({ "status": "injected", "turn_id": "turn-1", "queue_len": 1 }),
            json!({ "status": "queued", "queue_len": 1, "turn_id": "turn-1" }),
        ];

        for value in invalid {
            assert!(
                serde_json::from_value::<HostSessionDeliveryOutput>(value.clone()).is_err(),
                "accepted invalid session delivery output: {value}"
            );
        }
    }
}
