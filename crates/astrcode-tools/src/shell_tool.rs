//! Shell execution tool with streaming stdout/stderr and timeout.

use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    process::Stdio,
    sync::{Mutex, OnceLock},
    time::Instant,
};

use astrcode_core::{
    event::{EventPayload, ToolOutputStream},
    tool::*,
};
use astrcode_support::{
    hostpaths::resolve_path,
    shell::{ShellFamily, ShellInfo, resolve_shell},
};
use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

/// Shell 命令执行工具，支持流式 stdout/stderr 捕获和超时控制。
///
/// 自动检测系统默认 Shell（PowerShell / cmd / bash / wsl），
/// 以非交互方式执行命令并返回输出和退出码。
pub struct ShellTool {
    /// 工具的工作目录，用于解析相对路径
    pub working_dir: PathBuf,
    /// 默认命令超时时间（秒）
    pub timeout_secs: u64,
}

/// shell 工具的参数。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShellArgs {
    /// 要执行的 shell 命令
    command: String,
    /// 命令意图，供审计和 UI 摘要使用，不影响执行
    #[serde(default)]
    intent: Option<String>,
    /// 命令的工作目录（可选，默认为工具的 working_dir）
    #[serde(default)]
    cwd: Option<PathBuf>,
    /// 本次执行的超时时间（秒，最大 600）
    #[serde(default)]
    timeout: Option<u64>,
    /// 是否立即在后台执行，不阻塞 agent loop。
    ///
    /// 设为 true 时，命令立刻转入后台运行，agent 收到占位结果后可继续推理。
    /// 适用于 dev server、watcher、build 等长时间运行的任务。
    ///
    /// 此字段由 agent loop 的工具执行调度层在反序列化前读取（从原始 tool_input JSON），
    /// 不在 ShellTool::execute 内部使用。
    #[serde(default)]
    #[expect(dead_code)]
    run_in_background: bool,
}

#[async_trait::async_trait]
impl Tool for ShellTool {
    /// 返回 shell 工具的定义，动态显示当前系统 Shell 名称。
    fn definition(&self) -> ToolDefinition {
        shell_tool_definition(self.timeout_secs)
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    /// Shell 命令执行超过 timeout 一半时间后自动后台化。
    fn background_policy(&self) -> BackgroundPolicy {
        BackgroundPolicy::AutoAfter {
            threshold_secs: (self.timeout_secs / 2).max(30),
        }
    }

    fn prompt_metadata(&self) -> Option<ToolPromptMetadata> {
        Some(
            ToolPromptMetadata::new(
                "Use `shell` for commands that need the OS or project toolchain: package \
                 managers, build tools, git, docker, etc.",
            )
            .caveat(
                "Default timeout varies by config. Prefer the timeout parameter for long-running \
                 commands.",
            )
            .prompt_tag("system")
            .always_include(true),
        )
    }

    /// 执行 shell 命令：解析参数 → 构建子进程 → 并发读取 stdout/stderr → 等待完成或超时。
    ///
    /// 超时后会强制终止子进程并返回 `ToolError::Timeout`。
    /// 退出码非零时 `is_error` 为 true。
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let started_at = Instant::now();
        let args: ShellArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid shell args: {e}")))?;
        if args.command.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "command cannot be empty".into(),
            ));
        }
        let shell = resolve_shell();
        let command_args = command_args(&shell, &args.command);
        let cwd = args
            .cwd
            .as_deref()
            .map(|cwd| resolve_path(&self.working_dir, cwd))
            .unwrap_or_else(|| self.working_dir.clone());
        let timeout_secs = args.timeout.unwrap_or(self.timeout_secs).min(600);

        let mut command = Command::new(&shell.path);
        command
            .args(&command_args)
            .current_dir(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        hide_command_window(&mut command);

        let mut child = command
            .spawn()
            .map_err(|e| ToolError::Execution(format!("spawn: {e}")))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Execution("failed to capture stdout".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Execution("failed to capture stderr".into()))?;

        let call_id = tool_call_id(ctx);
        let out_h = tokio::spawn(read_stream(
            stdout,
            ToolOutputStream::Stdout,
            ctx.event_tx.clone(),
            call_id.clone(),
        ));
        let err_h = tokio::spawn(read_stream(
            stderr,
            ToolOutputStream::Stderr,
            ctx.event_tx.clone(),
            call_id.clone(),
        ));

        let (status, timed_out) =
            match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), child.wait())
                .await
            {
                Ok(status) => (status, false),
                Err(_) => {
                    terminate_child_tree(&mut child).await;
                    let status = child.wait().await;
                    (status, true)
                },
            };

        let exit = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
        let stdout_capture = out_h.await.unwrap_or_else(|_| CapturedOutput::default());
        let stderr_capture = err_h.await.unwrap_or_else(|_| CapturedOutput::default());

        let mut output = render_shell_output(&stdout_capture.text, &stderr_capture.text);

        let mut meta = BTreeMap::new();
        meta.insert("command".into(), serde_json::json!(args.command));
        if let Some(intent) = args.intent.filter(|intent| !intent.trim().is_empty()) {
            meta.insert("intent".into(), serde_json::json!(intent));
        }
        meta.insert("exitCode".into(), serde_json::json!(exit));
        meta.insert("shell".into(), serde_json::json!(shell.name));
        meta.insert("shellPath".into(), serde_json::json!(shell.path));
        meta.insert("cwd".into(), serde_json::json!(cwd.display().to_string()));
        meta.insert("streamed".into(), serde_json::json!(true));
        meta.insert("timedOut".into(), serde_json::json!(timed_out));
        meta.insert(
            "stdoutBytes".into(),
            serde_json::json!(stdout_capture.bytes_read),
        );
        meta.insert(
            "stderrBytes".into(),
            serde_json::json!(stderr_capture.bytes_read),
        );
        if output.is_empty() {
            output = "(no output)".into();
        }

        let is_error = timed_out || exit != 0;
        let error = if timed_out {
            Some(format!("shell command timed out after {timeout_secs}s"))
        } else if exit == 0 {
            None
        } else {
            Some(format!("exit code {exit}"))
        };

        Ok(ToolResult {
            call_id,
            content: output,
            is_error,
            error,
            metadata: meta,
            duration_ms: Some(started_at.elapsed().as_millis() as u64),
        })
    }
}

fn shell_tool_definition(timeout_secs: u64) -> ToolDefinition {
    static DEFINITIONS: OnceLock<Mutex<HashMap<(String, u64), ToolDefinition>>> = OnceLock::new();
    let shell = resolve_shell();
    let key = (shell.name.clone(), timeout_secs);
    let mut definitions = DEFINITIONS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(definition) = definitions.get(&key) {
        return definition.clone();
    }

    let definition = ToolDefinition {
        name: "shell".into(),
        description: format!(
            "Execute a shell command with the default shell ({}). Returns stdout, stderr, and \
             exit code. Prefer file tools for reading, searching, and editing files; use shell \
             for commands that need the OS or project toolchain. Default timeout: {}s (max 600s, \
             prefer override with the timeout parameter).",
            shell.name, timeout_secs,
        ),
        origin: ToolOrigin::Builtin,
        execution_mode: ExecutionMode::Sequential,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "intent": {
                    "type": "string",
                    "description": "Short active-voice reason for running this command, useful for audit and progress display."
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory for this command. Prefer this over shell-level cd."
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 600,
                    "description": "Timeout in seconds (default from config, max 600)."
                },
                "runInBackground": {
                    "type": "boolean",
                    "default": false,
                    "description": "Run the command in the background immediately. Use for long-running tasks like dev servers, file watchers, or builds. When true, the agent receives a placeholder result and can continue reasoning."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        }),
    };
    definitions.insert(key, definition.clone());
    definition
}

/// 根据 Shell 类型构建命令行参数。
///
/// 不同 Shell 的调用方式：
/// - PowerShell: `-NoProfile -Command <cmd>`
/// - cmd: `/d /s /c <cmd>`
/// - POSIX: `-lc <cmd>`
/// - WSL: `bash -lc <cmd>`
fn command_args(shell: &ShellInfo, command: &str) -> Vec<String> {
    match shell.family {
        ShellFamily::PowerShell => vec![
            "-NoProfile".to_string(),
            "-Command".to_string(),
            command.to_string(),
        ],
        ShellFamily::Cmd => vec![
            "/d".to_string(),
            "/s".to_string(),
            "/c".to_string(),
            command.to_string(),
        ],
        ShellFamily::Posix => vec!["-lc".to_string(), command.to_string()],
        ShellFamily::Wsl => vec!["bash".to_string(), "-lc".to_string(), command.to_string()],
    }
}

#[cfg(windows)]
fn hide_command_window(command: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_command_window(_: &mut Command) {}

#[cfg(windows)]
async fn terminate_child_tree(child: &mut tokio::process::Child) {
    let Some(pid) = child.id() else {
        let _ = child.start_kill();
        return;
    };
    let mut taskkill = Command::new("taskkill.exe");
    taskkill
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    hide_command_window(&mut taskkill);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), taskkill.status()).await;
    let _ = child.start_kill();
}

#[cfg(not(windows))]
#[allow(clippy::unused_async)]
async fn terminate_child_tree(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
}

#[derive(Default)]
struct CapturedOutput {
    text: String,
    bytes_read: usize,
}

/// 异步读取流的内容，发送增量事件，并保留最终文本。
async fn read_stream(
    mut stream: impl AsyncRead + Unpin,
    stream_kind: ToolOutputStream,
    event_tx: Option<tokio::sync::mpsc::UnboundedSender<EventPayload>>,
    call_id: String,
) -> CapturedOutput {
    let mut output = CapturedOutput::default();
    let mut buf = [0u8; 8192];
    while let Ok(n) = stream.read(&mut buf).await {
        if n == 0 {
            break;
        }
        output.bytes_read += n;
        let delta = String::from_utf8_lossy(&buf[..n]).into_owned();
        output.text.push_str(&delta);
        if let Some(tx) = &event_tx {
            let _ = tx.send(EventPayload::ToolOutputDelta {
                call_id: call_id.clone().into(),
                stream: stream_kind,
                delta,
            });
        }
    }
    output
}

fn render_shell_output(stdout: &str, stderr: &str) -> String {
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.to_string(),
        (true, false) => format!("STDERR:\n{stderr}"),
        (false, false) => format!("{stdout}\nSTDERR:\n{stderr}"),
    }
}

fn tool_call_id(ctx: &ToolExecutionContext) -> String {
    ctx.tool_call_id.clone().unwrap_or_default()
}

// TODO: sandbox support — execute commands in isolated environment
// TODO: execpolicy — command allow/deny rules (via extensions)

#[cfg(test)]
mod tests {
    use astrcode_core::{
        event::{EventPayload, ToolOutputStream},
        tool::{Tool, ToolCapabilities, ToolExecutionContext},
    };
    use astrcode_support::shell::{ShellFamily, ShellInfo, resolve_shell};
    use tokio::sync::mpsc;

    use super::{ShellTool, command_args};

    fn empty_ctx() -> ToolExecutionContext {
        ToolExecutionContext {
            session_id: String::new().into(),
            working_dir: String::new(),
            tool_call_id: None,
            event_tx: None,
            capabilities: ToolCapabilities::default(),
        }
    }

    fn command_with_stderr() -> String {
        match resolve_shell().family {
            ShellFamily::PowerShell => "Write-Output out; [Console]::Error.WriteLine('err')".into(),
            ShellFamily::Cmd => "echo out & echo err 1>&2".into(),
            ShellFamily::Posix | ShellFamily::Wsl => "echo out; echo err >&2".into(),
        }
    }

    fn command_with_delay() -> String {
        match resolve_shell().family {
            ShellFamily::PowerShell => "[Console]::Out.WriteLine('before'); \
                                        [Console]::Out.Flush(); Start-Sleep -Seconds 10; \
                                        [Console]::Out.WriteLine('after')"
                .into(),
            ShellFamily::Cmd => "echo before & ping -n 11 127.0.0.1 > nul & echo after".into(),
            ShellFamily::Posix | ShellFamily::Wsl => "echo before; sleep 10; echo after".into(),
        }
    }

    #[test]
    fn command_args_match_resolved_shell_family() {
        let command = "echo ok";

        let powershell = ShellInfo {
            family: ShellFamily::PowerShell,
            name: "powershell".into(),
            path: "powershell.exe".into(),
        };
        assert_eq!(
            command_args(&powershell, command),
            vec!["-NoProfile", "-Command", command]
        );

        let cmd = ShellInfo {
            family: ShellFamily::Cmd,
            name: "cmd".into(),
            path: "cmd.exe".into(),
        };
        assert_eq!(command_args(&cmd, command), vec!["/d", "/s", "/c", command]);

        let posix = ShellInfo {
            family: ShellFamily::Posix,
            name: "bash".into(),
            path: "bash".into(),
        };
        assert_eq!(command_args(&posix, command), vec!["-lc", command]);
    }

    #[tokio::test]
    async fn shell_streams_stdout_and_stderr_events() {
        let tool = ShellTool {
            working_dir: std::env::current_dir().expect("cwd should exist"),
            timeout_secs: 30,
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ctx = ToolExecutionContext {
            tool_call_id: Some("shell-stream".into()),
            event_tx: Some(tx),
            ..empty_ctx()
        };

        let result = tool
            .execute(
                serde_json::json!({
                    "command": command_with_stderr(),
                    "intent": "Check stdout and stderr capture"
                }),
                &ctx,
            )
            .await
            .expect("shell should execute");

        assert_eq!(result.call_id, "shell-stream");
        assert!(!result.is_error, "{result:?}");
        assert!(result.content.contains("out"));
        assert!(result.content.contains("err"));
        assert_eq!(result.metadata["streamed"], serde_json::json!(true));
        assert_eq!(
            result.metadata["intent"],
            serde_json::json!("Check stdout and stderr capture")
        );

        let mut saw_stdout = false;
        let mut saw_stderr = false;
        while let Ok(event) = rx.try_recv() {
            if let EventPayload::ToolOutputDelta {
                call_id,
                stream,
                delta,
            } = event
            {
                assert_eq!(call_id.as_str(), "shell-stream");
                saw_stdout |= stream == ToolOutputStream::Stdout && delta.contains("out");
                saw_stderr |= stream == ToolOutputStream::Stderr && delta.contains("err");
            }
        }
        assert!(saw_stdout, "stdout event should be emitted");
        assert!(saw_stderr, "stderr event should be emitted");
    }

    #[tokio::test]
    async fn shell_timeout_returns_partial_output() {
        let tool = ShellTool {
            working_dir: std::env::current_dir().expect("cwd should exist"),
            timeout_secs: 30,
        };

        let result = tool
            .execute(
                serde_json::json!({
                    "command": command_with_delay(),
                    "timeout": 5
                }),
                &empty_ctx(),
            )
            .await
            .expect("shell should return a structured timeout result");

        assert!(result.is_error);
        assert!(result.content.contains("before"));
        assert!(!result.content.contains("after"));
        assert_eq!(result.metadata["timedOut"], serde_json::json!(true));
        assert_eq!(result.metadata["streamed"], serde_json::json!(true));
    }
}
