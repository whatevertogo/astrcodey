//! network 线缆契约与边界常量。

use base64::engine::general_purpose::STANDARD;

pub const HOST_NETWORK_MAX_BYTES: usize = 10 * 1024 * 1024;
pub const HOST_NETWORK_MAX_REQUEST_BODY_BYTES: usize = 10 * 1024 * 1024;
pub const HOST_NETWORK_DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const HOST_NETWORK_MAX_TIMEOUT_MS: u64 = 60_000;
pub(crate) const HOST_NETWORK_MAX_REQUEST_BODY_WIRE_CHARS: usize =
    HOST_NETWORK_MAX_REQUEST_BODY_BYTES.div_ceil(3) * 4;


use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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
