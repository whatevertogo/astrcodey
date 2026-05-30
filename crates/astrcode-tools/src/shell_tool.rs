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

use crate::files::tool_call_id;

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
    /// 通过 stdin 传入命令的输入数据。
    #[serde(default)]
    stdin: Option<String>,
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

    fn prompt_metadata(&self) -> Option<ToolPromptMetadata> {
        Some(ToolPromptMetadata::new("").prompt_tag(ToolPromptTag::System))
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
            .stderr(Stdio::piped());
        hide_command_window(&mut command);
        setup_process_group(&mut command);

        // stdin 处理：有数据则 pipe，否则 null
        if args.stdin.is_some() {
            command.stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null());
        }

        let mut child = command
            .spawn()
            .map_err(|e| ToolError::Execution(format!("spawn: {e}")))?;

        // 写入 stdin 数据后关闭
        if let Some(input) = &args.stdin {
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt;
                stdin
                    .write_all(input.as_bytes())
                    .await
                    .map_err(|e| ToolError::Execution(format!("stdin write: {e}")))?;
                drop(stdin);
            }
        }

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
            let timeout_msg = format!("shell command timed out after {timeout_secs}s");
            // LLM 历史只携带 content；超时说明必须写入正文。
            if output == "(no output)" {
                output = timeout_msg.clone();
            } else {
                output.push_str("\n\n");
                output.push_str(&timeout_msg);
            }
            Some(timeout_msg)
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
            concat!(
                "Executes a {shell} command and returns output. Working directory persists, shell \
                 state does not.\n\n",
                "When NOT to use:\n",
                "- File search or reading files → `grep`/`glob`/`read`\n",
                "- Interactive REPL or debugger sessions → `terminal`\n\n",
                "Tips:\n",
                "- One-shot commands with timeout up to 600s (default {timeout_secs}s)\n",
                "- Independent commands may run together; chain dependent ones with `&&`\n",
                "- Set `cwd` instead of using `cd`. Use `stdin` to pipe data.\n",
                "- Non-zero exit codes produce errors.",
            ),
            shell = shell.name,
            timeout_secs = timeout_secs,
        ),
        origin: ToolOrigin::Builtin,
        execution_mode: ExecutionMode::Sequential,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute. Use absolute paths; chain dependent commands with &&." },
                "intent": {
                    "type": "string",
                    "description": "Short active-voice reason, shown in audit/UI."
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory. Prefer over shell-level cd."
                },
                "timeout": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 600,
                    "description": "Seconds. Override default for long commands."
                },
                "stdin": {
                    "type": "string",
                    "description": "Pipe data into stdin (jq, wc, python, etc.)."
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

/// 在 Unix 上设置子进程为独立进程组（pgid == pid），
/// 以便超时时可以通过 `kill(-pgid, SIGTERM)` 杀掉整棵子进程树。
#[cfg(unix)]
fn setup_process_group(command: &mut Command) {
    // SAFETY: setsid() 是 async-signal-safe 的 POSIX 调用。
    // 在 fork 后 exec 前执行，让子进程成为新 session leader。
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn setup_process_group(_command: &mut Command) {}

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

/// 杀掉子进程及其整个进程组。
///
/// 先发 SIGTERM 给进程组（graceful），等 2 秒后若仍存活则 SIGKILL。
#[cfg(unix)]
async fn terminate_child_tree(child: &mut tokio::process::Child) {
    let Some(pid) = child.id() else {
        let _ = child.start_kill();
        return;
    };
    let pgid = pid as i32;

    // 先 SIGTERM 整个进程组
    unsafe {
        libc::kill(-pgid, libc::SIGTERM);
    }

    // 等待 2 秒让进程优雅退出
    if tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
        .await
        .is_ok()
    {
        return;
    }

    // 仍未退出，SIGKILL 进程组
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
    let _ = child.start_kill();
}

/// 非 Unix、非 Windows 平台的 fallback。
#[cfg(all(not(unix), not(windows)))]
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
        const WINDOWS_SLEEP: &str = "powershell -NoProfile -Command \"Start-Sleep -Seconds 10\"";
        match resolve_shell().family {
            ShellFamily::PowerShell => "[Console]::Out.WriteLine('before'); \
                                        [Console]::Out.Flush(); Start-Sleep -Seconds 10; \
                                        [Console]::Out.WriteLine('after')"
                .into(),
            ShellFamily::Cmd => format!("echo before & {WINDOWS_SLEEP} & echo after"),
            ShellFamily::Posix | ShellFamily::Wsl => {
                if cfg!(windows) {
                    format!("echo before; {WINDOWS_SLEEP}; echo after")
                } else {
                    "echo before; sleep 10; echo after".into()
                }
            },
        }
    }

    /// 读取 stdin 并原样输出，用于测试 stdin 参数。
    fn command_echoing_stdin() -> String {
        match resolve_shell().family {
            ShellFamily::PowerShell => "[Console]::In.ReadToEnd()".into(),
            ShellFamily::Cmd => "findstr \"^\"".into(),
            ShellFamily::Posix | ShellFamily::Wsl => "cat".into(),
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
        assert!(
            !result.content.lines().any(|line| line.trim() == "after"),
            "command output after the sleep should not appear: {}",
            result.content
        );
        assert!(
            result.content.contains("timed out after 5s"),
            "timeout reason must be in content for the LLM: {}",
            result.content
        );
        assert_eq!(result.metadata["timedOut"], serde_json::json!(true));
        assert_eq!(result.metadata["streamed"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn shell_stdin_pipes_data_to_command() {
        let tool = ShellTool {
            working_dir: std::env::current_dir().expect("cwd should exist"),
            timeout_secs: 30,
        };

        let result = tool
            .execute(
                serde_json::json!({
                    "command": command_echoing_stdin(),
                    "stdin": "hello from stdin\n"
                }),
                &empty_ctx(),
            )
            .await
            .expect("shell with stdin should execute");

        assert!(!result.is_error, "unexpected error: {result:?}");
        assert!(
            result.content.contains("hello from stdin"),
            "stdout should contain stdin data, got: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn shell_rejects_empty_command() {
        let tool = ShellTool {
            working_dir: std::env::current_dir().expect("cwd should exist"),
            timeout_secs: 30,
        };
        let result = tool
            .execute(serde_json::json!({ "command": "   " }), &empty_ctx())
            .await;
        let err = result.expect_err("empty command should fail");
        assert!(
            err.to_string().contains("empty"),
            "error should mention empty: {err}"
        );
    }

    #[tokio::test]
    async fn shell_nonzero_exit_code_is_error() {
        let tool = ShellTool {
            working_dir: std::env::current_dir().expect("cwd should exist"),
            timeout_secs: 30,
        };
        let exit_cmd = match resolve_shell().family {
            ShellFamily::PowerShell => "exit 42",
            ShellFamily::Cmd => "exit /b 42",
            // On MSYS2/bash, the exit code may be transformed; just verify non-zero.
            ShellFamily::Posix | ShellFamily::Wsl => "exit 42",
        };
        let result = tool
            .execute(serde_json::json!({ "command": exit_cmd }), &empty_ctx())
            .await
            .expect("shell should execute");
        assert!(result.is_error, "non-zero exit should be error");
        let exit_code = result.metadata["exitCode"].as_i64().expect("exitCode");
        assert_ne!(
            exit_code, 0,
            "exit code should be non-zero, got {exit_code}"
        );
    }

    #[test]
    fn render_shell_output_formats_stdout_and_stderr() {
        use super::render_shell_output;
        assert_eq!(render_shell_output("", ""), "");
        assert_eq!(render_shell_output("hello", ""), "hello");
        assert_eq!(render_shell_output("", "oops"), "STDERR:\noops");
        assert_eq!(render_shell_output("hi", "err"), "hi\nSTDERR:\nerr");
    }

    #[test]
    fn command_args_wsl_prepends_bash() {
        let wsl = ShellInfo {
            family: ShellFamily::Wsl,
            name: "wsl".into(),
            path: "wsl.exe".into(),
        };
        assert_eq!(command_args(&wsl, "ls -la"), vec!["bash", "-lc", "ls -la"]);
    }

    #[tokio::test]
    async fn shell_metadata_includes_shell_info_and_cwd() {
        let tool = ShellTool {
            working_dir: std::env::current_dir().expect("cwd should exist"),
            timeout_secs: 30,
        };
        let result = tool
            .execute(
                serde_json::json!({
                    "command": match resolve_shell().family {
                        ShellFamily::PowerShell => "Write-Output ok",
                        ShellFamily::Cmd => "echo ok",
                        ShellFamily::Posix | ShellFamily::Wsl => "echo ok",
                    },
                }),
                &empty_ctx(),
            )
            .await
            .expect("shell should execute");

        assert!(!result.is_error, "{result:?}");
        assert!(result.metadata.contains_key("shell"));
        assert!(result.metadata.contains_key("shellPath"));
        assert!(result.metadata.contains_key("cwd"));
        assert!(result.metadata.contains_key("exitCode"));
        assert_eq!(result.metadata["streamed"], serde_json::json!(true));
        assert_eq!(result.metadata["timedOut"], serde_json::json!(false));
    }
}
