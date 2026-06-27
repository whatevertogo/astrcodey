//! Shell execution tool — waits for completion and returns captured stdout/stderr.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

use astrcode_core::{tool::*, tool_access::ResourceAccess};
use astrcode_support::{
    hostpaths::resolve_path,
    shell::{ShellFamily, ShellInfo, resolve_shell},
};
use regex::Regex;
use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::Notify,
};

use crate::{
    background_shell::{
        AdoptBackgroundShellParams, BackgroundShellSpawnParams, DEFAULT_STATUS_OUTPUT_MAX_TOKENS,
        MAX_STATUS_OUTPUT_MAX_TOKENS, adopt_running_shell, append_shell_output,
        spawn_background_shell, wait_background_shell,
    },
    files::tool_call_id,
};

/// 前台命令超过此时间仍运行时，自动收编为后台 shell（参考 Claude Code assistant blocking budget）。
const AUTO_BACKGROUND_AFTER_MS: u64 = 30_000;

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
    /// 为 true 时在后台运行，立即返回 `shellId`；之后通过 `shellId` 查询增量输出。
    #[serde(default)]
    run_in_background: Option<bool>,
    /// 已有后台 shell 的 id；提供时忽略 `command`，用于等待或查询状态。
    #[serde(default, rename = "shellId")]
    shell_id: Option<String>,
    /// 与 `shellId` 联用：最多阻塞等待的毫秒数（0 表示立即返回当前状态）。
    #[serde(default, rename = "blockUntilMs")]
    block_until_ms: Option<u64>,
    /// 与 `shellId` 联用：本次增量输出预览的 token 预算。
    #[serde(default, rename = "maxOutputTokens")]
    max_output_tokens: Option<usize>,
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
                args.max_output_tokens,
                started_at,
                ctx,
            )
            .await;
        }

        if args.max_output_tokens.is_some() {
            return Err(ToolError::InvalidArguments(
                "maxOutputTokens can only be used with shellId background-shell polling".into(),
            ));
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
        return self.execute_foreground_shell(args, started_at, ctx).await;
    }
}

impl ShellTool {
    async fn execute_foreground_shell(
        &self,
        args: ShellArgs,
        started_at: Instant,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let shell = resolve_shell();
        let command = preprocess_shell_command(&args.command, &shell);
        let command_args = command_args(&shell, &command);
        let cwd = args
            .cwd
            .as_deref()
            .map(|cwd| resolve_path(&self.working_dir, cwd))
            .unwrap_or_else(|| self.working_dir.clone());
        let timeout_secs = args.timeout.unwrap_or(self.timeout_secs).min(600);
        let can_auto_background =
            is_auto_background_allowed(&command) && ctx.capabilities.session.ops.is_some();

        let mut command_builder = Command::new(&shell.path);
        command_builder
            .args(&command_args)
            .current_dir(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        hide_command_window(&mut command_builder);
        setup_process_group(&mut command_builder);

        if args.stdin.is_some() {
            command_builder.stdin(Stdio::piped());
        } else {
            command_builder.stdin(Stdio::null());
        }

        let mut child = Some(
            command_builder
                .spawn()
                .map_err(|e| ToolError::Execution(format!("spawn: {e}")))?,
        );

        if let Some(input) = &args.stdin {
            if let Some(stdin) = child.as_mut().and_then(|c| c.stdin.take()) {
                let mut stdin = stdin;
                use tokio::io::AsyncWriteExt;
                stdin
                    .write_all(input.as_bytes())
                    .await
                    .map_err(|e| ToolError::Execution(format!("stdin write: {e}")))?;
                drop(stdin);
            }
        }

        let stdout = child
            .as_mut()
            .and_then(|c| c.stdout.take())
            .ok_or_else(|| ToolError::Execution("failed to capture stdout".into()))?;
        let stderr = child
            .as_mut()
            .and_then(|c| c.stderr.take())
            .ok_or_else(|| ToolError::Execution("failed to capture stderr".into()))?;

        let call_id = tool_call_id(ctx);
        let transfer = BackgroundTransfer::new();
        let mut out_h = Some(tokio::spawn(capture_stream_with_background_transfer(
            stdout,
            Arc::clone(&transfer),
            false,
        )));
        let mut err_h = Some(tokio::spawn(capture_stream_with_background_transfer(
            stderr,
            Arc::clone(&transfer),
            true,
        )));

        let auto_bg_timer =
            tokio::time::sleep(std::time::Duration::from_millis(AUTO_BACKGROUND_AFTER_MS));
        tokio::pin!(auto_bg_timer);
        let timeout_timer = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
        tokio::pin!(timeout_timer);

        let mut auto_bg_attempted = false;
        let adopt_result = loop {
            tokio::select! {
                status = async {
                    match child.as_mut() {
                        Some(c) => c.wait().await,
                        None => std::future::pending().await,
                    }
                }, if child.is_some() => {
                    break ForegroundWaitOutcome::Completed(status);
                }
                _ = &mut auto_bg_timer, if can_auto_background && !auto_bg_attempted => {
                    auto_bg_attempted = true;
                    match self.try_adopt_foreground_shell(
                        &args,
                        &command,
                        &shell,
                        &cwd,
                        timeout_secs,
                        started_at,
                        ctx,
                        &transfer,
                        &mut child,
                        &mut out_h,
                        &mut err_h,
                        "auto_background",
                    ).await {
                        Ok(result) => return Ok(result),
                        Err(e) => {
                            tracing::warn!(error = %e, "auto-background adopt failed; continuing foreground wait");
                        }
                    }
                }
                _ = &mut timeout_timer, if child.is_some() => {
                    if can_auto_background {
                        match self.try_adopt_foreground_shell(
                            &args,
                            &command,
                            &shell,
                            &cwd,
                            timeout_secs,
                            started_at,
                            ctx,
                            &transfer,
                            &mut child,
                            &mut out_h,
                            &mut err_h,
                            "timeout_background",
                        ).await {
                            Ok(result) => return Ok(result),
                            Err(e) => {
                                tracing::warn!(error = %e, "timeout adopt failed; terminating shell");
                            }
                        }
                    }
                    if let Some(ref mut running) = child {
                        terminate_child_tree(running).await;
                    }
                    break ForegroundWaitOutcome::TimedOut;
                }
            }
        };

        let (exit, timed_out) = match adopt_result {
            ForegroundWaitOutcome::Completed(status) => {
                (status.ok().and_then(|s| s.code()).unwrap_or(-1), false)
            },
            ForegroundWaitOutcome::TimedOut => {
                if let Some(running) = child.as_mut() {
                    let _ = running.wait().await;
                }
                (-1, true)
            },
        };
        let stdout_capture = out_h
            .take()
            .unwrap_or_else(|| tokio::spawn(async { CapturedOutput::default() }))
            .await
            .unwrap_or_default();
        let stderr_capture = err_h
            .take()
            .unwrap_or_else(|| tokio::spawn(async { CapturedOutput::default() }))
            .await
            .unwrap_or_default();

        let mut output = render_shell_output(&stdout_capture.text, &stderr_capture.text);

        let mut meta = foreground_shell_metadata(
            &args.command,
            args.intent.as_deref(),
            &shell,
            &cwd,
            exit,
            timed_out,
            &stdout_capture,
            &stderr_capture,
        );

        if output.is_empty() {
            output = "(no output)".into();
        }

        let diagnostic = detect_shell_output_diagnostic(&output);
        if let Some(diagnostic) = diagnostic {
            meta.insert("semanticError".into(), serde_json::json!(diagnostic.code()));
            meta.insert(
                "semanticErrorMessage".into(),
                serde_json::json!(diagnostic.message()),
            );
            if output == "(no output)" {
                output = diagnostic.message().to_string();
            } else {
                output = format!("{}\n\n{}", diagnostic.message(), output);
            }
        }

        let is_error = timed_out || exit != 0 || diagnostic.is_some();
        let error = if timed_out {
            let timeout_msg = format!("shell command timed out after {timeout_secs}s");
            if output == "(no output)" {
                output = timeout_msg.clone();
            } else {
                output.push_str("\n\n");
                output.push_str(&timeout_msg);
            }
            Some(timeout_msg)
        } else if let Some(diagnostic) = diagnostic {
            Some(diagnostic.message().to_string())
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

    #[allow(clippy::too_many_arguments)]
    async fn try_adopt_foreground_shell(
        &self,
        args: &ShellArgs,
        command: &str,
        shell: &ShellInfo,
        cwd: &Path,
        timeout_secs: u64,
        started_at: Instant,
        ctx: &ToolExecutionContext,
        transfer: &BackgroundTransfer,
        child: &mut Option<tokio::process::Child>,
        out_h: &mut Option<tokio::task::JoinHandle<CapturedOutput>>,
        err_h: &mut Option<tokio::task::JoinHandle<CapturedOutput>>,
        reason: &str,
    ) -> Result<ToolResult, ToolError> {
        let shell_id = uuid::Uuid::new_v4().to_string();
        let output_dir = resolve_background_output_dir(ctx, cwd)?;
        std::fs::create_dir_all(&output_dir)
            .map_err(|e| ToolError::Execution(format!("create background-shells dir: {e}")))?;
        let output_path = output_dir.join(format!("{shell_id}.txt"));
        let description = args
            .intent
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| truncate_command_for_description(command));

        write_background_shell_header(&output_path, command, &description, cwd, shell, &shell_id)
            .await?;

        let remaining_timeout = timeout_secs
            .saturating_sub(started_at.elapsed().as_secs())
            .max(1);

        let out_handle = out_h
            .take()
            .ok_or_else(|| ToolError::Execution("stdout capture task missing".into()))?;
        let err_handle = err_h
            .take()
            .ok_or_else(|| ToolError::Execution("stderr capture task missing".into()))?;

        let adopted = match adopt_running_shell(
            AdoptBackgroundShellParams {
                session_id: ctx.session_id.to_string(),
                timeout_secs: remaining_timeout,
                shell_id: shell_id.clone(),
                output_path: output_path.clone(),
                child: child
                    .take()
                    .ok_or_else(|| ToolError::Execution("child already taken".into()))?,
            },
            tokio::spawn(async move {
                let _ = out_handle.await;
            }),
            tokio::spawn(async move {
                let _ = err_handle.await;
            }),
        )
        .await
        {
            Ok(adopted) => adopted,
            Err(e) => {
                return Err(e);
            },
        };

        transfer.activate(output_path.clone());

        let path = adopted.output_path.display().to_string();
        let reason_note = match reason {
            "auto_background" => format!(
                "Command exceeded {AUTO_BACKGROUND_AFTER_MS}ms and was moved to the background."
            ),
            _ => format!(
                "Command reached the {timeout_secs}s foreground timeout and was moved to the \
                 background."
            ),
        };
        let content = format!(
            "{reason_note}\nBackground shell started (shell_id: {}).\nOutput file: {path}\nYou \
             can check it with `shellId` and optional `blockUntilMs`. Use `read` on this path \
             only when you need more output than the status result returns.",
            adopted.shell_id
        );
        let mut meta = BTreeMap::new();
        meta.insert("backgrounded".into(), serde_json::json!(true));
        meta.insert("autoBackgrounded".into(), serde_json::json!(true));
        meta.insert("shellId".into(), serde_json::json!(adopted.shell_id));
        meta.insert("outputPath".into(), serde_json::json!(path));
        meta.insert("command".into(), serde_json::json!(args.command));
        if let Some(intent) = args
            .intent
            .as_ref()
            .filter(|intent| !intent.trim().is_empty())
        {
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

enum ForegroundWaitOutcome {
    Completed(std::io::Result<std::process::ExitStatus>),
    TimedOut,
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
        let command = preprocess_shell_command(&args.command, &shell);
        let spawned = spawn_background_shell(BackgroundShellSpawnParams {
            session_id: ctx.session_id.to_string(),
            command,
            intent: args.intent.clone(),
            cwd,
            shell,
            timeout_secs,
            store_dir: ctx.capabilities.paths.store_dir.clone(),
        })
        .await?;
        let path = spawned.output_path.display().to_string();
        let content = format!(
            "Background shell started (shell_id: {}).\nOutput is being written to: {path}\nYou \
             can check it with `shellId` and optional `blockUntilMs`. Use `read` on this path \
             only when you need more output than the status result returns.",
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
    max_output_tokens: Option<usize>,
    started_at: Instant,
    ctx: &ToolExecutionContext,
) -> Result<ToolResult, ToolError> {
    let status = match wait_background_shell(shell_id, block_until_ms, max_output_tokens).await {
        Ok(status) => status,
        Err(ToolError::InvalidArguments(message)) if is_unknown_background_shell(&message) => {
            return Ok(stale_background_shell_result(
                shell_id, message, started_at, ctx,
            ));
        },
        Err(error) => return Err(error),
    };
    let path = status.output_path.display().to_string();
    let output_label = if status.running {
        "New output"
    } else {
        "Final output"
    };
    let output = if status.output.trim().is_empty() {
        "(no new output)".to_string()
    } else {
        status.output
    };
    let has_new_output = output != "(no new output)";
    let rendered_output = if has_new_output {
        output
    } else if status.running {
        "No new output since the previous background-shell poll.".to_string()
    } else {
        "No new output remained when the shell finished. Read the output file only if you need to \
         inspect earlier output."
            .to_string()
    };
    let diagnostic = if status.running {
        None
    } else {
        detect_shell_output_diagnostic(&rendered_output)
    };
    let content = if status.running {
        format!(
            "Shell {shell_id} is still running.\nOutput file: \
             {path}\n\n{output_label}:\n{rendered_output}"
        )
    } else if let Some(diagnostic) = diagnostic {
        format!(
            "{}\n\nShell {shell_id} finished (status: {}, exit_code: {:?}).\nOutput file: \
             {path}\n\n{output_label}:\n{rendered_output}",
            diagnostic.message(),
            status.status,
            status.exit_code
        )
    } else {
        format!(
            "Shell {shell_id} finished (status: {}, exit_code: {:?}).\nOutput file: \
             {path}\n\n{output_label}:\n{rendered_output}",
            status.status, status.exit_code
        )
    };
    let is_error =
        matches!(status.status.as_str(), "failed" | "timed_out" | "killed") || diagnostic.is_some();
    let mut meta = BTreeMap::new();
    meta.insert("shellId".into(), serde_json::json!(shell_id));
    meta.insert("outputPath".into(), serde_json::json!(path));
    meta.insert("running".into(), serde_json::json!(status.running));
    meta.insert(
        "outputTruncated".into(),
        serde_json::json!(status.output_truncated),
    );
    meta.insert("hasNewOutput".into(), serde_json::json!(has_new_output));
    meta.insert(
        "outputTokens".into(),
        serde_json::json!(status.output_tokens),
    );
    meta.insert(
        "returnedOutputTokens".into(),
        serde_json::json!(status.returned_output_tokens),
    );
    meta.insert(
        "omittedOutputTokens".into(),
        serde_json::json!(status.omitted_output_tokens),
    );
    meta.insert(
        "maxOutputTokens".into(),
        serde_json::json!(status.max_output_tokens),
    );
    meta.insert("status".into(), serde_json::json!(status.status));
    if let Some(code) = status.exit_code {
        meta.insert("exitCode".into(), serde_json::json!(code));
    }
    if let Some(diagnostic) = diagnostic {
        meta.insert("semanticError".into(), serde_json::json!(diagnostic.code()));
        meta.insert(
            "semanticErrorMessage".into(),
            serde_json::json!(diagnostic.message()),
        );
    }
    Ok(ToolResult {
        call_id: tool_call_id(ctx),
        content,
        is_error,
        error: if let Some(diagnostic) = diagnostic {
            Some(diagnostic.message().to_string())
        } else {
            is_error.then(|| format!("background shell {}", status.status))
        },
        metadata: meta,
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
    })
}

fn is_unknown_background_shell(message: &str) -> bool {
    message.starts_with("unknown shell_id:")
}

fn stale_background_shell_result(
    shell_id: &str,
    message: String,
    started_at: Instant,
    ctx: &ToolExecutionContext,
) -> ToolResult {
    let content = format!(
        "Shell {shell_id} is not active in this server process.\nStatus: \
         unknown_stale_shell_id.\n\nThis usually means the shellId came from an older server \
         process, an older session, or a session cleanup. The shell is not running here, and \
         polling this shellId again will not produce output. Stop polling this shellId. If an \
         earlier shell result showed an output file path, use `read` on that path only if you \
         still need to inspect saved output."
    );
    let mut meta = BTreeMap::new();
    meta.insert("shellId".into(), serde_json::json!(shell_id));
    meta.insert("running".into(), serde_json::json!(false));
    meta.insert("status".into(), serde_json::json!("unknown_stale_shell_id"));
    meta.insert("hasNewOutput".into(), serde_json::json!(false));
    meta.insert("outputTruncated".into(), serde_json::json!(false));
    meta.insert("outputTokens".into(), serde_json::json!(0));
    meta.insert("returnedOutputTokens".into(), serde_json::json!(0));
    meta.insert("omittedOutputTokens".into(), serde_json::json!(0));
    meta.insert(
        "maxOutputTokens".into(),
        serde_json::json!(DEFAULT_STATUS_OUTPUT_MAX_TOKENS),
    );
    meta.insert("staleShellId".into(), serde_json::json!(true));
    meta.insert("diagnostic".into(), serde_json::json!(message));
    ToolResult {
        call_id: tool_call_id(ctx),
        content,
        is_error: false,
        error: None,
        metadata: meta,
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
    }
}

#[derive(Clone, Copy)]
enum ShellOutputDiagnostic {
    SudoAuthenticationRequired,
}

impl ShellOutputDiagnostic {
    fn code(self) -> &'static str {
        match self {
            Self::SudoAuthenticationRequired => "sudo_authentication_required",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::SudoAuthenticationRequired => {
                "Command output indicates sudo authentication failed. Treat this command as failed \
                 even if the shell exit code was zero, because a pipeline may have hidden the \
                 original failure. Do not retry with sudo; report the missing privilege or \
                 dependency as blocked."
            },
        }
    }
}

fn detect_shell_output_diagnostic(output: &str) -> Option<ShellOutputDiagnostic> {
    let lower = output.to_ascii_lowercase();
    let sudo_auth_failed = [
        "sudo: a terminal is required",
        "sudo: a password is required",
        "sudo: no tty present",
        "sudo: sorry, you must have a tty",
        "sudo: no askpass program specified",
    ]
    .iter()
    .any(|needle| lower.contains(needle));

    sudo_auth_failed.then_some(ShellOutputDiagnostic::SudoAuthenticationRequired)
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
                "- Foreground commands expected to exceed ~30s (builds, large scans, sleeps, \
                 network fetches) → set `runInBackground` to true; do not block the turn waiting \
                 for output.\n",
                "- One-shot foreground timeout up to 600s (default {timeout_secs}s); background \
                 up to 600s.\n",
                "- Long-running commands: `runInBackground` returns immediately with a `shellId`. \
                 Check it later with `shellId` and optional `blockUntilMs`; output is written \
                 under session background-shells/.\n",
                "- Foreground commands still running after ~30s may be auto-moved to background \
                 (same as `runInBackground`) and can be checked with the returned `shellId`.\n",
                "- Poll/wait on a background shell with `shellId` and optional `blockUntilMs` (0 \
                 = status only). Each poll returns only output written since the previous poll.\n",
                "- Background-shell poll output is token-budgeted (default {default_poll_tokens} \
                 tokens, max {max_poll_tokens}); large increments are shown as head+tail previews \
                 with omitted-token counts.\n",
                "- A completed background shell remains queryable through `shellId`; repeated \
                 polls return completed status with only newly written output, usually none. Stop \
                 polling once completed unless you need to inspect the output file.\n",
                "- If a `shellId` is reported as `unknown_stale_shell_id`, it belongs to an older \
                 server process/session or was cleaned up; stop polling that id.\n",
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
            default_poll_tokens = DEFAULT_STATUS_OUTPUT_MAX_TOKENS,
            max_poll_tokens = MAX_STATUS_OUTPUT_MAX_TOKENS,
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
                },
                "runInBackground": {
                    "type": "boolean",
                    "description": "Run in background when the command may take more than ~30s (builds, scans, sleep). Returns shellId immediately; use shellId to poll for incremental output and final status."
                },
                "shellId": {
                    "type": "string",
                    "description": "Existing background shell id. Omit command; returns output written since the previous poll plus current status."
                },
                "blockUntilMs": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "With shellId: max ms to wait for new output or completion (0 = immediate status)."
                },
                "maxOutputTokens": {
                    "type": "integer",
                    "minimum": 256,
                    "maximum": MAX_STATUS_OUTPUT_MAX_TOKENS,
                    "description": "With shellId only: token budget for this poll's incremental output preview. Defaults to 10000, matching Codex unified exec. Large increments return a head+tail preview with omittedOutputTokens metadata."
                }
            },
            "required": [],
            "additionalProperties": false
        }),
    };
    definitions.insert(key, definition.clone());
    definition
}

/// Windows 上在 POSIX shell（Git Bash / MSYS）里，将 `>nul` / `2>nul` 改写为 `/dev/null`，
/// 避免创建名为 `nul` 的 literal 文件（Windows 保留设备名）。
pub(crate) fn preprocess_shell_command(command: &str, shell: &ShellInfo) -> String {
    if !cfg!(windows) || shell.family != ShellFamily::Posix {
        return command.to_string();
    }
    static NUL_REDIRECT: OnceLock<Regex> = OnceLock::new();
    let re = NUL_REDIRECT.get_or_init(|| {
        Regex::new(r"(\d?&?>+\s*)[Nn][Uu][Ll](\s|$|[|&;)\n\r])")
            .expect("nul redirect regex must compile")
    });
    re.replace_all(command, "${1}/dev/null${2}").into_owned()
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

/// 前台执行期间可切换为写 background-shell 输出文件。
struct BackgroundTransfer {
    path: Mutex<Option<PathBuf>>,
    notify: Notify,
}

impl BackgroundTransfer {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            path: Mutex::new(None),
            notify: Notify::new(),
        })
    }

    fn activate(&self, path: PathBuf) {
        *self.path.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
        self.notify.notify_waiters();
    }
}

fn is_auto_background_allowed(command: &str) -> bool {
    let first = command
        .split(['|', '&', ';'])
        .next()
        .unwrap_or(command)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    !matches!(
        first.as_str(),
        "sleep" | "start-sleep" | "timeout" | "ping" | "await"
    )
}

fn resolve_background_output_dir(
    ctx: &ToolExecutionContext,
    cwd: &Path,
) -> Result<PathBuf, ToolError> {
    if let Some(dir) = &ctx.capabilities.paths.store_dir {
        return Ok(dir.join("background-shells"));
    }
    Ok(cwd.join(".astrcode").join("background-shells"))
}

fn truncate_command_for_description(command: &str) -> String {
    const MAX: usize = 80;
    let trimmed = command.trim();
    if trimmed.chars().count() <= MAX {
        return trimmed.to_string();
    }
    let mut out = String::new();
    for ch in trimmed.chars().take(MAX.saturating_sub(1)) {
        out.push(ch);
    }
    out.push('…');
    out
}

async fn write_background_shell_header(
    path: &Path,
    command: &str,
    description: &str,
    cwd: &Path,
    shell: &ShellInfo,
    shell_id: &str,
) -> Result<(), ToolError> {
    let header = format!(
        "---\nshell_id: {shell_id}\ncommand: {}\ndescription: {}\ncwd: {}\nshell: {}\n---\n\n",
        command.replace('\n', " "),
        description.replace('\n', " "),
        cwd.display(),
        shell.name,
    );
    tokio::fs::write(path, header.as_bytes())
        .await
        .map_err(|e| ToolError::Execution(format!("write background shell header: {e}")))
}

#[allow(clippy::too_many_arguments)]
fn foreground_shell_metadata(
    command: &str,
    intent: Option<&str>,
    shell: &ShellInfo,
    cwd: &Path,
    exit: i32,
    timed_out: bool,
    stdout_capture: &CapturedOutput,
    stderr_capture: &CapturedOutput,
) -> BTreeMap<String, serde_json::Value> {
    let mut meta = BTreeMap::new();
    meta.insert("command".into(), serde_json::json!(command));
    if let Some(intent) = intent.filter(|intent| !intent.trim().is_empty()) {
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
                "Output exceeded {MAX_CAPTURE_BYTES_PER_STREAM} bytes per stream; captured prefix \
                 only. Re-run with narrower scope or redirect to a file and use `read`."
            )),
        );
    }
    meta
}

async fn capture_stream_with_background_transfer(
    mut stream: impl AsyncRead + Unpin + Send + 'static,
    transfer: Arc<BackgroundTransfer>,
    is_stderr: bool,
) -> CapturedOutput {
    let mut output = CapturedOutput::default();
    let mut buf = [0u8; 8192];
    let mut drain_buf = [0u8; 65536];
    let mut file_path: Option<PathBuf> = None;
    let mut file_kept = 0usize;
    let mut file_truncated = false;
    let mut stderr_marker_written = false;
    let mut draining = false;

    loop {
        if file_path.is_none() {
            let activated_path = {
                transfer
                    .path
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
            };
            if let Some(path) = activated_path {
                if !output.text.is_empty() {
                    if is_stderr {
                        let _ = append_shell_output(&path, b"\n--- STDERR ---\n").await;
                        stderr_marker_written = true;
                    }
                    let _ = append_shell_output(path.as_path(), output.text.as_bytes()).await;
                    file_kept += output.text.len();
                }
                file_path = Some(path);
            }
        }

        let read_future = stream.read(if draining {
            &mut drain_buf[..]
        } else {
            &mut buf[..]
        });

        let n = if file_path.is_none() {
            tokio::select! {
                _ = transfer.notify.notified() => continue,
                res = read_future => match res {
                    Ok(n) => n,
                    Err(_) => break,
                },
            }
        } else {
            match read_future.await {
                Ok(n) => n,
                Err(_) => break,
            }
        };

        if n == 0 {
            break;
        }
        output.bytes_read += n;

        if let Some(ref path) = file_path {
            if file_truncated {
                continue;
            }
            let chunk = if draining { &drain_buf[..n] } else { &buf[..n] };
            let take = if file_kept + n > MAX_CAPTURE_BYTES_PER_STREAM {
                file_truncated = true;
                MAX_CAPTURE_BYTES_PER_STREAM.saturating_sub(file_kept)
            } else {
                n
            };
            if take > 0 {
                if is_stderr && !stderr_marker_written {
                    if append_shell_output(path.as_path(), b"\n--- STDERR ---\n")
                        .await
                        .is_err()
                    {
                        break;
                    }
                    stderr_marker_written = true;
                }
                if append_shell_output(path.as_path(), &chunk[..take])
                    .await
                    .is_err()
                {
                    break;
                }
                file_kept += take;
            }
            if file_truncated {
                let note = format!(
                    "\n[output truncated at {MAX_CAPTURE_BYTES_PER_STREAM} bytes per stream]\n"
                );
                let _ = append_shell_output(path.as_path(), note.as_bytes()).await;
            }
            continue;
        }

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

/// 读取子进程输出直到 EOF；超出 [`MAX_CAPTURE_BYTES_PER_STREAM`] 后仍继续 drain pipe
/// 以免子进程写端阻塞。保留供单元测试与简单路径使用。
#[allow(dead_code)]
async fn capture_stream(mut stream: impl AsyncRead + Unpin) -> CapturedOutput {
    let mut output = CapturedOutput::default();
    let mut buf = [0u8; 8192];
    // Larger buffer for the drain-only phase — fewer syscalls, same correctness.
    let mut drain_buf = [0u8; 65536];
    let mut draining = false;
    loop {
        let read_buf = if draining {
            &mut drain_buf[..]
        } else {
            &mut buf[..]
        };
        let Ok(n) = stream.read(read_buf).await else {
            break;
        };
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

    use super::{
        MAX_CAPTURE_BYTES_PER_STREAM, ShellTool, capture_stream, command_args,
        preprocess_shell_command,
    };

    fn empty_ctx() -> ToolExecutionContext {
        ToolExecutionContext::new(
            String::new().into(),
            String::new(),
            None,
            None,
            ToolCapabilities::default(),
        )
    }

    fn ctx_with_session(session_id: &str) -> ToolExecutionContext {
        ToolExecutionContext::new(
            session_id.to_string().into(),
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
            // Use `cmd /c echo` to bypass PowerShell's .NET output buffering.
            // `[Console]::Out.WriteLine` + `.Flush()` may not reach the OS pipe before
            // `taskkill /F` destroys the process, losing the buffered data.
            ShellFamily::PowerShell => {
                "cmd /c echo before; Start-Sleep -Seconds 10; cmd /c echo after".into()
            },
            // cmd.exe has no built-in sleep; delegate to powershell (always on PATH from cmd).
            ShellFamily::Cmd => "echo before & powershell -NoProfile -Command \"Start-Sleep \
                                 -Seconds 10\" & echo after"
                .into(),
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

    fn command_with_sudo_auth_failure_output_and_zero_exit() -> String {
        match resolve_shell().family {
            ShellFamily::PowerShell => "Write-Output 'sudo: a terminal is required to read the \
                                        password'; Write-Output 'sudo: a password is required'; \
                                        exit 0"
                .into(),
            ShellFamily::Cmd => "echo sudo: a terminal is required to read the password & echo \
                                 sudo: a password is required & exit /b 0"
                .into(),
            ShellFamily::Posix | ShellFamily::Wsl => "printf '%s\\n' 'sudo: a terminal is \
                                                      required to read the password' 'sudo: a \
                                                      password is required'; exit 0"
                .into(),
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

    #[test]
    fn preprocess_shell_command_rewrites_nul_redirect_on_windows_posix() {
        let bash = ShellInfo {
            family: ShellFamily::Posix,
            name: "bash".into(),
            path: "bash".into(),
        };
        if cfg!(windows) {
            assert_eq!(
                preprocess_shell_command("cmd 2>nul || echo ok", &bash),
                "cmd 2>/dev/null || echo ok"
            );
            assert_eq!(
                preprocess_shell_command("echo \">nul\"", &bash),
                "echo \">nul\""
            );
        } else {
            assert_eq!(
                preprocess_shell_command("cmd 2>nul || echo ok", &bash),
                "cmd 2>nul || echo ok"
            );
        }
    }

    #[test]
    fn preprocess_shell_command_skips_powershell_on_windows() {
        let powershell = ShellInfo {
            family: ShellFamily::PowerShell,
            name: "powershell".into(),
            path: "powershell.exe".into(),
        };
        let command = "cmd 2>nul";
        assert_eq!(preprocess_shell_command(command, &powershell), command);
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

    #[tokio::test]
    async fn shell_sudo_auth_failure_output_is_error_even_with_zero_exit() {
        let tool = ShellTool {
            working_dir: std::env::current_dir().expect("cwd should exist"),
            timeout_secs: 30,
        };

        let result = tool
            .execute(
                serde_json::json!({
                    "command": command_with_sudo_auth_failure_output_and_zero_exit()
                }),
                &empty_ctx(),
            )
            .await
            .expect("shell should execute");

        assert!(
            result.is_error,
            "sudo auth failure output should be treated as a semantic error: {result:?}"
        );
        assert_eq!(result.metadata["exitCode"], serde_json::json!(0));
        assert_eq!(
            result.metadata["semanticError"],
            serde_json::json!("sudo_authentication_required")
        );
        assert!(
            result.content.contains("pipeline may have hidden"),
            "diagnostic should explain the hidden failure: {}",
            result.content
        );
    }

    #[test]
    fn auto_background_disallows_sleep_commands() {
        assert!(!super::is_auto_background_allowed("sleep 60"));
        assert!(!super::is_auto_background_allowed("Start-Sleep -Seconds 5"));
        assert!(super::is_auto_background_allowed("cargo build --release"));
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
            super::execute_background_shell_wait(shell_id, 0, None, Instant::now(), &empty_ctx())
                .await
                .expect("status query");
        assert_eq!(status.metadata["running"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn completed_background_shell_can_be_polled_repeatedly_without_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let session_id = format!("sess-repeat-terminal-{}", uuid::Uuid::new_v4());
        let ctx = ctx_with_session(&session_id);
        let tool = ShellTool {
            working_dir: temp.path().to_path_buf(),
            timeout_secs: 30,
        };
        let command: String = match resolve_shell().family {
            ShellFamily::PowerShell => "Write-Output done".into(),
            ShellFamily::Cmd => "echo done".into(),
            ShellFamily::Posix | ShellFamily::Wsl => "echo done".into(),
        };

        let started = tool
            .execute(
                serde_json::json!({
                    "command": command,
                    "runInBackground": true,
                    "intent": "Finish quickly in background"
                }),
                &ctx,
            )
            .await
            .expect("background shell should start");
        let shell_id = started.metadata["shellId"]
            .as_str()
            .expect("shellId metadata");

        let final_poll = tool
            .execute(
                serde_json::json!({
                    "shellId": shell_id,
                    "blockUntilMs": 5_000
                }),
                &ctx,
            )
            .await
            .expect("first terminal poll should succeed");
        assert!(!final_poll.is_error, "{final_poll:?}");
        assert_eq!(final_poll.metadata["running"], serde_json::json!(false));
        assert_eq!(
            final_poll.metadata["status"],
            serde_json::json!("completed")
        );
        assert!(
            final_poll.content.contains("done"),
            "first terminal poll should include final output: {}",
            final_poll.content
        );

        let repeated_poll = tool
            .execute(
                serde_json::json!({
                    "shellId": shell_id,
                    "blockUntilMs": 0
                }),
                &ctx,
            )
            .await
            .expect("repeated terminal poll should not become unknown shell_id");
        assert!(!repeated_poll.is_error, "{repeated_poll:?}");
        assert_eq!(repeated_poll.metadata["running"], serde_json::json!(false));
        assert_eq!(
            repeated_poll.metadata["hasNewOutput"],
            serde_json::json!(false)
        );
        assert!(
            repeated_poll
                .content
                .contains("No new output remained when the shell finished"),
            "repeated poll should tell the model to stop polling: {}",
            repeated_poll.content
        );

        crate::background_shell::cleanup_background_shells_for_session(&session_id);
    }

    #[tokio::test]
    async fn stale_background_shell_id_returns_terminal_status_instead_of_tool_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tool = ShellTool {
            working_dir: temp.path().to_path_buf(),
            timeout_secs: 30,
        };
        let stale_shell_id = format!("stale-{}", uuid::Uuid::new_v4());

        let result = tool
            .execute(
                serde_json::json!({
                    "shellId": stale_shell_id,
                    "blockUntilMs": 0
                }),
                &ctx_with_session("sess-stale-shell-id"),
            )
            .await
            .expect("stale shellId should be a terminal status result");

        assert!(!result.is_error, "{result:?}");
        assert_eq!(result.metadata["running"], serde_json::json!(false));
        assert_eq!(
            result.metadata["status"],
            serde_json::json!("unknown_stale_shell_id")
        );
        assert_eq!(result.metadata["staleShellId"], serde_json::json!(true));
        assert!(
            result.content.contains("Stop polling this shellId"),
            "stale shellId response must tell the model to stop: {}",
            result.content
        );
        assert!(
            !result.content.contains("Invalid arguments"),
            "stale shellId should not be framed as a parameter error: {}",
            result.content
        );
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

    #[tokio::test]
    async fn max_output_tokens_requires_shell_id() {
        let tool = ShellTool {
            working_dir: std::env::current_dir().expect("cwd should exist"),
            timeout_secs: 30,
        };
        let result = tool
            .execute(
                serde_json::json!({
                    "command": "echo hi",
                    "maxOutputTokens": 256,
                }),
                &empty_ctx(),
            )
            .await;
        let err = result.expect_err("maxOutputTokens without shellId should fail");
        assert!(
            err.to_string().contains("maxOutputTokens can only be used"),
            "unexpected error: {err}"
        );
    }
}
