//! session state 线缆契约与边界常量。


use serde::{Deserialize, Serialize};

pub const HOST_SESSION_STATE_KEY_MAX_LENGTH: usize = 128;
pub const HOST_SESSION_STATE_VALUE_MAX_BYTES: usize = 1024 * 1024;


use super::{deserialize_bounded_utf8, serialize_bounded_utf8};

/// Request for extension-namespaced state in the current session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionStateReadRequest {
    #[serde(deserialize_with = "deserialize_session_state_key")]
    pub key: String,
}

/// Value stored under an extension-namespaced key in the current session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostSessionStateReadOutput {
    pub content: Option<String>,
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

bounded_utf8_serde_fns!(
    serialize_session_state_content,
    deserialize_session_state_content,
    String,
    HOST_SESSION_STATE_VALUE_MAX_BYTES,
    "session state content"
);
