//! 后台 shell 任务：长命令脱离当前 tool 调用，输出写入 session 目录，由 shellId 查询状态。

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, OnceLock},
};

use astrcode_core::tool::ToolError;
use astrcode_support::shell::ShellInfo;
use parking_lot::Mutex;
use tokio::{
    fs::OpenOptions,
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    sync::Notify,
};

use crate::shell_tool::{
    MAX_CAPTURE_BYTES_PER_STREAM, command_args, hide_command_window, setup_process_group,
    terminate_child_tree,
};

const BACKGROUND_SHELLS_DIR: &str = "background-shells";

/// 单次状态查询默认返回的新输出 token 预算（粗略按 4 chars ≈ 1 token 估算）。
///
/// 与 Codex unified exec 的默认 `max_output_tokens` 对齐；完整输出仍写入
/// `output_path`，需要更多上下文时用 read 分页读取。
pub const DEFAULT_STATUS_OUTPUT_MAX_TOKENS: usize = 10_000;
/// 防止单次后台 shell 查询挤爆上下文；完整输出可通过 output_path 用 read 分页读取。
pub const MAX_STATUS_OUTPUT_MAX_TOKENS: usize = 20_000;
const MIN_STATUS_OUTPUT_MAX_TOKENS: usize = 256;

/// session 销毁时终止该 session 下所有后台 shell。
pub fn cleanup_background_shells_for_session(session_id: &str) {
    BackgroundShellRegistry::global().cleanup_session(session_id);
}

/// 启动后台 shell；立即返回 shell id 与输出文件路径。
pub async fn spawn_background_shell(
    params: BackgroundShellSpawnParams,
) -> Result<SpawnedBackgroundShell, ToolError> {
    BackgroundShellRegistry::global().spawn(params).await
}

/// 将已在运行的前台 shell 收编为后台任务（进程与输出流保持不中断）。
pub async fn adopt_running_shell(
    params: AdoptBackgroundShellParams,
    stdout_join: tokio::task::JoinHandle<()>,
    stderr_join: tokio::task::JoinHandle<()>,
) -> Result<SpawnedBackgroundShell, ToolError> {
    BackgroundShellRegistry::global()
        .adopt(params, stdout_join, stderr_join)
        .await
}

/// 收编已在运行的前台 shell 时的参数（输出文件与 header 由调用方预先写好）。
pub struct AdoptBackgroundShellParams {
    pub session_id: String,
    pub timeout_secs: u64,
    pub shell_id: String,
    pub output_path: PathBuf,
    pub child: Child,
}

/// 等待已有后台 shell 结束或超时（`block_until_ms`）；`0` 表示仅查询状态。
pub async fn wait_background_shell(
    shell_id: &str,
    block_until_ms: u64,
    max_output_tokens: Option<usize>,
) -> Result<BackgroundShellStatus, ToolError> {
    let record = BackgroundShellRegistry::global()
        .get(shell_id)
        .ok_or_else(|| ToolError::InvalidArguments(format!("unknown shell_id: {shell_id}")))?;

    if *record.status.lock() == ShellRunStatus::Running && block_until_ms > 0 {
        wait_for_output_or_completion(&record, block_until_ms.min(600_000)).await;
    }

    let status = read_shell_status(shell_id, &record, max_output_tokens).await?;
    Ok(status)
}

async fn wait_for_output_or_completion(record: &BackgroundShellRecord, block_until_ms: u64) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(block_until_ms);
    loop {
        if *record.status.lock() != ShellRunStatus::Running {
            return;
        }
        if has_unread_output(record).await {
            return;
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return;
        }
        let sleep_for = (deadline - now).min(std::time::Duration::from_millis(100));
        let notified = record.done.notified();
        tokio::select! {
            _ = notified => return,
            _ = tokio::time::sleep(sleep_for) => {}
        }
    }
}

async fn has_unread_output(record: &BackgroundShellRecord) -> bool {
    let Ok(len) = file_len(&record.output_path).await else {
        return false;
    };
    len > *record.read_offset.lock()
}

#[derive(Clone)]
pub struct BackgroundShellSpawnParams {
    pub session_id: String,
    pub command: String,
    pub intent: Option<String>,
    pub cwd: PathBuf,
    pub shell: ShellInfo,
    pub timeout_secs: u64,
    pub store_dir: Option<PathBuf>,
}

pub struct SpawnedBackgroundShell {
    pub shell_id: String,
    pub output_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellRunStatus {
    Running,
    Completed,
    Failed,
    TimedOut,
    Killed,
}

#[derive(Debug)]
pub struct BackgroundShellStatus {
    pub shell_id: String,
    pub output_path: PathBuf,
    pub status: String,
    pub exit_code: Option<i32>,
    pub output: String,
    pub output_truncated: bool,
    pub output_tokens: usize,
    pub returned_output_tokens: usize,
    pub omitted_output_tokens: usize,
    pub max_output_tokens: usize,
    pub running: bool,
}

struct BackgroundShellRecord {
    session_id: String,
    shell_id: String,
    output_path: PathBuf,
    status: Mutex<ShellRunStatus>,
    exit_code: Mutex<Option<i32>>,
    read_offset: Mutex<u64>,
    child: Mutex<Option<Child>>,
    done: Notify,
}

struct BackgroundShellRegistry {
    shells: Mutex<HashMap<String, Arc<BackgroundShellRecord>>>,
}

impl BackgroundShellRegistry {
    fn global() -> &'static Self {
        static REGISTRY: OnceLock<BackgroundShellRegistry> = OnceLock::new();
        REGISTRY.get_or_init(|| BackgroundShellRegistry {
            shells: Mutex::new(HashMap::new()),
        })
    }

    async fn spawn(
        &self,
        params: BackgroundShellSpawnParams,
    ) -> Result<SpawnedBackgroundShell, ToolError> {
        let prepared = prepare_background_shell_output(
            params.store_dir.as_deref(),
            &params.cwd,
            &params.command,
            params.intent.as_deref(),
            &params.shell,
        )
        .await?;
        let shell_id = prepared.shell_id;
        let output_path = prepared.output_path;
        let read_offset = prepared.header_len;

        let command_args = command_args(&params.shell, &params.command);
        let mut command = Command::new(&params.shell.path);
        command
            .args(&command_args)
            .current_dir(&params.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());
        hide_command_window(&mut command);
        setup_process_group(&mut command);

        let mut child = command
            .spawn()
            .map_err(|e| ToolError::Execution(format!("spawn background shell: {e}")))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::Execution("failed to capture stdout".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::Execution("failed to capture stderr".into()))?;

        let record = Arc::new(BackgroundShellRecord {
            session_id: params.session_id.clone(),
            shell_id: shell_id.clone(),
            output_path: output_path.clone(),
            status: Mutex::new(ShellRunStatus::Running),
            exit_code: Mutex::new(None),
            read_offset: Mutex::new(read_offset),
            child: Mutex::new(Some(child)),
            done: Notify::new(),
        });
        self.shells
            .lock()
            .insert(shell_id.clone(), Arc::clone(&record));

        let wait_record = Arc::clone(&record);
        tokio::spawn(async move {
            run_background_shell(wait_record, stdout, stderr, params.timeout_secs).await;
        });

        Ok(SpawnedBackgroundShell {
            shell_id,
            output_path,
        })
    }

    async fn adopt(
        &self,
        params: AdoptBackgroundShellParams,
        stdout_join: tokio::task::JoinHandle<()>,
        stderr_join: tokio::task::JoinHandle<()>,
    ) -> Result<SpawnedBackgroundShell, ToolError> {
        let shell_id = params.shell_id;
        let output_path = params.output_path.clone();
        let read_offset = file_len(&output_path).await.unwrap_or(0);
        let record = Arc::new(BackgroundShellRecord {
            session_id: params.session_id.clone(),
            shell_id: shell_id.clone(),
            output_path: output_path.clone(),
            status: Mutex::new(ShellRunStatus::Running),
            exit_code: Mutex::new(None),
            read_offset: Mutex::new(read_offset),
            child: Mutex::new(Some(params.child)),
            done: Notify::new(),
        });
        self.shells
            .lock()
            .insert(shell_id.clone(), Arc::clone(&record));

        let wait_record = Arc::clone(&record);
        let timeout_secs = params.timeout_secs;
        tokio::spawn(async move {
            run_adopted_background_shell(wait_record, stdout_join, stderr_join, timeout_secs).await;
        });

        Ok(SpawnedBackgroundShell {
            shell_id,
            output_path,
        })
    }

    fn get(&self, shell_id: &str) -> Option<Arc<BackgroundShellRecord>> {
        self.shells.lock().get(shell_id).cloned()
    }

    fn cleanup_session(&self, session_id: &str) {
        // Remove all matching entries under a single lock hold to avoid TOCTOU.
        let records: Vec<Arc<BackgroundShellRecord>> = {
            let mut shells = self.shells.lock();
            let ids: Vec<String> = shells
                .iter()
                .filter(|(_, r)| r.session_id == session_id)
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| shells.remove(id)).collect()
        };
        for record in records {
            kill_record(&record);
        }
    }
}

async fn run_background_shell(
    record: Arc<BackgroundShellRecord>,
    stdout: impl AsyncRead + Unpin + Send + 'static,
    stderr: impl AsyncRead + Unpin + Send + 'static,
    timeout_secs: u64,
) {
    let path = record.output_path.clone();
    finish_background_shell(
        record,
        tokio::spawn(stream_to_file(stdout, path.clone(), false)),
        tokio::spawn(stream_to_file(stderr, path, true)),
        timeout_secs,
    )
    .await;
}

async fn run_adopted_background_shell(
    record: Arc<BackgroundShellRecord>,
    stdout_join: tokio::task::JoinHandle<()>,
    stderr_join: tokio::task::JoinHandle<()>,
    timeout_secs: u64,
) {
    finish_background_shell(record, stdout_join, stderr_join, timeout_secs).await;
}

async fn finish_background_shell(
    record: Arc<BackgroundShellRecord>,
    stdout_join: tokio::task::JoinHandle<()>,
    stderr_join: tokio::task::JoinHandle<()>,
    timeout_secs: u64,
) {
    let mut child = match record.child.lock().take() {
        Some(c) => c,
        None => return,
    };

    let (exit_code, run_status) = match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        child.wait(),
    )
    .await
    {
        Ok(Ok(status)) => {
            let code = status.code().unwrap_or(-1);
            let st = if code == 0 {
                ShellRunStatus::Completed
            } else {
                ShellRunStatus::Failed
            };
            (Some(code), st)
        },
        Ok(Err(_)) => (None, ShellRunStatus::Failed),
        Err(_) => {
            terminate_child_tree(&mut child).await;
            let _ = child.wait().await;
            (None, ShellRunStatus::TimedOut)
        },
    };

    let _ = tokio::join!(stdout_join, stderr_join);

    let footer = format_footer(run_status, exit_code);
    if let Err(e) = append_bytes(&record.output_path, footer.as_bytes()).await {
        tracing::warn!(
            shell_id = %record.shell_id,
            error = %e,
            "failed to append adopted background shell footer"
        );
    }

    publish_completion(&record, run_status, exit_code);
}

fn publish_completion(
    record: &BackgroundShellRecord,
    run_status: ShellRunStatus,
    exit_code: Option<i32>,
) {
    *record.exit_code.lock() = exit_code;
    *record.status.lock() = run_status;
    record.done.notify_waiters();
}

fn kill_record(record: &BackgroundShellRecord) {
    *record.status.lock() = ShellRunStatus::Killed;
    if let Some(mut child) = record.child.lock().take() {
        // Synchronous kill guarantees the process dies even if the tokio runtime
        // is shutting down (cleanup is called from a sync trait method).
        let _ = child.start_kill();
        // Best-effort async reap; if the runtime is gone the OS will clean up
        // when our process exits.
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
    }
    record.done.notify_waiters();
}

pub(crate) struct PreparedBackgroundShellOutput {
    pub shell_id: String,
    pub output_path: PathBuf,
    header_len: u64,
}

pub(crate) async fn prepare_background_shell_output(
    store_dir: Option<&Path>,
    cwd: &Path,
    command: &str,
    intent: Option<&str>,
    shell: &ShellInfo,
) -> Result<PreparedBackgroundShellOutput, ToolError> {
    let shell_id = uuid::Uuid::new_v4().to_string();
    let output_dir = background_output_dir(store_dir, cwd);
    std::fs::create_dir_all(&output_dir)
        .map_err(|e| ToolError::Execution(format!("create background-shells dir: {e}")))?;
    let output_path = output_dir.join(format!("{shell_id}.txt"));
    let description = intent
        .filter(|intent| !intent.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| command_description(command));
    let header_len =
        write_file_header(&output_path, &shell_id, command, &description, cwd, shell).await?;

    Ok(PreparedBackgroundShellOutput {
        shell_id,
        output_path,
        header_len,
    })
}

fn background_output_dir(store_dir: Option<&Path>, cwd: &Path) -> PathBuf {
    store_dir.map_or_else(
        || cwd.join(".astrcode").join(BACKGROUND_SHELLS_DIR),
        |dir| dir.join(BACKGROUND_SHELLS_DIR),
    )
}

fn command_description(command: &str) -> String {
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

async fn write_file_header(
    path: &Path,
    shell_id: &str,
    command: &str,
    description: &str,
    cwd: &Path,
    shell: &ShellInfo,
) -> Result<u64, ToolError> {
    let header = format!(
        "---\nshell_id: {shell_id}\ncommand: {}\ndescription: {}\ncwd: {}\nshell: {}\n---\n\n",
        command.replace('\n', " "),
        description.replace('\n', " "),
        cwd.display(),
        shell.name,
    );
    tokio::fs::write(path, header.as_bytes())
        .await
        .map_err(|e| ToolError::Execution(format!("write background shell header: {e}")))?;
    Ok(header.len() as u64)
}

fn format_footer(status: ShellRunStatus, exit_code: Option<i32>) -> String {
    let status_str = match status {
        ShellRunStatus::Completed => "completed",
        ShellRunStatus::Failed => "failed",
        ShellRunStatus::TimedOut => "timed_out",
        ShellRunStatus::Killed => "killed",
        ShellRunStatus::Running => "running",
    };
    let code_line = exit_code
        .map(|c| format!("\nexit_code: {c}"))
        .unwrap_or_default();
    format!("\n---\nstatus: {status_str}{code_line}\n---\n")
}

async fn stream_to_file(mut stream: impl AsyncRead + Unpin, path: PathBuf, is_stderr: bool) {
    let mut buf = [0u8; 8192];
    let mut kept = 0usize;
    let mut truncated = false;
    let mut stderr_marker_written = false;
    while let Ok(n) = stream.read(&mut buf).await {
        if n == 0 {
            break;
        }
        if truncated {
            continue;
        }
        let take = if kept + n > MAX_CAPTURE_BYTES_PER_STREAM {
            truncated = true;
            MAX_CAPTURE_BYTES_PER_STREAM.saturating_sub(kept)
        } else {
            n
        };
        if take > 0 {
            if is_stderr && !stderr_marker_written {
                if append_bytes(&path, b"\n--- STDERR ---\n").await.is_err() {
                    break;
                }
                stderr_marker_written = true;
            }
            if append_bytes(&path, &buf[..take]).await.is_err() {
                break;
            }
            kept += take;
        }
        if truncated {
            let note = format!(
                "\n[output truncated at {MAX_CAPTURE_BYTES_PER_STREAM} bytes per stream]\n"
            );
            let _ = append_bytes(&path, note.as_bytes()).await;
        }
    }
}

pub(crate) async fn append_shell_output(path: &Path, data: &[u8]) -> std::io::Result<()> {
    append_bytes(path, data).await
}

async fn append_bytes(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(data).await?;
    Ok(())
}

async fn file_len(path: &Path) -> std::io::Result<u64> {
    Ok(tokio::fs::metadata(path).await?.len())
}

async fn read_shell_status(
    shell_id: &str,
    record: &BackgroundShellRecord,
    max_output_tokens: Option<usize>,
) -> Result<BackgroundShellStatus, ToolError> {
    let status = *record.status.lock();
    let exit_code = *record.exit_code.lock();
    let (status_str, running) = match status {
        ShellRunStatus::Running => ("running", true),
        ShellRunStatus::Completed => ("completed", false),
        ShellRunStatus::Failed => ("failed", false),
        ShellRunStatus::TimedOut => ("timed_out", false),
        ShellRunStatus::Killed => ("killed", false),
    };
    let preview = read_new_output(record, max_output_tokens).await?;
    Ok(BackgroundShellStatus {
        shell_id: shell_id.to_string(),
        output_path: record.output_path.clone(),
        status: status_str.into(),
        exit_code,
        output: preview.content,
        output_truncated: preview.truncated,
        output_tokens: preview.original_tokens,
        returned_output_tokens: preview.returned_tokens,
        omitted_output_tokens: preview.omitted_tokens,
        max_output_tokens: preview.max_tokens,
        running,
    })
}

struct OutputPreview {
    content: String,
    truncated: bool,
    original_tokens: usize,
    returned_tokens: usize,
    omitted_tokens: usize,
    max_tokens: usize,
}

async fn read_new_output(
    record: &BackgroundShellRecord,
    max_output_tokens: Option<usize>,
) -> Result<OutputPreview, ToolError> {
    let bytes = tokio::fs::read(&record.output_path)
        .await
        .map_err(|e| ToolError::Execution(format!("read background shell output: {e}")))?;
    let mut offset = record.read_offset.lock();
    let start = (*offset as usize).min(bytes.len());
    *offset = bytes.len() as u64;
    drop(offset);

    let unread = String::from_utf8_lossy(&bytes[start..]).into_owned();
    Ok(preview_output_by_tokens(&unread, max_output_tokens))
}

fn preview_output_by_tokens(content: &str, max_output_tokens: Option<usize>) -> OutputPreview {
    let max_tokens = normalize_status_output_max_tokens(max_output_tokens);
    let original_tokens = estimate_text_tokens(content);
    if original_tokens <= max_tokens {
        return OutputPreview {
            content: content.to_string(),
            truncated: false,
            original_tokens,
            returned_tokens: original_tokens,
            omitted_tokens: 0,
            max_tokens,
        };
    }

    let max_chars = max_tokens.saturating_mul(4);
    let marker_reserve_chars = 512.min(max_chars / 3);
    let content_budget_chars = max_chars.saturating_sub(marker_reserve_chars).max(64);
    let head_chars = (content_budget_chars / 4).max(32);
    let tail_chars = content_budget_chars.saturating_sub(head_chars).max(32);
    let total_chars = content.chars().count();

    let head = take_chars(content, head_chars);
    let tail = take_last_chars(content, tail_chars);
    let kept_chars = head.chars().count().saturating_add(tail.chars().count());
    let omitted_chars = total_chars.saturating_sub(kept_chars);
    let omitted_tokens = estimate_char_tokens(omitted_chars);
    let marker = format!(
        "\n\n[... background shell output truncated: omitted about {omitted_tokens} tokens from \
         this poll; full output is available in the output file ...]\n\n"
    );
    let preview = format!("{head}{marker}{tail}");
    let returned_tokens = estimate_text_tokens(&preview);

    OutputPreview {
        content: preview,
        truncated: true,
        original_tokens,
        returned_tokens,
        omitted_tokens,
        max_tokens,
    }
}

fn normalize_status_output_max_tokens(max_output_tokens: Option<usize>) -> usize {
    max_output_tokens
        .unwrap_or(DEFAULT_STATUS_OUTPUT_MAX_TOKENS)
        .clamp(MIN_STATUS_OUTPUT_MAX_TOKENS, MAX_STATUS_OUTPUT_MAX_TOKENS)
}

fn estimate_text_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    if chars == 0 {
        0
    } else {
        estimate_char_tokens(chars)
    }
}

fn estimate_char_tokens(chars: usize) -> usize {
    chars.div_ceil(4)
}

fn take_chars(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

fn take_last_chars(text: &str, count: usize) -> String {
    let mut chars: Vec<char> = text.chars().rev().take(count).collect();
    chars.reverse();
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests;
