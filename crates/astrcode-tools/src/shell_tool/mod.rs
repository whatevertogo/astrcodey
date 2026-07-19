//! Shell execution tool — waits for completion and returns captured stdout/stderr.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Instant,
};

use astrcode_core::{tool::*, tool_access::ResourceAccess};
use astrcode_support::{hostpaths::resolve_path, shell::resolve_shell};
use serde::Deserialize;
use tokio::process::Command;

use crate::files::tool_call_id;

mod background;
mod definition;
mod output;
mod process;

use background::execute_background_shell_wait;
use definition::shell_tool_definition;
#[cfg(test)]
use output::capture_stream;
use output::{
    BackgroundTransfer, CapturedOutput, capture_stream_with_background_transfer,
    detect_shell_output_diagnostic, foreground_shell_metadata, is_auto_background_allowed,
    render_shell_output,
};
pub(crate) use process::{
    command_args, hide_command_window, preprocess_shell_command, setup_process_group,
    terminate_child_tree,
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
}

enum ForegroundWaitOutcome {
    Completed(std::io::Result<std::process::ExitStatus>),
    TimedOut,
}
// TODO: sandbox support — execute commands in isolated environment
// TODO: execpolicy — command allow/deny rules (via extensions)

#[cfg(test)]
mod tests;
