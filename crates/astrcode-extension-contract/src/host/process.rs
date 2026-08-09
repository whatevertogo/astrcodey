//! process 线缆契约与边界常量。

use serde::{Deserialize, Serialize};

pub const HOST_PROCESS_DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const HOST_PROCESS_MAX_TIMEOUT_MS: u64 = 120_000;
pub const HOST_PROCESS_MAX_STDIN_BYTES: usize = 1024 * 1024;

use super::{deserialize_optional_bounded_utf8, serialize_bounded_utf8};

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
}

bounded_utf8_serde_fns!(
    serialize_process_stdin,
    deserialize_process_stdin,
    Option<String>,
    HOST_PROCESS_MAX_STDIN_BYTES,
    "stdin"
);

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
