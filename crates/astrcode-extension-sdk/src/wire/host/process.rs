//! Process wire contracts and boundary constants.

use serde::{Deserialize, Serialize};

pub const HOST_PROCESS_DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const HOST_PROCESS_MAX_TIMEOUT_MS: u64 = 600_000;
pub const HOST_PROCESS_MAX_STDIN_BYTES: usize = 1024 * 1024;
pub const HOST_PROCESS_MAX_WAIT_MS: u64 = 600_000;

use super::{deserialize_bounded_utf8, deserialize_optional_bounded_utf8, serialize_bounded_utf8};

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

/// Wire response for `astrcode.process.spawn`.
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

/// Lifetime owner for a process handle.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostProcessLifetime {
    /// Terminate when the current tool invocation is cancelled.
    Call,
    /// Remain available until explicitly killed or the owning session/extension is closed.
    #[default]
    Session,
}

/// Starts a process that remains owned by the current session and extension.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostProcessStartRequest {
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub lifetime: HostProcessLifetime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl HostProcessStartRequest {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            lifetime: HostProcessLifetime::Session,
            timeout_ms: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostProcessHandleOutput {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostProcessTargetRequest {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostProcessReadRequest {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostProcessReadOutput {
    pub id: String,
    pub stdout: String,
    pub stderr: String,
    pub combined: String,
    pub dropped_bytes: usize,
    pub state: HostProcessState,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostProcessState {
    Running {},
    Exited { status: Option<i32> },
    TimedOut { status: Option<i32> },
    Killed { status: Option<i32> },
    Cancelled { status: Option<i32> },
}

impl Default for HostProcessState {
    fn default() -> Self {
        Self::Running {}
    }
}

impl HostProcessState {
    pub const fn is_running(self) -> bool {
        matches!(self, Self::Running {})
    }

    pub const fn status(self) -> Option<i32> {
        match self {
            Self::Running {} => None,
            Self::Exited { status }
            | Self::TimedOut { status }
            | Self::Killed { status }
            | Self::Cancelled { status } => status,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostProcessInputRequest {
    pub id: String,
    pub action: HostProcessInputAction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostProcessInputAction {
    Write {
        #[serde(
            serialize_with = "serialize_process_input",
            deserialize_with = "deserialize_process_input"
        )]
        input: String,
    },
    Close,
}

bounded_utf8_serde_fns!(
    serialize_process_input,
    deserialize_process_input,
    String,
    HOST_PROCESS_MAX_STDIN_BYTES,
    "input"
);

impl HostProcessInputRequest {
    pub fn write(id: impl Into<String>, input: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            action: HostProcessInputAction::Write {
                input: input.into(),
            },
        }
    }

    pub fn close(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            action: HostProcessInputAction::Close,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostProcessStatusOutput {
    pub id: String,
    pub state: HostProcessState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostProcessListOutput {
    pub processes: Vec<HostProcessStatusOutput>,
}
