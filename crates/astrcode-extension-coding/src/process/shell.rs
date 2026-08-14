use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use astrcode_extension_sdk::{
    extension::{ExtensionCall, ExtensionError, ToolContext, ToolHandler, ToolPlanContext},
    host::{
        HostProcessLifetime, HostProcessReadOutput, HostProcessReadRequest,
        HostProcessStartRequest, HostProcessState, HostProcessTargetRequest,
    },
    shell::{ShellFamily, ShellInfo, resolve_shell},
    tool::{ExecutionMode, ToolDefinition, ToolExecutionResult, ToolOrigin, ToolPlan, ToolResult},
};
use serde::Deserialize;

use crate::result::{completed_error, success};

const AUTO_BACKGROUND_AFTER_MS: u64 = 30_000;
const MAX_TIMEOUT_SECS: u64 = 600;
const MAX_FOREGROUND_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Default)]
struct BoundedOutput {
    content: String,
    dropped_bytes: usize,
}

struct ProcessPresentation {
    id: String,
    content: String,
    stdout_bytes: usize,
    stderr_bytes: usize,
    dropped_bytes: usize,
    state: HostProcessState,
}

impl From<HostProcessReadOutput> for ProcessPresentation {
    fn from(output: HostProcessReadOutput) -> Self {
        Self {
            id: output.id,
            content: output.combined,
            stdout_bytes: output.stdout.len(),
            stderr_bytes: output.stderr.len(),
            dropped_bytes: output.dropped_bytes,
            state: output.state,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PipelineSemantics {
    enforced: bool,
}

impl BoundedOutput {
    fn push(&mut self, chunk: &str) {
        self.content.push_str(chunk);
        if self.content.len() <= MAX_FOREGROUND_OUTPUT_BYTES {
            return;
        }

        let mut discard = self.content.len() - MAX_FOREGROUND_OUTPUT_BYTES;
        while !self.content.is_char_boundary(discard) {
            discard += 1;
        }
        self.content.drain(..discard);
        self.dropped_bytes = self.dropped_bytes.saturating_add(discard);
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ShellArgs {
    #[serde(default)]
    command: String,
    #[serde(default)]
    intent: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    stdin: Option<String>,
    #[serde(default)]
    run_in_background: bool,
    #[serde(default)]
    shell_id: Option<String>,
    #[serde(default)]
    block_until_ms: Option<u64>,
}

pub(super) struct ShellHandler {
    default_timeout_secs: Arc<AtomicU64>,
}

impl ShellHandler {
    pub(super) fn new(default_timeout_secs: Arc<AtomicU64>) -> Self {
        Self {
            default_timeout_secs,
        }
    }
}

#[async_trait::async_trait]
impl ToolHandler for ShellHandler {
    async fn plan(&self, context: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        let args: ShellArgs = context.arguments()?;
        validate(&args)?;
        Ok(ToolPlan::host(
            astrcode_extension_sdk::tool::HostResource::Process,
        ))
    }

    async fn execute(&self, context: ToolContext) -> Result<ToolExecutionResult, ExtensionError> {
        let started_at = Instant::now();
        let args: ShellArgs = context.arguments()?;
        validate(&args)?;
        let process = context.host().process()?;

        if let Some(id) = normalized_id(args.shell_id.as_deref()) {
            let output = process
                .read(HostProcessReadRequest {
                    id: id.to_owned(),
                    wait_ms: Some(args.block_until_ms.unwrap_or(0)),
                })
                .await?;
            return Ok(render_process_result(started_at, output, args.intent).into());
        }

        let shell = resolve_shell();
        let (command, pipeline_semantics) = prepare_shell_command(&shell, &args.command)?;
        let display_cwd = args
            .cwd
            .clone()
            .unwrap_or_else(|| context.working_dir().display().to_string());
        let mut request = HostProcessStartRequest::pipes(shell.path.clone());
        request.args = shell_args(shell.family, &command);
        request.cwd = args.cwd.clone();
        let timeout_secs = args
            .timeout
            .unwrap_or_else(|| self.default_timeout_secs.load(Ordering::Acquire));
        request.timeout_ms = Some(timeout_secs * 1_000);
        request.lifetime = if args.run_in_background {
            HostProcessLifetime::Session
        } else {
            HostProcessLifetime::Call
        };
        let handle = process.start(request).await?;
        if let Some(input) = &args.stdin {
            process.write(handle.id.clone(), input.clone()).await?;
        }
        process.close_stdin(handle.id.clone()).await?;

        if args.run_in_background {
            let mut result = background_result(
                started_at,
                handle.id,
                args.intent.clone(),
                String::new(),
                0,
                0,
                0,
            );
            add_invocation_metadata(
                &mut result,
                &args,
                &shell,
                &display_cwd,
                timeout_secs,
                pipeline_semantics,
            );
            return Ok(result.into());
        }

        let deadline = Instant::now() + std::time::Duration::from_millis(AUTO_BACKGROUND_AFTER_MS);
        let mut combined = BoundedOutput::default();
        let mut stdout_bytes = 0usize;
        let mut stderr_bytes = 0usize;
        let mut dropped_bytes = 0usize;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                process
                    .promote(HostProcessTargetRequest {
                        id: handle.id.clone(),
                    })
                    .await?;
                let mut result = background_result(
                    started_at,
                    handle.id,
                    args.intent.clone(),
                    combined.content,
                    stdout_bytes,
                    stderr_bytes,
                    dropped_bytes.saturating_add(combined.dropped_bytes),
                );
                add_invocation_metadata(
                    &mut result,
                    &args,
                    &shell,
                    &display_cwd,
                    timeout_secs,
                    pipeline_semantics,
                );
                return Ok(result.into());
            }
            let output = process
                .read(HostProcessReadRequest {
                    id: handle.id.clone(),
                    wait_ms: Some(remaining.as_millis().min(1_000) as u64),
                })
                .await?;
            stdout_bytes = stdout_bytes.saturating_add(output.stdout.len());
            stderr_bytes = stderr_bytes.saturating_add(output.stderr.len());
            dropped_bytes = dropped_bytes.saturating_add(output.dropped_bytes);
            combined.push(&output.combined);
            if !output.state.is_running() {
                let mut result = render_completed(
                    started_at,
                    ProcessPresentation {
                        id: output.id,
                        content: combined.content,
                        stdout_bytes,
                        stderr_bytes,
                        dropped_bytes: dropped_bytes.saturating_add(combined.dropped_bytes),
                        state: output.state,
                    },
                    args.intent.clone(),
                );
                add_invocation_metadata(
                    &mut result,
                    &args,
                    &shell,
                    &display_cwd,
                    timeout_secs,
                    pipeline_semantics,
                );
                return Ok(result.into());
            }
        }
    }
}

fn normalized_id(id: Option<&str>) -> Option<&str> {
    id.map(str::trim).filter(|id| !id.is_empty())
}

fn validate(args: &ShellArgs) -> Result<(), ExtensionError> {
    let shell_id = normalized_id(args.shell_id.as_deref());
    if shell_id.is_some() {
        if !args.command.trim().is_empty() {
            return Err(invalid("cannot specify both shellId and command"));
        }
        if args.run_in_background {
            return Err(invalid("cannot specify both shellId and runInBackground"));
        }
        if args.cwd.is_some() || args.timeout.is_some() || args.stdin.is_some() {
            return Err(invalid(
                "cwd, timeout, and stdin apply only when starting a command",
            ));
        }
    } else if args.command.trim().is_empty() {
        return Err(invalid("command cannot be empty"));
    } else if args.block_until_ms.is_some() {
        return Err(invalid("blockUntilMs requires an existing shellId"));
    }
    if args
        .timeout
        .is_some_and(|timeout| !(1..=MAX_TIMEOUT_SECS).contains(&timeout))
    {
        return Err(invalid("timeout must be between 1 and 600 seconds"));
    }
    if args
        .block_until_ms
        .is_some_and(|wait| wait > astrcode_extension_sdk::host::HOST_PROCESS_MAX_WAIT_MS)
    {
        return Err(invalid("blockUntilMs must not exceed 600000"));
    }
    Ok(())
}

fn prepare_shell_command(
    shell: &ShellInfo,
    command: &str,
) -> Result<(String, PipelineSemantics), ExtensionError> {
    let command = preprocess_shell_command(command, shell);
    let has_pipeline = has_pipeline_operator(shell, &command);
    let pipefail = supports_pipefail(shell);
    if has_pipeline && !pipefail {
        return Err(invalid(format!(
            "strict pipeline status cannot be enforced by shell '{}'; run without a pipeline or \
             select bash/zsh",
            shell.name
        )));
    }
    let command = if pipefail {
        format!("set -o pipefail\n{command}")
    } else {
        command
    };
    Ok((
        command,
        PipelineSemantics {
            enforced: !has_pipeline || pipefail,
        },
    ))
}

fn supports_pipefail(shell: &ShellInfo) -> bool {
    if shell.family == ShellFamily::Wsl {
        return true;
    }
    if shell.family != ShellFamily::Posix {
        return false;
    }
    let executable = shell
        .name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(shell.name.as_str())
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    executable == "bash"
        || executable.starts_with("bash ")
        || executable == "zsh"
        || executable.starts_with("zsh ")
        || matches!(executable.as_str(), "ksh" | "mksh")
}

fn has_pipeline_operator(shell: &ShellInfo, command: &str) -> bool {
    #[derive(Clone, Copy)]
    enum Quote {
        Single,
        Double,
    }

    let mut quote = None;
    let mut escaped = false;
    let mut chars = command.chars().peekable();
    while let Some(character) = chars.next() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some(Quote::Single) if character == '\'' => quote = None,
            Some(Quote::Single) => {},
            Some(Quote::Double) if character == '"' => quote = None,
            Some(Quote::Double) if is_escape_character(shell, character) => escaped = true,
            Some(Quote::Double) => {},
            None if character == '\'' && shell.family != ShellFamily::Cmd => {
                quote = Some(Quote::Single);
            },
            None if character == '"' => quote = Some(Quote::Double),
            None if is_escape_character(shell, character) => escaped = true,
            None if character == '|' && chars.peek() == Some(&'|') => {
                chars.next();
            },
            None if character == '|' => return true,
            None => {},
        }
    }
    false
}

fn is_escape_character(shell: &ShellInfo, character: char) -> bool {
    match shell.family {
        ShellFamily::Posix | ShellFamily::Wsl => character == '\\',
        ShellFamily::PowerShell => character == '`',
        ShellFamily::Cmd => character == '^',
    }
}

#[cfg(windows)]
fn preprocess_shell_command(command: &str, shell: &ShellInfo) -> String {
    if shell.family != ShellFamily::Posix {
        return command.into();
    }
    static NUL_REDIRECT: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    let regex = NUL_REDIRECT
        .get_or_init(|| regex::Regex::new(r"(\d?&?>+\s*)[Nn][Uu][Ll](\s|$|[|&;)\n\r])").ok());
    regex.as_ref().map_or_else(
        || command.into(),
        |regex| regex.replace_all(command, "${1}/dev/null${2}").into_owned(),
    )
}

#[cfg(not(windows))]
fn preprocess_shell_command(command: &str, _shell: &ShellInfo) -> String {
    command.into()
}

fn shell_args(family: ShellFamily, command: &str) -> Vec<String> {
    match family {
        ShellFamily::Posix => vec!["-c".into(), command.into()],
        ShellFamily::PowerShell => vec![
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            command.into(),
        ],
        ShellFamily::Cmd => vec!["/D".into(), "/S".into(), "/C".into(), command.into()],
        ShellFamily::Wsl => vec!["--".into(), "bash".into(), "-lc".into(), command.into()],
    }
}

fn add_invocation_metadata(
    result: &mut ToolResult,
    args: &ShellArgs,
    shell: &ShellInfo,
    cwd: &str,
    timeout_secs: u64,
    pipeline: PipelineSemantics,
) {
    result
        .metadata
        .insert("command".into(), serde_json::json!(args.command));
    result.metadata.insert("cwd".into(), serde_json::json!(cwd));
    result
        .metadata
        .insert("shell".into(), serde_json::json!(shell.name));
    result
        .metadata
        .insert("shellPath".into(), serde_json::json!(shell.path));
    result
        .metadata
        .insert("timeoutSecs".into(), serde_json::json!(timeout_secs));
    result
        .metadata
        .insert("pipelinePolicy".into(), serde_json::json!("strict"));
    result.metadata.insert(
        "pipelinePolicyEnforced".into(),
        serde_json::json!(pipeline.enforced),
    );
    result.metadata.insert(
        "pipelineStatusScope".into(),
        serde_json::json!(if pipeline.enforced {
            "allPipelineStages"
        } else {
            "lastPipelineStage"
        }),
    );
}

fn sudo_authentication_failed(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    [
        "sudo: a terminal is required",
        "sudo: a password is required",
        "sudo: no tty present",
        "sudo: sorry, you must have a tty",
        "sudo: no askpass program specified",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn render_process_result(
    started_at: Instant,
    output: HostProcessReadOutput,
    intent: Option<String>,
) -> ToolResult {
    if output.state.is_running() {
        let output = ProcessPresentation::from(output);
        background_result(
            started_at,
            output.id,
            intent,
            output.content,
            output.stdout_bytes,
            output.stderr_bytes,
            output.dropped_bytes,
        )
    } else {
        render_completed(started_at, output.into(), intent)
    }
}

fn render_completed(
    started_at: Instant,
    output: ProcessPresentation,
    intent: Option<String>,
) -> ToolResult {
    let ProcessPresentation {
        id,
        content: combined,
        stdout_bytes,
        stderr_bytes,
        dropped_bytes,
        state,
    } = output;
    let status = state.status();
    let timed_out = matches!(state, HostProcessState::TimedOut { .. });
    let semantic_error = sudo_authentication_failed(&combined);
    let execution_status = match state {
        HostProcessState::Running {} => "running",
        HostProcessState::TimedOut { .. } => "timed_out",
        HostProcessState::Killed { .. } | HostProcessState::Cancelled { .. } => "cancelled",
        HostProcessState::Exited { status: Some(0) } if !semantic_error => "succeeded",
        HostProcessState::Exited { .. } => "failed",
    };
    let mut metadata = BTreeMap::from([
        ("shellId".into(), serde_json::json!(id)),
        (
            "executionStatus".into(),
            serde_json::json!(execution_status),
        ),
        ("exitCode".into(), serde_json::json!(status)),
        ("timedOut".into(), serde_json::json!(timed_out)),
        ("stdoutBytes".into(), serde_json::json!(stdout_bytes)),
        ("stderrBytes".into(), serde_json::json!(stderr_bytes)),
        ("droppedBytes".into(), serde_json::json!(dropped_bytes)),
        ("intent".into(), serde_json::json!(intent)),
    ]);
    if semantic_error {
        metadata.insert(
            "semanticError".into(),
            serde_json::json!("sudo_authentication_required"),
        );
    }
    let process_status = match state {
        HostProcessState::TimedOut { .. } => "Process timed out".into(),
        HostProcessState::Killed { .. } => "Process was killed".into(),
        HostProcessState::Cancelled { .. } => "Process was cancelled".into(),
        HostProcessState::Exited { .. } => {
            format!("Process exited with code {}", status.unwrap_or(-1))
        },
        HostProcessState::Running {} => "Process is still running".into(),
    };
    let diagnostic = semantic_error.then_some(
        "Command output indicates sudo authentication failed. Do not retry with sudo; report the \
         missing privilege or dependency as blocked.",
    );
    let output = if combined.is_empty() {
        "(no output)".into()
    } else if let Some(diagnostic) = diagnostic {
        format!("{diagnostic}\n\n{combined}")
    } else {
        combined
    };
    let content = format!("{process_status}\nOutput:\n{output}");
    if execution_status == "succeeded" {
        success(started_at, content, metadata)
    } else {
        completed_error(started_at, content, metadata)
    }
}

fn background_result(
    started_at: Instant,
    id: String,
    intent: Option<String>,
    output: String,
    stdout_bytes: usize,
    stderr_bytes: usize,
    dropped_bytes: usize,
) -> ToolResult {
    let content = if output.is_empty() {
        format!("Process is running in background: {id}")
    } else {
        format!("Process is running in background: {id}\nOutput so far:\n{output}")
    };
    success(
        started_at,
        content,
        BTreeMap::from([
            ("shellId".into(), serde_json::json!(id)),
            ("executionStatus".into(), serde_json::json!("running")),
            ("timedOut".into(), serde_json::json!(false)),
            ("stdoutBytes".into(), serde_json::json!(stdout_bytes)),
            ("stderrBytes".into(), serde_json::json!(stderr_bytes)),
            ("droppedBytes".into(), serde_json::json!(dropped_bytes)),
            ("intent".into(), serde_json::json!(intent)),
        ]),
    )
}

fn invalid(message: impl Into<String>) -> ExtensionError {
    ExtensionError::InvalidInput {
        code: astrcode_extension_sdk::WireErrorCode::InvalidInput
            .as_str()
            .into(),
        message: message.into(),
        hint: Some("provide either a command to start or a shellId to poll".into()),
    }
}

pub(super) fn definition() -> ToolDefinition {
    let shell = resolve_shell();
    ToolDefinition {
        name: "shell".into(),
        description: format!(
            concat!(
                "Executes a {shell} command and returns output. Working directory persists, shell \
                 state does not.\n\n",
                "When NOT to use:\n- File search or reading files → `grep`/`glob`/`read`\n",
                "- Interactive REPL or debugger sessions → `terminal`\n\n",
                "Tips:\n- Set runInBackground for commands expected to exceed ~30s.\n",
                "- Foreground commands still running after ~30s are promoted to a session-owned \
                 background process.\n",
                "- Poll shellId for incremental output; stop polling once completed.\n",
                "- Timeout is 1-600s and uses the configured default when omitted. Set cwd \
                 instead of using cd.\n",
                "- Non-zero exit codes are errors."
            ),
            shell = shell.name,
        ),
        strict: true,
        origin: ToolOrigin::Bundled,
        execution_mode: ExecutionMode::Sequential,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "intent": { "type": "string" },
                "cwd": { "type": "string" },
                "timeout": { "type": "integer", "minimum": 1, "maximum": 600 },
                "stdin": { "type": "string" },
                "runInBackground": { "type": "boolean" },
                "shellId": { "type": "string" },
                "blockUntilMs": { "type": "integer", "minimum": 0, "maximum": 600000 }
            },
            "required": [],
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_output_keeps_a_valid_utf8_tail_and_counts_discarded_bytes() {
        let mut output = BoundedOutput::default();
        output.push(&"x".repeat(MAX_FOREGROUND_OUTPUT_BYTES - 1));
        output.push("界界");

        assert!(output.content.len() <= MAX_FOREGROUND_OUTPUT_BYTES);
        assert_eq!(
            output.dropped_bytes + output.content.len(),
            MAX_FOREGROUND_OUTPUT_BYTES + 5
        );
        assert!(output.content.ends_with("界界"));
    }

    #[test]
    fn shell_semantics_fail_closed_without_misreading_quoted_pipes() {
        let bash = ShellInfo {
            family: ShellFamily::Posix,
            name: "bash".into(),
            path: "/bin/bash".into(),
        };
        let powershell = ShellInfo {
            family: ShellFamily::PowerShell,
            name: "pwsh".into(),
            path: "pwsh".into(),
        };

        let (pipeline, policy) =
            prepare_shell_command(&bash, "printf x | cat").expect("bash pipeline");
        assert_eq!(pipeline, "set -o pipefail\nprintf x | cat");
        assert!(policy.enforced);
        assert!(prepare_shell_command(&powershell, "Get-Item . | Format-List").is_err());
        assert!(!has_pipeline_operator(&powershell, "Write-Output 'a|b'"));
        assert!(!has_pipeline_operator(
            &bash,
            "printf 'a|b' || printf fallback"
        ));
        assert!(sudo_authentication_failed(
            "sudo: a terminal is required to read the password"
        ));
    }
}
