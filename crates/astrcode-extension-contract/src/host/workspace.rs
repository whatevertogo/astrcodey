//! workspace 线缆契约与边界常量。

use serde::{Deserialize, Serialize};

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


use super::{deserialize_bounded_usize, deserialize_optional_bounded_usize};

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

/// `astrcode.workspace.write` 的线缆请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceWriteRequest {
    pub path: String,
    pub content: String,
}

/// `astrcode.workspace.write` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceWriteOutput {
    pub path: String,
    pub bytes_written: usize,
    pub parent_created: bool,
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

/// `astrcode.workspace.edit` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceEditOutput {
    pub path: String,
    pub replacements: usize,
    pub bytes_written: usize,
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

const fn default_workspace_list_depth() -> usize {
    HOST_WORKSPACE_LIST_DEFAULT_DEPTH
}

bounded_usize_deserializer!(
    deserialize_workspace_list_depth,
    usize,
    HOST_WORKSPACE_LIST_MAX_DEPTH,
    "depth"
);

bounded_usize_deserializer!(
    deserialize_workspace_list_limit,
    Option<usize>,
    HOST_WORKSPACE_LIST_MAX_ENTRIES,
    "limit"
);

bounded_usize_deserializer!(
    deserialize_workspace_search_max_matches,
    Option<usize>,
    HOST_WORKSPACE_SEARCH_MAX_MATCHES,
    "max_matches"
);

bounded_usize_deserializer!(
    deserialize_workspace_search_max_bytes,
    Option<usize>,
    HOST_WORKSPACE_SEARCH_MAX_OUTPUT_BYTES,
    "max_bytes"
);

bounded_usize_deserializer!(
    deserialize_workspace_search_max_line_chars,
    Option<usize>,
    HOST_WORKSPACE_SEARCH_MAX_LINE_CHARS,
    "max_line_chars"
);


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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceGlobOutput {
    pub pattern: String,
    pub root: String,
    pub paths: Vec<String>,
    pub truncated: bool,
}
