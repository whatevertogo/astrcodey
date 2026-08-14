use std::{collections::BTreeMap, time::Instant};

use astrcode_extension_sdk::{
    extension::{ExtensionCall, ExtensionError, ToolContext, ToolHandler, ToolPlanContext},
    host::{
        HostProcessIo, HostProcessLifetime, HostProcessReadRequest, HostProcessResizeRequest,
        HostProcessStartRequest, HostProcessTargetRequest,
    },
    tool::{
        ExecutionMode, HostResource, ToolDefinition, ToolExecutionResult, ToolOrigin, ToolPlan,
    },
};
use serde::Deserialize;

use crate::result::success;

const DEFAULT_READ_WAIT_MS: u64 = 100;
const MAX_READ_WAIT_MS: u64 = 10_000;
const TERMINAL_TIMEOUT_MS: u64 = 600_000;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TerminalArgs {
    action: TerminalAction,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    wait_ms: Option<u64>,
    #[serde(default)]
    rows: Option<u16>,
    #[serde(default)]
    cols: Option<u16>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TerminalAction {
    Start,
    Send,
    Read,
    Resize,
    Close,
    List,
}

pub(super) struct TerminalHandler;

#[async_trait::async_trait]
impl ToolHandler for TerminalHandler {
    async fn plan(&self, context: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        let args: TerminalArgs = context.arguments()?;
        validate(&args)?;
        Ok(ToolPlan::host(HostResource::Process))
    }

    async fn execute(&self, context: ToolContext) -> Result<ToolExecutionResult, ExtensionError> {
        let started_at = Instant::now();
        let args: TerminalArgs = context.arguments()?;
        validate(&args)?;
        let process = context.host().process()?;
        let result = match args.action {
            TerminalAction::Start => {
                let command = required(args.command, "start requires command")?;
                let mut request = HostProcessStartRequest::pty(command.clone());
                request.args = args.args;
                request.cwd = args.cwd;
                request.io = HostProcessIo::Pty {
                    rows: args.rows.unwrap_or(24),
                    cols: args.cols.unwrap_or(80),
                };
                request.lifetime = HostProcessLifetime::Session;
                request.timeout_ms = Some(TERMINAL_TIMEOUT_MS);
                let output = process.start(request).await?;
                success(
                    started_at,
                    format!("terminal started: {}", output.id),
                    BTreeMap::from([
                        ("id".into(), serde_json::json!(output.id)),
                        ("command".into(), serde_json::json!(command)),
                    ]),
                )
            },
            TerminalAction::Send => {
                let id = required(args.id, "send requires id")?;
                let input = required(args.input, "send requires input")?;
                process.write(id.clone(), input.clone()).await?;
                success(
                    started_at,
                    "sent",
                    BTreeMap::from([
                        ("id".into(), serde_json::json!(id)),
                        ("bytesSent".into(), serde_json::json!(input.len())),
                    ]),
                )
            },
            TerminalAction::Read => {
                let id = required(args.id, "read requires id")?;
                let output = process
                    .read(HostProcessReadRequest {
                        id,
                        wait_ms: Some(args.wait_ms.unwrap_or(DEFAULT_READ_WAIT_MS)),
                    })
                    .await?;
                success(
                    started_at,
                    output.combined,
                    BTreeMap::from([
                        ("id".into(), serde_json::json!(output.id)),
                        ("alive".into(), serde_json::json!(output.state.is_running())),
                        ("exitCode".into(), serde_json::json!(output.state.status())),
                        ("state".into(), serde_json::json!(output.state)),
                        (
                            "droppedBytes".into(),
                            serde_json::json!(output.dropped_bytes),
                        ),
                    ]),
                )
            },
            TerminalAction::Resize => {
                let id = required(args.id, "resize requires id")?;
                let rows = required(args.rows, "resize requires rows")?;
                let cols = required(args.cols, "resize requires cols")?;
                process
                    .resize(HostProcessResizeRequest {
                        id: id.clone(),
                        rows,
                        cols,
                    })
                    .await?;
                success(
                    started_at,
                    "resized",
                    BTreeMap::from([
                        ("id".into(), serde_json::json!(id)),
                        ("rows".into(), serde_json::json!(rows)),
                        ("cols".into(), serde_json::json!(cols)),
                    ]),
                )
            },
            TerminalAction::Close => {
                let id = required(args.id, "close requires id")?;
                process
                    .kill(HostProcessTargetRequest { id: id.clone() })
                    .await?;
                success(
                    started_at,
                    "closed",
                    BTreeMap::from([("id".into(), serde_json::json!(id))]),
                )
            },
            TerminalAction::List => {
                let output = process.list().await?;
                let ids = output
                    .processes
                    .iter()
                    .filter(|process| matches!(process.io, HostProcessIo::Pty { .. }))
                    .map(|process| process.id.clone())
                    .collect::<Vec<_>>();
                let content = if ids.is_empty() {
                    "no active terminals".into()
                } else {
                    ids.join("\n")
                };
                success(
                    started_at,
                    content,
                    BTreeMap::from([
                        ("count".into(), serde_json::json!(ids.len())),
                        ("terminals".into(), serde_json::json!(ids)),
                    ]),
                )
            },
        };
        Ok(result.into())
    }
}

fn validate(args: &TerminalArgs) -> Result<(), ExtensionError> {
    if args.wait_ms.is_some_and(|wait| wait > MAX_READ_WAIT_MS) {
        return Err(invalid("waitMs must not exceed 10000"));
    }
    let has_id = args.id.as_deref().is_some_and(|id| !id.trim().is_empty());
    match args.action {
        TerminalAction::Start
            if args
                .command
                .as_deref()
                .is_none_or(|command| command.trim().is_empty()) =>
        {
            Err(invalid("start requires command"))
        },
        TerminalAction::Start if args.rows == Some(0) || args.cols == Some(0) => {
            Err(invalid("start rows and cols must be greater than zero"))
        },
        TerminalAction::Send if !has_id || args.input.is_none() => {
            Err(invalid("send requires id and input"))
        },
        TerminalAction::Read | TerminalAction::Close if !has_id => {
            Err(invalid("action requires id"))
        },
        TerminalAction::Resize
            if !has_id
                || args.rows.is_none_or(|value| value == 0)
                || args.cols.is_none_or(|value| value == 0) =>
        {
            Err(invalid(
                "resize requires id, rows, and cols greater than zero",
            ))
        },
        _ => Ok(()),
    }
}

fn required<T>(value: Option<T>, message: &'static str) -> Result<T, ExtensionError> {
    value.ok_or_else(|| invalid(message))
}

fn invalid(message: impl Into<String>) -> ExtensionError {
    ExtensionError::InvalidInput {
        code: astrcode_extension_sdk::WireErrorCode::InvalidInput
            .as_str()
            .into(),
        message: message.into(),
        hint: Some("follow the terminal start/send/read/resize/close/list lifecycle".into()),
    }
}

pub(super) fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "terminal".into(),
        description: concat!(
            "Manages session-owned PTY processes for interactive REPLs and debuggers.\n\n",
            "Lifecycle: start → send/read/resize → close. list shows this extension's PTYs.\n",
            "Always close terminals when finished; session shutdown also cleans them up."
        )
        .into(),
        strict: true,
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Sequential,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["start", "send", "read", "resize", "close", "list"] },
                "id": { "type": "string" },
                "command": { "type": "string" },
                "args": { "type": "array", "items": { "type": "string" } },
                "cwd": { "type": "string" },
                "input": { "type": "string" },
                "waitMs": { "type": "integer", "minimum": 0, "maximum": 10000 },
                "rows": { "type": "integer", "minimum": 1, "maximum": 65535 },
                "cols": { "type": "integer", "minimum": 1, "maximum": 65535 }
            },
            "required": ["action"],
            "additionalProperties": false
        }),
    }
}
