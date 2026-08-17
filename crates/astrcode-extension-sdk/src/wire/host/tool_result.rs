//! Session-scoped persisted tool-result wire contract.

use serde::{Deserialize, Serialize};

use super::deserialize_bounded_usize;

pub const HOST_TOOL_RESULT_DEFAULT_MAX_BYTES: usize = 20_000;
pub const HOST_TOOL_RESULT_MIN_BYTES: usize = 4;
pub const HOST_TOOL_RESULT_MAX_BYTES: usize = 20_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostToolResultReadRequest {
    pub artifact_id: String,
    #[serde(default)]
    pub byte_offset: usize,
    #[serde(
        default = "default_tool_result_max_bytes",
        deserialize_with = "deserialize_tool_result_max_bytes"
    )]
    pub max_bytes: usize,
}

const fn default_tool_result_max_bytes() -> usize {
    HOST_TOOL_RESULT_DEFAULT_MAX_BYTES
}

fn deserialize_tool_result_max_bytes<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_usize(
        deserializer,
        HOST_TOOL_RESULT_MIN_BYTES,
        HOST_TOOL_RESULT_MAX_BYTES,
        "max_bytes",
    )
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostToolResultReadOutput {
    pub artifact_id: String,
    pub bytes: usize,
    pub byte_offset: usize,
    pub returned_bytes: usize,
    pub next_byte_offset: Option<usize>,
    pub has_more: bool,
    pub content: String,
}
