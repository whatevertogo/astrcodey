//! Shell execution tool — waits for completion and returns captured stdout/stderr.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Mutex, OnceLock},
    time::Instant,
};

use astrcode_core::{tool::*, tool_access::ResourceAccess};
use astrcode_support::{
    hostpaths::resolve_path,
    shell::{ShellFamily, ShellInfo, resolve_shell},
};
use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

use crate::{
    background_shell::{BackgroundShellSpawnParams, spawn_background_shell, wait_background_shell},
    files::tool_call_id,
};

/// 每路 stdout/stderr 在内存中保留的上限；超出部分仍会从 pipe 读走以免子进程阻塞。
pub(crate) const MAX_CAPTURE_BYTES_PER_STREAM: usize = 512 * 1024;

/// Shell 命令执行工具，等待子进程结束并返回完整输出（不流式推送 live delta）。
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
    /// 要执行的 shell 命令（与 `shell_id` 互斥；省略时须提供 `shell_id`）。
    #[serde(default)]
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
    /// 为 true 时在后台运行，立即返回 `shellId`；完成后通过会话注入通知 agent。
    #[serde(default)]
    run_in_background: Option<bool>,
    /// 已有后台 shell 的 id；提供时忽略 `command`，用于等待或查询状态。
    #[serde(default, rename = "shellId")]
    shell_id: Option<String>,
    /// 与 `shellId` 联用：最多阻塞等待的毫秒数（0 表示立即返回当前状态）。
    #[serde(default, rename = "blockUntilMs")]
    block_until_ms: Option<u64>,
}

#[async_trait::async_trait]
impl Tool for ShellTool {
    /// 返回 shell 工具的定义，动态显示当前系统 Shell 名称。
    fn definition(&self) -> ToolDefinition {
        shell_tool_definition(self.timeout_secs)
    }

    fn resource_accesses(
        &self,
        _arguments: &serde_json::Value,
        _working_dir: &Path,
    ) -> Result<Vec<ResourceAccess>, ToolError> {
        Ok(vec![ResourceAccess::all()])
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

        if let Some(shell_id) = args
            .shell_id
            .as_ref()
            .filter(|id| !id.trim().is_empty())
            .map(|id| id.as_str())
        {
            if !args.command.trim().is_empty() {
                return Err(ToolError::InvalidArguments(
                    "cannot specify both shellId and command; use shellId to query an existing \
                     background shell"
                        .into(),
                ));
            }
            if args.run_in_background == Some(true) {
                return Err(ToolError::InvalidArguments(
                    "cannot specify both shellId and runInBackground".into(),
                ));
            }
            return execute_background_shell_wait(
                shell_id,
                args.block_until_ms.unwrap_or(0),
                started_at,
                ctx,
            )
            .await;
        }

        if args.run_in_background == Some(true) {
            return self
                .execute_background_shell_spawn(args, started_at, ctx)
                .await;
        }

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
        let out_h = tokio::spawn(capture_stream(stdout));
        let err_h = tokio::spawn(capture_stream(stderr));

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
        meta.insert("streamed".into(), serde_json::json!(false));
        meta.insert("timedOut".into(), serde_json::json!(timed_out));
        meta.insert(
            "stdoutBytes".into(),
            serde_json::json!(stdout_capture.bytes_read),
        );
        meta.insert(
            "stderrBytes".into(),
            serde_json::json!(stderr_capture.bytes_read),
        );
        if stdout_capture.truncated {
            meta.insert("stdoutTruncated".into(), serde_json::json!(true));
        }
        if stderr_capture.truncated {
            meta.insert("stderrTruncated".into(), serde_json::json!(true));
        }
        if stdout_capture.truncated || stderr_capture.truncated {
            meta.insert(
                "captureNote".into(),
                serde_json::json!(format!(
                    "Output exceeded {MAX_CAPTURE_BYTES_PER_STREAM} bytes per stream; captured \
                     prefix only. Re-run with narrower scope or redirect to a file and use `read`."
                )),
            );
        }
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

impl ShellTool {
    async fn execute_background_shell_spawn(
        &self,
        args: ShellArgs,
        started_at: Instant,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        if args.command.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "command cannot be empty when run_in_background is true".into(),
            ));
        }
        let shell = resolve_shell();
        let cwd = args
            .cwd
            .as_deref()
            .map(|cwd| resolve_path(&self.working_dir, cwd))
            .unwrap_or_else(|| self.working_dir.clone());
        let timeout_secs = args.timeout.unwrap_or(self.timeout_secs).min(600);
        let spawned = spawn_background_shell(BackgroundShellSpawnParams {
            session_id: ctx.session_id.to_string(),
            tool_call_id: ctx.tool_call_id.clone(),
            command: args.command.clone(),
            intent: args.intent.clone(),
            cwd,
            shell,
            timeout_secs,
            store_dir: ctx.capabilities.paths.store_dir.clone(),
            session_ops: ctx.capabilities.session.ops.clone(),
        })
        .await?;
        let path = spawned.output_path.display().to_string();
        let content = format!(
            "Background shell started (shell_id: {}).\nOutput is being written to: {path}\nYou \
             will be notified when the command completes. Do not poll; use `read` on this path \
             only if you need partial output before completion.",
            spawned.shell_id
        );
        let mut meta = BTreeMap::new();
        meta.insert("backgrounded".into(), serde_json::json!(true));
        meta.insert("shellId".into(), serde_json::json!(spawned.shell_id));
        meta.insert("outputPath".into(), serde_json::json!(path));
        meta.insert("command".into(), serde_json::json!(args.command));
        if let Some(intent) = args.intent.filter(|intent| !intent.trim().is_empty()) {
            meta.insert("intent".into(), serde_json::json!(intent));
        }
        Ok(ToolResult {
            call_id: tool_call_id(ctx),
            content,
            is_error: false,
            error: None,
            metadata: meta,
            duration_ms: Some(started_at.elapsed().as_millis() as u64),
        })
    }
}

async fn execute_background_shell_wait(
    shell_id: &str,
    block_until_ms: u64,
    started_at: Instant,
    ctx: &ToolExecutionContext,
) -> Result<ToolResult, ToolError> {
    let status = wait_background_shell(shell_id, block_until_ms).await?;
    let path = status.output_path.display().to_string();
    let content = if status.running {
        format!(
            "Shell {shell_id} is still running.\nOutput file: {path}\n\nRecent output:\n{}",
            status.tail
        )
    } else {
        format!(
            "Shell {shell_id} finished (status: {}, exit_code: {:?}).\nOutput file: \
             {path}\n\nOutput tail:\n{}",
            status.status, status.exit_code, status.tail
        )
    };
    let is_error = matches!(status.status.as_str(), "failed" | "timed_out" | "killed");
    let mut meta = BTreeMap::new();
    meta.insert("shellId".into(), serde_json::json!(shell_id));
    meta.insert("outputPath".into(), serde_json::json!(path));
    meta.insert("running".into(), serde_json::json!(status.running));
    meta.insert("status".into(), serde_json::json!(status.status));
    if let Some(code) = status.exit_code {
        meta.insert("exitCode".into(), serde_json::json!(code));
    }
    Ok(ToolResult {
        call_id: tool_call_id(ctx),
        content,
        is_error,
        error: is_error.then(|| format!("background shell {}", status.status)),
        metadata: meta,
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
    })
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
                "- Long-running commands: set `runInBackground` to true; you will be notified on \
                 completion (do not poll). Output is written under session background-shells/.\n",
                "- Poll/wait on a background shell with `shellId` and optional `blockUntilMs` (0 \
                 = status only).\n",
                "- Independent commands may run together; chain dependent ones with `&&`\n",
                "- Set `cwd` instead of using `cd`. Use `stdin` to pipe data.\n",
                "- Non-zero exit codes produce errors.\n",
                "- Foreground output is returned when the command finishes (not streamed live).\n",
                "- Very large output may be persisted to tool-results/; use `read` with \
                 charOffset and maxChars to paginate the saved path (do not re-run the command \
                 for more output).",
            ),
            shell = shell.name,
            timeout_secs = timeout_secs,
        ),
        origin: ToolOrigin::Builtin,
        execution_mode: ExecutionMode::Parallel,
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
                },
                "runInBackground": {
                    "type": "boolean",
                    "description": "Run in background; returns shellId immediately. Completion is delivered via session notification."
                },
                "shellId": {
                    "type": "string",
                    "description": "Existing background shell id. Omit command; use with blockUntilMs to wait."
                },
                "blockUntilMs": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "With shellId: max ms to block waiting for completion (0 = immediate status)."
                }
            },
            "required": [],
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
pub(crate) fn command_args(shell: &ShellInfo, command: &str) -> Vec<String> {
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
pub(crate) fn hide_command_window(command: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub(crate) fn hide_command_window(_: &mut Command) {}

/// 在 Unix 上设置子进程为独立进程组（pgid == pid），
/// 以便超时时可以通过 `kill(-pgid, SIGTERM)` 杀掉整棵子进程树。
#[cfg(unix)]
pub(crate) fn setup_process_group(command: &mut Command) {
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
pub(crate) fn setup_process_group(_command: &mut Command) {}

#[cfg(windows)]
pub(crate) async fn terminate_child_tree(child: &mut tokio::process::Child) {
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
pub(crate) async fn terminate_child_tree(child: &mut tokio::process::Child) {
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
pub(crate) async fn terminate_child_tree(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
}

#[derive(Default)]
struct CapturedOutput {
    text: String,
    bytes_read: usize,
    truncated: bool,
}

/// 读取子进程输出直到 EOF；超出 [`MAX_CAPTURE_BYTES_PER_STREAM`] 后仍继续 drain pipe
/// 以免子进程写端阻塞。
async fn capture_stream(mut stream: impl AsyncRead + Unpin) -> CapturedOutput {
    let mut output = CapturedOutput::default();
    let mut buf = [0u8; 8192];
    // Larger buffer for the drain-only phase — fewer syscalls, same correctness.
    let mut drain_buf = [0u8; 65536];
    let mut draining = false;
    loop {
        let read_buf = if draining { &mut drain_buf[..] } else { &mut buf[..] };
        let Ok(n) = stream.read(read_buf).await else { break };
        if n == 0 {
            break;
        }
        output.bytes_read += n;
        if draining {
            continue;
        }
        let kept = output.text.len();
        let remaining = MAX_CAPTURE_BYTES_PER_STREAM.saturating_sub(kept);
        if remaining == 0 {
            output.truncated = true;
            draining = true;
            continue;
        }
        let take = n.min(remaining);
        output.text.push_str(&String::from_utf8_lossy(&buf[..take]));
        if take < n {
            output.truncated = true;
            draining = true;
        }
    }
    output
}

pub(crate) fn render_shell_output(stdout: &str, stderr: &str) -> String {
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
    use std::time::Instant;

    use astrcode_core::tool::{Tool, ToolCapabilities, ToolExecutionContext};
    use astrcode_support::shell::{ShellFamily, ShellInfo, resolve_shell};

    use super::{MAX_CAPTURE_BYTES_PER_STREAM, ShellTool, capture_stream, command_args};

    fn empty_ctx() -> ToolExecutionContext {
        ToolExecutionContext::new(
            String::new().into(),
            String::new(),
            None,
            None,
            ToolCapabilities::default(),
        )
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
            // cmd.exe has no built-in sleep; delegate to powershell (always on PATH from cmd).
            ShellFamily::Cmd => {
                "echo before & powershell -NoProfile -Command \"Start-Sleep -Seconds 10\" & echo after".into()
            },
            // POSIX shells (bash, zsh, Git Bash, WSL) all provide native `sleep`.
            ShellFamily::Posix | ShellFamily::Wsl => "echo before; sleep 10; echo after".into(),
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
    async fn shell_captures_stdout_and_stderr_without_live_events() {
        let tool = ShellTool {
            working_dir: std::env::current_dir().expect("cwd should exist"),
            timeout_secs: 30,
        };
        let mut ctx = empty_ctx();
        ctx.tool_call_id = Some("shell-capture".into());

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

        assert_eq!(result.call_id, "shell-capture");
        assert!(!result.is_error, "{result:?}");
        assert!(result.content.contains("out"));
        assert!(result.content.contains("err"));
        assert_eq!(result.metadata["streamed"], serde_json::json!(false));
        assert_eq!(
            result.metadata["intent"],
            serde_json::json!("Check stdout and stderr capture")
        );
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
        assert_eq!(result.metadata["streamed"], serde_json::json!(false));
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
        assert_eq!(result.metadata["streamed"], serde_json::json!(false));
        assert_eq!(result.metadata["timedOut"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn shell_run_in_background_returns_shell_id() {
        let tool = ShellTool {
            working_dir: std::env::current_dir().expect("cwd should exist"),
            timeout_secs: 30,
        };
        let sleep_cmd: String = match resolve_shell().family {
            ShellFamily::PowerShell => "Start-Sleep -Seconds 5".into(),
            ShellFamily::Cmd => "timeout /t 5 /nobreak >nul".into(),
            ShellFamily::Posix | ShellFamily::Wsl => "sleep 5".into(),
        };
        let result = tool
            .execute(
                serde_json::json!({
                    "command": sleep_cmd,
                    "runInBackground": true,
                    "intent": "Long sleep in background"
                }),
                &empty_ctx(),
            )
            .await
            .expect("background shell should start");

        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.metadata["backgrounded"], serde_json::json!(true));
        let shell_id = result.metadata["shellId"]
            .as_str()
            .expect("shellId metadata");
        assert!(!shell_id.is_empty());
        assert!(result.content.contains(shell_id));

        let status =
            super::execute_background_shell_wait(shell_id, 0, Instant::now(), &empty_ctx())
                .await
                .expect("status query");
        assert_eq!(status.metadata["running"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn capture_stream_truncates_but_keeps_draining() {
        use tokio::io::AsyncWriteExt;

        let total = MAX_CAPTURE_BYTES_PER_STREAM + 4096;
        let (mut writer, reader) = tokio::io::duplex(total);
        tokio::spawn(async move {
            writer
                .write_all(&vec![b'x'; total])
                .await
                .expect("write test payload");
        });

        let captured = capture_stream(reader).await;
        assert!(captured.truncated);
        assert_eq!(captured.bytes_read, total);
        assert_eq!(captured.text.len(), MAX_CAPTURE_BYTES_PER_STREAM);
    }

    #[tokio::test]
    async fn shell_id_and_command_are_mutually_exclusive() {
        let tool = ShellTool {
            working_dir: std::env::current_dir().expect("cwd should exist"),
            timeout_secs: 30,
        };
        let result = tool
            .execute(
                serde_json::json!({
                    "command": "echo hi",
                    "shellId": "fake-id",
                }),
                &empty_ctx(),
            )
            .await;
        let err = result.expect_err("should reject shellId + command");
        let msg = format!("{err}");
        assert!(
            msg.contains("cannot specify both shellId and command"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test]
    async fn shell_id_and_run_in_background_are_mutually_exclusive() {
        let tool = ShellTool {
            working_dir: std::env::current_dir().expect("cwd should exist"),
            timeout_secs: 30,
        };
        let result = tool
            .execute(
                serde_json::json!({
                    "shellId": "fake-id",
                    "runInBackground": true,
                }),
                &empty_ctx(),
            )
            .await;
        let err = result.expect_err("should reject shellId + runInBackground");
        let msg = format!("{err}");
        assert!(
            msg.contains("cannot specify both shellId and runInBackground"),
            "unexpected error: {msg}"
        );
    }
}
