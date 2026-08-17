//! `astrcode.event.emit` 线缆契约。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::deserialize_non_empty_string;

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

/// Publication state returned by the host after an extension event emit request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "status",
    content = "publication",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum HostEventEmitOutput {
    Accepted,
    LivePublished { event_id: String },
    Persisted { event_id: String, seq: u64 },
}

/// 共享的非空字符串校验核心，`deserialize_with` 需要无参函数路径，故各字段保留薄包装。
fn deserialize_non_empty_event_type<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_non_empty_string(deserializer, "event_type")
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
