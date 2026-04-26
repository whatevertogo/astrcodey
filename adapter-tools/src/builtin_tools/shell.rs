//! # Shell 工具
//!
//! 实现 `shell` 内置工具，用于执行一次性非交互式 shell 命令。
//!
//! ## 架构设计
//!
//! Shell 工具是工具系统中最复杂的组件之一，需要处理：
//! - **流式输出**: stdout/stderr 通过独立线程实时读取并增量广播到前端
//! - **UTF-8 安全**: 跨读取边界的碎片 UTF-8 序列必须正确拼接，不能截断多字节字符
//! - **输出截断**: 防止超长输出导致内存溢出或前端渲染卡顿
//! - **取消支持**: 用户取消时立即 kill 子进程并清理资源
//! - **跨平台**: 默认优先继承当前环境使用的 shell，再回退到平台兜底链
//!
//! ## 流式读取机制
//!
//! 子进程的 stdout/stderr 各由一个独立线程读取（`spawn_stream_reader`），
//! 按行分割并通过 `ctx.emit_stdout`/`ctx.emit_stderr` 增量广播。
//! 前端基于 `metadata.display.kind = "terminal"` 渲染终端视图，
//! 断线重连后通过 replay 恢复完整输出。
//!
//! ## 为什么不用异步 I/O
//!
//! `std::process::Command` 的 stdout/stderr 是同步 `Read` trait，
//! 无法直接 await。使用 `thread::spawn` 将阻塞读取移到后台线程，
//! 主线程轮询子进程退出状态，两者通过 `JoinHandle` 同步。

use std::{
    io::Read,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use astrcode_core::{AstrError, ResolvedShell, Result, ShellFamily, SideEffect};
use astrcode_runtime_contract::tool::{
    Tool, ToolCapabilityMetadata, ToolContext, ToolDefinition, ToolExecutionResult,
    ToolOutputStream, ToolPromptMetadata,
};
use astrcode_support::{
    shell::{default_shell_label, resolve_shell},
    tool_results::maybe_persist_tool_result,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::builtin_tools::fs_common::{
    check_cancel, merge_persisted_tool_output_metadata, resolve_path, session_dir_for_tool_results,
};

/// Shell 工具实现。
///
/// 执行一条非交互式 shell 命令，返回 stdout/stderr/exitCode。
/// 支持流式输出、取消、跨平台 shell 自动检测。
#[derive(Default)]
pub struct ShellTool;

/// Shell 工具的反序列化参数。
///
/// `command` 是必填项；`cwd` 可选，未指定时使用上下文工作目录。
/// shell 始终由运行时根据当前环境解析，避免模型误选 shell family。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShellArgs {
    command: String,
    #[serde(default)]
    cwd: Option<PathBuf>,
    /// 超时参数（秒），默认 120，上限 300。
    #[serde(default)]
    timeout: Option<u64>,
}

/// 平台相关的 shell 命令规范。
///
/// 将用户输入的 command 字符串转换为具体的可执行程序 + 参数列表，
/// 屏蔽 PowerShell/cmd/Posix/WSL 的参数差异。
#[derive(Debug)]
struct CommandSpec {
    program: String,
    args: Vec<String>,
    display_shell: String,
}

/// 流输出捕获器，负责增量收集、截断控制和截断通知。
///
/// ## 为什么需要独立的 StreamCapture
///
/// 子进程可能产生任意大小的输出，不能全部缓存在内存中。
/// `StreamCapture` 在流式读取的同时维护累计文本和字节计数，
/// 达到 `limit` 后停止追加内容但继续计数，
/// 最终在输出末尾附加截断通知，让前端和用户知道有内容被省略。
struct StreamCapture {
    text: String,
    bytes_read: usize,
    truncated: bool,
    limit: usize,
    stream: ToolOutputStream,
}

impl StreamCapture {
    /// 创建新的流捕获器。
    ///
    /// `limit` 是单个流（stdout 或 stderr）的最大字符数预算。
    fn new(stream: ToolOutputStream, limit: usize) -> Self {
        Self {
            text: String::new(),
            bytes_read: 0,
            truncated: false,
            limit,
            stream,
        }
    }

    /// 将新数据块追加到捕获缓冲区，返回可发送给前端的可见文本。
    ///
    /// ## 截断策略
    ///
    /// 当累计文本达到 `limit` 时，后续内容不再追加到 `text`，
    /// 但 `bytes_read` 继续计数以反映真实输出量。
    /// 首次触发截断时会在末尾附加一条截断通知（如 `[stdout truncated after N bytes]`），
    /// 后续调用返回空字符串，避免重复通知。
    fn push_chunk(&mut self, chunk: &str) -> String {
        self.bytes_read = self.bytes_read.saturating_add(chunk.len());
        if self.truncated || chunk.is_empty() {
            return String::new();
        }

        let remaining = self.limit.saturating_sub(self.text.len());
        if remaining == 0 {
            self.truncated = true;
            return self.append_truncation_notice();
        }

        let take_len = chunk.floor_char_boundary(remaining.min(chunk.len()));
        let visible = &chunk[..take_len];
        self.text.push_str(visible);

        let mut emitted = visible.to_string();
        if take_len < chunk.len() {
            self.truncated = true;
            let notice = self.append_truncation_notice();
            emitted.push_str(&notice);
        }

        emitted
    }

    /// 生成截断通知文本并追加到内部缓冲区。
    ///
    /// 通知格式如 `\n... [stdout truncated after 65536 bytes; later output omitted]\n`，
    /// 让前端和用户明确知道输出被截断以及截断前的字节量。
    fn append_truncation_notice(&mut self) -> String {
        let label = match self.stream {
            ToolOutputStream::Stdout => "stdout",
            ToolOutputStream::Stderr => "stderr",
        };
        let notice = format!(
            "\n... [{label} truncated after {} bytes; later output omitted]\n",
            self.limit
        );
        self.text.push_str(&notice);
        notice
    }
}

/// 在后台线程中从 `Read` 流读取数据并增量广播。
///
/// ## 为什么需要独立线程
///
/// `std::process::ChildStdout` 实现的是同步 `Read` trait，
/// 无法在 async 上下文中直接 `.await`。此函数将阻塞读取
/// 移到 `thread::spawn` 创建的后台线程，主线程通过
/// `JoinHandle` 等待读取完成并获取最终捕获结果。
///
/// ## UTF-8 碎片处理
///
/// 每次 `read()` 可能返回不完整的 UTF-8 序列（如一个 3 字节中文字符
/// 被拆到两次 read 之间）。`drain_decoded_utf8` 负责将完整字符
/// 解码出来，不完整的字节保留到下次 read 再尝试。
///
/// ## 行缓冲与强制刷新
///
/// 默认按 `\n` 分割后逐行广播，保证前端终端视图的渲染粒度。
/// 当 pending 缓冲区超过 4096 字节（超长无换行输出）时强制刷新，
/// 避免单个超长行导致整个输出被缓存直到进程退出。
fn spawn_stream_reader<R: Read + Send + 'static>(
    reader: R,
    stream: ToolOutputStream,
    ctx: ToolContext,
    tool_call_id: String,
    tool_name: String,
    limit: usize,
) -> thread::JoinHandle<std::result::Result<StreamCapture, std::io::Error>> {
    thread::spawn(move || {
        let mut capture = StreamCapture::new(stream, limit);
        let mut reader = reader;
        let mut chunk = [0u8; 4096];
        let mut pending_bytes = Vec::new();
        let mut pending = String::new();

        loop {
            let read = reader.read(&mut chunk)?;
            if read == 0 {
                // EOF: 将剩余的不完整 UTF-8 字节做 lossy 刷新，
                // 保留任何不完整的尾部 UTF-8 字节而不是静默丢弃，
                // 确保终端转录的完整性。
                if !pending_bytes.is_empty() {
                    // A final lossy flush at EOF preserves any incomplete trailing UTF-8 bytes
                    // instead of silently dropping them from the terminal transcript.
                    pending.push_str(&String::from_utf8_lossy(&pending_bytes));
                    pending_bytes.clear();
                }
                if !pending.is_empty() {
                    let visible = capture.push_chunk(&pending);
                    if !visible.is_empty() {
                        match stream {
                            ToolOutputStream::Stdout => {
                                let _ = ctx.emit_stdout(
                                    tool_call_id.clone(),
                                    tool_name.clone(),
                                    visible,
                                );
                            },
                            ToolOutputStream::Stderr => {
                                let _ = ctx.emit_stderr(
                                    tool_call_id.clone(),
                                    tool_name.clone(),
                                    visible,
                                );
                            },
                        }
                    }
                }
                break;
            }

            pending_bytes.extend_from_slice(&chunk[..read]);
            // 将已完成的 UTF-8 字符从 pending_bytes 中解码出来
            pending.push_str(&drain_decoded_utf8(&mut pending_bytes));
            // 按行分割：每遇到一个换行符就提取并广播
            while let Some(newline_index) = pending.find('\n') {
                let next_chunk = pending[..=newline_index].to_string();
                pending.replace_range(..=newline_index, "");
                let visible = capture.push_chunk(&next_chunk);
                if visible.is_empty() {
                    continue;
                }

                match stream {
                    ToolOutputStream::Stdout => {
                        let _ = ctx.emit_stdout(tool_call_id.clone(), tool_name.clone(), visible);
                    },
                    ToolOutputStream::Stderr => {
                        let _ = ctx.emit_stderr(tool_call_id.clone(), tool_name.clone(), visible);
                    },
                }
            }

            if pending.len() >= 4096 {
                // 超长无换行行：仍然需要渐进式流式输出，
                // 否则单个无换行命令可以hold住整个转录直到进程退出。
                let visible = capture.push_chunk(&pending);
                pending.clear();
                if visible.is_empty() {
                    continue;
                }

                match stream {
                    ToolOutputStream::Stdout => {
                        let _ = ctx.emit_stdout(tool_call_id.clone(), tool_name.clone(), visible);
                    },
                    ToolOutputStream::Stderr => {
                        let _ = ctx.emit_stderr(tool_call_id.clone(), tool_name.clone(), visible);
                    },
                }
            }
        }

        Ok(capture)
    })
}

/// 从待解码字节缓冲区中提取完整的 UTF-8 字符。
///
/// ## UTF-8 增量解码策略
///
/// 进程输出可能在一个多字节字符的中间被截断（如 3 字节的中文字符
/// 只读到了前 2 字节）。此函数：
/// 1. 尝试将整个 `pending_bytes` 解码为 UTF-8
/// 2. 如果成功，清空缓冲区并返回完整字符串
/// 3. 如果失败但 `error_len` 为 None，说明剩余字节是不完整序列， 保留到下次 read 再尝试（可能下一个
///    read 会补全）
/// 4. 如果 `error_len` 有值，说明是无效字节序列， 用 lossy 转换替换并继续
fn drain_decoded_utf8(pending_bytes: &mut Vec<u8>) -> String {
    let mut decoded = String::new();

    loop {
        match std::str::from_utf8(pending_bytes) {
            Ok(valid) => {
                decoded.push_str(valid);
                pending_bytes.clear();
                break;
            },
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                if valid_up_to > 0 {
                    let valid = std::str::from_utf8(&pending_bytes[..valid_up_to])
                        .expect("valid UTF-8 prefix should decode");
                    decoded.push_str(valid);
                    pending_bytes.drain(..valid_up_to);
                    continue;
                }

                let Some(invalid_len) = error.error_len() else {
                    // `error_len == None` means the remaining bytes form an incomplete UTF-8
                    // sequence that may become valid once the next read arrives, so keep them.
                    break;
                };

                decoded.push_str(&String::from_utf8_lossy(&pending_bytes[..invalid_len]));
                pending_bytes.drain(..invalid_len);
            },
        }
    }

    decoded
}

/// 将 stdout 和 stderr 组合为最终输出文本。
///
/// 根据两个流的空/非空状态选择展示策略：
/// - 都空：返回空字符串
/// - 只有一个非空：直接返回该流内容
/// - 都非空：用 `[stdout]`/`[stderr]` 标签分隔
fn render_shell_output(stdout: &str, stderr: &str) -> String {
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.to_string(),
        (true, false) => stderr.to_string(),
        (false, false) => format!("[stdout]\n{stdout}\n\n[stderr]\n{stderr}"),
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn definition(&self) -> ToolDefinition {
        let default_shell = default_shell_for_prompt();
        ToolDefinition {
            name: "shell".to_string(),
            description: format!(
                "Run a non-interactive shell command with the default shell ({default_shell}). \
                 Use for directory inspection and commands without a dedicated tool."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "cwd": { "type": "string" },
                    "timeout": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 600,
                        "description": "Timeout in seconds (default 120, max 600)"
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn capability_metadata(&self) -> ToolCapabilityMetadata {
        let default_shell = default_shell_for_prompt();
        ToolCapabilityMetadata::builtin()
            .tags(["process", "shell"])
            .permission("shell.exec")
            .side_effect(SideEffect::External)
            .prompt(
                ToolPromptMetadata::new(
                    format!(
                        "Run one-shot shell commands with `{default_shell}`. Use for directory \
                         inspection, build/test/git/system commands, and operations without a \
                         dedicated tool."
                    ),
                    format!(
                        "Use `shell` for non-interactive commands with the default shell \
                         `{default_shell}`. Use `ls`, `dir`, or `Get-ChildItem` to inspect \
                         directories. Prefer `readFile` for files, `findFiles` for path globs, \
                         `grep` for content search, and `editFile`/`writeFile`/`apply_patch` for \
                         file changes. Keep commands scoped and prefer read-only inspection \
                         before mutation."
                    ),
                )
                .caveat(
                    "Single shot only: no stdin and no interactive prompts. Use `cwd` instead of \
                     `cd &&`; set `shell` explicitly only for quoting or shell-family issues.",
                )
                .prompt_tag("shell"),
            )
            .max_result_inline_size(30_000)
    }

    async fn execute(
        &self,
        tool_call_id: String,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolExecutionResult> {
        check_cancel(ctx.cancel())?;
        let args: ShellArgs = serde_json::from_value(args)
            .map_err(|e| AstrError::parse("invalid args for shell tool", e))?;
        if args.command.trim().is_empty() {
            return Err(AstrError::Validation(
                "shell command cannot be empty".to_string(),
            ));
        }

        let spec = command_spec(&args.command)?;
        let started_at = Instant::now();
        let command_text = args.command.clone();
        let shell_display = spec.display_shell.clone();
        // 超时上限 600 秒，默认 120 秒
        let timeout_secs = args.timeout.unwrap_or(120).min(600);
        let deadline = started_at + Duration::from_secs(timeout_secs);
        let cwd = match args.cwd {
            Some(cwd) => resolve_path(ctx, &cwd)?,
            None => ctx.working_dir().to_path_buf(),
        };
        let cwd_text = cwd.to_string_lossy().to_string();
        let session_dir = session_dir_for_tool_results(ctx)?;

        let mut child = Command::new(&spec.program)
            .args(&spec.args)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| AstrError::io("failed to spawn shell command", e))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AstrError::Internal("failed to capture stdout".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AstrError::Internal("failed to capture stderr".to_string()))?;
        let stream_limit = ctx.max_output_size();
        let stdout_task = spawn_stream_reader(
            stdout,
            ToolOutputStream::Stdout,
            ctx.clone(),
            tool_call_id.clone(),
            "shell".to_string(),
            stream_limit,
        );
        let stderr_task = spawn_stream_reader(
            stderr,
            ToolOutputStream::Stderr,
            ctx.clone(),
            tool_call_id.clone(),
            "shell".to_string(),
            stream_limit,
        );
        let status = loop {
            if ctx.cancel().is_cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AstrError::Cancelled);
            }

            // 超时检测：超过 deadline 自动 kill 子进程
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();

                // 收集已捕获的输出
                let stdout_capture = stdout_task
                    .join()
                    .ok()
                    .and_then(|r| r.ok())
                    .map(|c| c.text)
                    .unwrap_or_default();
                let stderr_capture = stderr_task
                    .join()
                    .ok()
                    .and_then(|r| r.ok())
                    .map(|c| c.text)
                    .unwrap_or_default();
                let output = render_shell_output(&stdout_capture, &stderr_capture);
                let output = maybe_persist_tool_result(
                    &session_dir,
                    &tool_call_id,
                    &output,
                    ctx.resolved_inline_limit(),
                );
                let mut metadata = serde_json::Map::new();
                metadata.insert("command".to_string(), json!(command_text));
                metadata.insert("cwd".to_string(), json!(cwd_text.clone()));
                metadata.insert("shell".to_string(), json!(shell_display.clone()));
                metadata.insert("exitCode".to_string(), json!(-1));
                metadata.insert("streamed".to_string(), json!(true));
                metadata.insert("timedOut".to_string(), json!(true));
                metadata.insert(
                    "display".to_string(),
                    json!({
                        "kind": "terminal",
                        "command": args.command,
                        "cwd": cwd_text,
                        "shell": spec.display_shell,
                        "exitCode": -1,
                    }),
                );
                merge_persisted_tool_output_metadata(&mut metadata, output.persisted.as_ref());

                return Ok(ToolExecutionResult {
                    tool_call_id,
                    tool_name: "shell".to_string(),
                    ok: false,
                    output: output.output,
                    error: Some(format!("shell command timed out after {timeout_secs}s")),
                    metadata: Some(serde_json::Value::Object(metadata)),
                    continuation: None,
                    duration_ms: started_at.elapsed().as_millis() as u64,
                    truncated: false,
                });
            }

            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {},
                Err(e) => return Err(AstrError::io("failed to wait shell command", e)),
            }

            thread::sleep(Duration::from_millis(25));
        };

        let stdout_capture = stdout_task
            .join()
            .map_err(|_| AstrError::Internal("stdout reader thread panicked".to_string()))?
            .map_err(|e| AstrError::io("failed to read stdout", e))?;
        let stderr_capture = stderr_task
            .join()
            .map_err(|_| AstrError::Internal("stderr reader thread panicked".to_string()))?
            .map_err(|e| AstrError::io("failed to read stderr", e))?;

        let exit_code = status.code().unwrap_or(-1);
        let ok = status.success();
        let output = render_shell_output(&stdout_capture.text, &stderr_capture.text);

        // Truncate if output exceeds max size
        let (output, truncated) = if output.len() > ctx.max_output_size() {
            let truncation_msg = format!(
                "\n... [OUTPUT TRUNCATED: {} bytes total, showing first {} bytes]",
                output.len(),
                ctx.max_output_size()
            );
            // 使用 floor_char_boundary 确保截断点在 UTF-8 char boundary 上，
            // 避免输出含中文/Unicode 且总量接近 max_output_size 时按字节切片 panic
            let truncate_at = output
                .floor_char_boundary(ctx.max_output_size().saturating_sub(truncation_msg.len()));
            let mut truncated_output = output[..truncate_at].to_string();
            truncated_output.push_str(&truncation_msg);
            (truncated_output, true)
        } else {
            (output, false)
        };

        // 大结果持久化到磁盘，替换为引用 + 预览
        let output = maybe_persist_tool_result(
            &session_dir,
            &tool_call_id,
            &output,
            ctx.resolved_inline_limit(),
        );
        let mut metadata = serde_json::Map::new();
        metadata.insert("command".to_string(), json!(command_text));
        metadata.insert("cwd".to_string(), json!(cwd_text.clone()));
        metadata.insert("shell".to_string(), json!(shell_display));
        metadata.insert("exitCode".to_string(), json!(exit_code));
        metadata.insert("streamed".to_string(), json!(true));
        metadata.insert("stdoutBytes".to_string(), json!(stdout_capture.bytes_read));
        metadata.insert("stderrBytes".to_string(), json!(stderr_capture.bytes_read));
        metadata.insert(
            "stdoutTruncated".to_string(),
            json!(stdout_capture.truncated),
        );
        metadata.insert(
            "stderrTruncated".to_string(),
            json!(stderr_capture.truncated),
        );
        metadata.insert(
            "display".to_string(),
            json!({
                "kind": "terminal",
                "command": args.command,
                "cwd": cwd_text,
                "shell": spec.display_shell,
                "exitCode": exit_code,
            }),
        );
        metadata.insert("truncated".to_string(), json!(truncated));
        merge_persisted_tool_output_metadata(&mut metadata, output.persisted.as_ref());

        Ok(ToolExecutionResult {
            tool_call_id,
            tool_name: "shell".to_string(),
            ok,
            output: output.output,
            error: if ok {
                None
            } else {
                Some(format!("shell command exited with code {}", exit_code))
            },
            metadata: Some(serde_json::Value::Object(metadata)),
            continuation: None,
            duration_ms: started_at.elapsed().as_millis() as u64,
            truncated,
        })
    }
}

/// 根据平台和用户偏好构建 shell 命令规范。
///
/// 默认策略优先继承当前环境中的 shell 线索，再回退到平台可用的
/// bash/PowerShell/WSL 兜底链。
fn command_spec(command: &str) -> Result<CommandSpec> {
    let resolved_shell = resolve_shell(None)?;
    Ok(command_spec_for_family(resolved_shell, command))
}

fn command_spec_for_family(shell: ResolvedShell, command: &str) -> CommandSpec {
    let args = match shell.family {
        ShellFamily::PowerShell => vec![
            "-NoProfile".to_string(),
            "-Command".to_string(),
            command.to_string(),
        ],
        // `/d /s /c` 让 cmd 的单次执行语义和终端直觉更一致，也避免加载 AutoRun。
        ShellFamily::Cmd => vec![
            "/d".to_string(),
            "/s".to_string(),
            "/c".to_string(),
            command.to_string(),
        ],
        ShellFamily::Posix => vec!["-lc".to_string(), command.to_string()],
        // 通过 `wsl.exe bash -lc` 复用 Linux bash 语义，同时保持工具仍是一次性调用。
        ShellFamily::Wsl => vec!["bash".to_string(), "-lc".to_string(), command.to_string()],
    };
    CommandSpec {
        program: shell.program,
        args,
        display_shell: shell.label,
    }
}

fn default_shell_for_prompt() -> String {
    default_shell_label()
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, io, path::Path};

    use astrcode_runtime_contract::tool::ToolOutputDelta;
    use astrcode_support::shell::detect_shell_family;
    use tokio::sync::mpsc;

    use super::*;
    use crate::{builtin_tools::read_file::ReadFileTool, test_support::test_tool_context_for};

    struct ChunkedReader {
        chunks: VecDeque<Vec<u8>>,
    }

    impl ChunkedReader {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                chunks: VecDeque::from(chunks),
            }
        }
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some(chunk) = self.chunks.pop_front() else {
                return Ok(0);
            };
            let read_len = chunk.len().min(buf.len());
            buf[..read_len].copy_from_slice(&chunk[..read_len]);
            if read_len < chunk.len() {
                self.chunks.push_front(chunk[read_len..].to_vec());
            }
            Ok(read_len)
        }
    }

    fn collect_output_deltas(
        rx: &mut mpsc::UnboundedReceiver<ToolOutputDelta>,
    ) -> Vec<ToolOutputDelta> {
        let mut deltas = Vec::new();
        while let Ok(delta) = rx.try_recv() {
            deltas.push(delta);
        }
        deltas
    }

    fn default_shell_family_for_tests() -> ShellFamily {
        let spec = command_spec("echo ok").expect("default shell should resolve");
        detect_shell_family(&spec.program).expect("default shell family should be known")
    }

    fn large_output_command_for_default_shell() -> String {
        match default_shell_family_for_tests() {
            ShellFamily::Cmd => "for /l %i in (1,1,10000) do @<nul set /p=x".to_string(),
            ShellFamily::PowerShell => "[Console]::Write(('x' * 10000))".to_string(),
            ShellFamily::Posix | ShellFamily::Wsl => "yes x | head -c 10000".to_string(),
        }
    }

    fn pwd_command_for_default_shell() -> String {
        match default_shell_family_for_tests() {
            ShellFamily::Cmd => "cd".to_string(),
            ShellFamily::PowerShell => "(Get-Location).Path".to_string(),
            ShellFamily::Posix | ShellFamily::Wsl => "pwd".to_string(),
        }
    }

    #[test]
    fn stream_capture_truncates_oversized_chunk_with_notice() {
        let mut capture = StreamCapture::new(ToolOutputStream::Stdout, 5);

        let emitted = capture.push_chunk("abcdef");

        assert_eq!(
            emitted,
            "abcde\n... [stdout truncated after 5 bytes; later output omitted]\n"
        );
        assert_eq!(capture.text, emitted);
        assert_eq!(capture.bytes_read, 6);
        assert!(capture.truncated);
    }

    #[test]
    fn stream_capture_emits_notice_when_next_chunk_crosses_limit_boundary() {
        let mut capture = StreamCapture::new(ToolOutputStream::Stderr, 5);

        assert_eq!(capture.push_chunk("hello"), "hello");
        assert!(!capture.truncated);
        let emitted = capture.push_chunk("!");

        assert_eq!(
            emitted,
            "\n... [stderr truncated after 5 bytes; later output omitted]\n"
        );
        assert_eq!(
            capture.text,
            "hello\n... [stderr truncated after 5 bytes; later output omitted]\n"
        );
        assert_eq!(capture.bytes_read, 6);
        assert!(capture.truncated);
    }

    #[tokio::test]
    async fn spawn_stream_reader_streams_long_lines_without_newlines_progressively() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let reader = ChunkedReader::new(vec![vec![b'a'; 5000]]);
        let ctx = test_tool_context_for(std::env::temp_dir()).with_tool_output_sender(tx);

        let handle = spawn_stream_reader(
            reader,
            ToolOutputStream::Stdout,
            ctx,
            "call-long".to_string(),
            "shell".to_string(),
            6000,
        );
        let capture = handle
            .join()
            .expect("reader thread should join")
            .expect("reader should succeed");
        let deltas = collect_output_deltas(&mut rx);

        assert_eq!(capture.text.len(), 5000);
        assert_eq!(capture.bytes_read, 5000);
        assert_eq!(
            deltas.len(),
            2,
            "4096 boundary should force an intermediate flush"
        );
        assert_eq!(deltas[0].delta.len(), 4096);
        assert_eq!(deltas[1].delta.len(), 904);
        assert!(
            deltas
                .iter()
                .all(|delta| delta.stream == ToolOutputStream::Stdout)
        );
    }

    #[tokio::test]
    async fn spawn_stream_reader_preserves_utf8_chars_split_across_reads() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let reader = ChunkedReader::new(vec![
            vec![0xE4, 0xBD],
            vec![0xA0, 0xE5, 0xA5],
            vec![0xBD, b'\n'],
        ]);
        let ctx = test_tool_context_for(std::env::temp_dir()).with_tool_output_sender(tx);

        let handle = spawn_stream_reader(
            reader,
            ToolOutputStream::Stdout,
            ctx,
            "call-utf8".to_string(),
            "shell".to_string(),
            100,
        );
        let capture = handle
            .join()
            .expect("reader thread should join")
            .expect("reader should succeed");
        let deltas = collect_output_deltas(&mut rx);

        assert_eq!(capture.text, "你好\n");
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].delta, "你好\n");
    }

    #[tokio::test]
    async fn shell_tool_runs_non_interactive_command() {
        let tool = ShellTool;
        let args = json!({"command": "echo ok"});

        let result = tool
            .execute(
                "tc1".to_string(),
                args,
                &test_tool_context_for(std::env::temp_dir()),
            )
            .await
            .expect("shell tool should execute");

        assert!(result.ok);
        assert!(result.output.contains("ok"));
    }

    #[test]
    fn shell_prompt_describes_directory_inspection_entrypoint() {
        let prompt = ShellTool
            .capability_metadata()
            .prompt
            .expect("shell should expose prompt metadata");

        assert!(prompt.guide.contains("inspect directories"));
        assert!(prompt.guide.contains("Get-ChildItem"));
        assert!(prompt.guide.contains("readFile"));
        assert!(prompt.guide.contains("findFiles"));
        assert!(prompt.guide.contains("grep"));
    }

    #[tokio::test]
    async fn shell_persists_large_output_and_read_file_can_open_it() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let shell_ctx = test_tool_context_for(temp.path())
            .with_resolved_inline_limit(4 * 1024)
            .with_max_output_size(20 * 1024);
        let tool = ShellTool;
        let args = json!({
            "command": large_output_command_for_default_shell()
        });

        let result = tool
            .execute("tc-shell-persisted".to_string(), args, &shell_ctx)
            .await
            .expect("shell tool should persist oversized output");

        assert!(result.ok);
        assert!(result.output.starts_with("<persisted-output>"));
        let metadata = result.metadata.as_ref().expect("metadata should exist");
        let persisted_absolute = metadata["persistedOutput"]["absolutePath"]
            .as_str()
            .expect("persisted absolute path should be present");
        assert!(Path::new(persisted_absolute).exists());

        let read_tool = ReadFileTool;
        let read_result = read_tool
            .execute(
                "tc-shell-read-persisted".to_string(),
                json!({
                    "path": persisted_absolute,
                    "charOffset": 0,
                    "maxChars": 2048
                }),
                &test_tool_context_for(temp.path()),
            )
            .await
            .expect("readFile should open persisted shell output");

        assert!(read_result.ok);
        assert!(read_result.output.contains('x'));
        assert!(read_result.output.len() >= 1024);
        let read_metadata = read_result.metadata.expect("metadata should exist");
        assert_eq!(read_metadata["persistedRead"], json!(true));
    }

    #[tokio::test]
    async fn shell_allows_cwd_outside_working_dir() {
        let parent = tempfile::tempdir().expect("tempdir should be created");
        let workspace = parent.path().join("workspace");
        let outside = parent.path().join("outside");
        tokio::fs::create_dir_all(&workspace)
            .await
            .expect("workspace should be created");
        tokio::fs::create_dir_all(&outside)
            .await
            .expect("outside dir should be created");
        let tool = ShellTool;
        let args = json!({
            "command": pwd_command_for_default_shell(),
            "cwd": outside.to_string_lossy()
        });

        let result = tool
            .execute(
                "tc-shell-cwd-outside".to_string(),
                args,
                &test_tool_context_for(&workspace),
            )
            .await
            .expect("shell tool should execute");

        assert!(result.ok);
        let metadata = result.metadata.expect("metadata should exist");
        let expected_cwd = resolve_path(&test_tool_context_for(&workspace), &outside)
            .expect("cwd should resolve consistently");
        assert_eq!(
            metadata["cwd"],
            json!(expected_cwd.to_string_lossy().to_string())
        );
    }

    #[tokio::test]
    async fn shell_tool_rejects_blank_command() {
        let tool = ShellTool;
        let err = tool
            .execute(
                "tc2".to_string(),
                json!({"command": "   "}),
                &test_tool_context_for(std::env::temp_dir()),
            )
            .await
            .expect_err("blank command should fail");

        assert!(matches!(err, AstrError::Validation(_)));
    }

    #[test]
    fn shell_tool_definition_and_prompt_include_default_shell() {
        let tool = ShellTool;
        let definition = tool.definition();
        let prompt = tool
            .capability_metadata()
            .prompt
            .expect("shell prompt metadata should exist");

        let default_shell = default_shell_for_prompt();

        assert!(definition.description.contains(default_shell.as_str()));
        assert!(prompt.summary.contains(default_shell.as_str()));
        assert!(prompt.guide.contains(default_shell.as_str()));
    }

    #[test]
    fn detect_shell_family_supports_common_shell_names() {
        assert!(matches!(
            detect_shell_family("pwsh"),
            Some(ShellFamily::PowerShell)
        ));
        assert!(matches!(
            detect_shell_family("powershell.exe"),
            Some(ShellFamily::PowerShell)
        ));
        assert!(matches!(detect_shell_family("cmd"), Some(ShellFamily::Cmd)));
        assert!(matches!(
            detect_shell_family("/bin/bash"),
            Some(ShellFamily::Posix)
        ));
        assert!(matches!(
            detect_shell_family("wsl.exe"),
            Some(ShellFamily::Wsl)
        ));
    }

    #[test]
    fn command_spec_uses_runtime_default_shell() {
        let spec = command_spec("echo ok").expect("default shell should resolve");
        assert!(!spec.program.is_empty());
        assert_eq!(spec.display_shell, default_shell_for_prompt());
    }
}
