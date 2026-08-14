//! workspace 线缆契约与边界常量。

use serde::{Deserialize, Serialize};

pub const HOST_WORKSPACE_MAX_FILE_BYTES: usize = 10 * 1024 * 1024;
pub const HOST_WORKSPACE_MAX_IMAGE_BYTES: usize = 3 * 1024 * 1024;
pub const HOST_WORKSPACE_MAX_TEXT_OUTPUT_BYTES: usize = 1024 * 1024;
pub const HOST_WORKSPACE_MAX_LINE_LIMIT: usize = 100_000;
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
pub const HOST_WORKSPACE_SEARCH_MAX_CONTEXT_LINES: usize = 20;
pub const HOST_WORKSPACE_MAX_PATCH_BYTES: usize = 1024 * 1024;
pub const HOST_WORKSPACE_MAX_DIFF_BYTES: usize = 64 * 1024;

use super::{
    deserialize_bounded_usize, deserialize_bounded_utf8, deserialize_optional_bounded_usize,
    deserialize_optional_bounded_utf8, serialize_bounded_utf8,
};

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
    #[serde(default)]
    pub line_offset: usize,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_workspace_read_line_limit"
    )]
    pub line_limit: Option<usize>,
}

impl HostWorkspaceReadRequest {
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            max_bytes: None,
            line_offset: 0,
            line_limit: None,
        }
    }
}

bounded_usize_deserializer!(
    deserialize_workspace_read_line_limit,
    Option<usize>,
    HOST_WORKSPACE_MAX_LINE_LIMIT,
    "line_limit"
);

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
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostWorkspaceReadOutput {
    Text {
        content: String,
        bytes: usize,
        total_lines: usize,
        line_offset: usize,
        returned_lines: usize,
        has_more_lines: bool,
    },
    Image {
        media_type: String,
        data_base64: String,
        bytes: usize,
    },
    Binary {
        bytes: usize,
    },
}

/// `astrcode.workspace.write` 的线缆请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceWriteRequest {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub create_dirs: bool,
}

/// `astrcode.workspace.write` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceWriteOutput {
    pub path: String,
    pub created: bool,
    pub change: HostWorkspaceTextChange,
}

/// A bounded, presentation-neutral summary of one committed text-file mutation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceTextChange {
    pub old_bytes: Option<u64>,
    pub new_bytes: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_workspace_diff",
        deserialize_with = "deserialize_workspace_diff"
    )]
    pub unified_diff: Option<String>,
    pub insertions: usize,
    pub deletions: usize,
    pub diff_truncated: bool,
}

bounded_utf8_serde_fns!(
    serialize_workspace_diff,
    deserialize_workspace_diff,
    Option<String>,
    HOST_WORKSPACE_MAX_DIFF_BYTES,
    "unified_diff"
);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceApplyPatchRequest {
    #[serde(
        serialize_with = "serialize_workspace_patch",
        deserialize_with = "deserialize_workspace_patch"
    )]
    pub patch: String,
}

bounded_utf8_serde_fns!(
    serialize_workspace_patch,
    deserialize_workspace_patch,
    String,
    HOST_WORKSPACE_MAX_PATCH_BYTES,
    "patch"
);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostWorkspacePatchChangeKind {
    Created,
    Updated,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspacePatchChange {
    pub kind: HostWorkspacePatchChangeKind,
    pub path: String,
    pub applied: bool,
    pub summary: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceApplyPatchOutput {
    pub changes: Vec<HostWorkspacePatchChange>,
}

/// `astrcode.workspace.edit` 的线缆请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceEditRequest {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_text: Option<String>,
    #[serde(default)]
    pub replace_all: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edits: Vec<HostWorkspaceTextEdit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceTextEdit {
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
    pub operation_count: usize,
    pub replacements: usize,
    pub change: HostWorkspaceTextChange,
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
    #[serde(default)]
    pub offset: usize,
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
    #[serde(default = "default_true")]
    pub recursive: bool,
    #[serde(default)]
    pub multiline: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_filters: Vec<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_workspace_search_context_lines"
    )]
    pub before_context: usize,
    #[serde(
        default,
        deserialize_with = "deserialize_workspace_search_context_lines"
    )]
    pub after_context: usize,
    #[serde(default)]
    pub mode: HostWorkspaceGrepMode,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostWorkspaceGrepMode {
    Content,
    #[default]
    FilesWithMatches,
    Count,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostWorkspaceGrepEntry {
    Content {
        path: String,
        line_number: usize,
        line: String,
        line_truncated: bool,
        before_context: Vec<HostWorkspaceGrepContextLine>,
        after_context: Vec<HostWorkspaceGrepContextLine>,
    },
    File {
        path: String,
    },
    Count {
        path: String,
        count: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceGrepContextLine {
    pub line_number: usize,
    pub line: String,
    pub line_truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceGrepOutput {
    pub pattern: String,
    pub root: String,
    pub entries: Vec<HostWorkspaceGrepEntry>,
    pub has_more: bool,
    pub scan_truncated: bool,
    pub skipped_files: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceGlobRequest {
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default)]
    pub offset: usize,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_workspace_search_max_matches"
    )]
    pub max_matches: Option<usize>,
    #[serde(default = "default_true")]
    pub respect_gitignore: bool,
    #[serde(default = "default_true")]
    pub include_hidden: bool,
    #[serde(default = "default_true")]
    pub include_directories: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostWorkspaceGlobOutput {
    pub pattern: String,
    pub root: String,
    pub paths: Vec<String>,
    pub total_matches: Option<usize>,
    pub has_more: bool,
    pub scan_truncated: bool,
}

const fn default_true() -> bool {
    true
}

fn deserialize_workspace_search_context_lines<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = usize::deserialize(deserializer)?;
    if value <= HOST_WORKSPACE_SEARCH_MAX_CONTEXT_LINES {
        Ok(value)
    } else {
        Err(serde::de::Error::custom(format_args!(
            "context lines must not exceed {HOST_WORKSPACE_SEARCH_MAX_CONTEXT_LINES}"
        )))
    }
}
