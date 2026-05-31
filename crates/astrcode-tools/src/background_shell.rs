//! 后台 shell 任务：长命令脱离当前 tool 调用，输出写入 session 目录，完成后注入通知。

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, OnceLock},
};

use astrcode_core::tool::{SessionAccess, SessionOperations, ToolError};
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

/// 等待已有后台 shell 结束或超时（`block_until_ms`）；`0` 表示仅查询状态。
pub async fn wait_background_shell(
    shell_id: &str,
    block_until_ms: u64,
) -> Result<BackgroundShellStatus, ToolError> {
    let record = BackgroundShellRegistry::global()
        .get(shell_id)
        .ok_or_else(|| ToolError::InvalidArguments(format!("unknown shell_id: {shell_id}")))?;

    if *record.status.lock() == ShellRunStatus::Running && block_until_ms > 0 {
        let notified = record.done.notified();
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(block_until_ms.min(600_000)),
            notified,
        )
        .await;
    }

    read_shell_status(shell_id, &record).await
}

#[derive(Clone)]
pub struct BackgroundShellSpawnParams {
    pub session_id: String,
    pub tool_call_id: Option<String>,
    pub command: String,
    pub intent: Option<String>,
    pub cwd: PathBuf,
    pub shell: ShellInfo,
    pub timeout_secs: u64,
    pub store_dir: Option<PathBuf>,
    pub session_ops: Option<Arc<dyn SessionOperations>>,
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

pub struct BackgroundShellStatus {
    pub shell_id: String,
    pub output_path: PathBuf,
    pub status: String,
    pub exit_code: Option<i32>,
    pub tail: String,
    pub running: bool,
}

struct BackgroundShellRecord {
    session_id: String,
    shell_id: String,
    output_path: PathBuf,
    description: String,
    tool_call_id: Option<String>,
    session_ops: Option<Arc<dyn SessionOperations>>,
    status: Mutex<ShellRunStatus>,
    exit_code: Mutex<Option<i32>>,
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
        let shell_id = uuid::Uuid::new_v4().to_string();
        let output_dir = resolve_output_dir(&params)?;
        std::fs::create_dir_all(&output_dir)
            .map_err(|e| ToolError::Execution(format!("create background-shells dir: {e}")))?;
        let output_path = output_dir.join(format!("{shell_id}.txt"));

        let description = params
            .intent
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| truncate_command(&params.command));

        write_file_header(&output_path, &params, &shell_id, &description).await?;

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
            description,
            tool_call_id: params.tool_call_id.clone(),
            session_ops: params.session_ops.clone(),
            status: Mutex::new(ShellRunStatus::Running),
            exit_code: Mutex::new(None),
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
            ids.iter()
                .filter_map(|id| shells.remove(id))
                .collect()
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
    let out_h = tokio::spawn(stream_to_file(stdout, path.clone(), false));
    let err_h = tokio::spawn(stream_to_file(stderr, path, true));

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

    let _ = out_h.await;
    let _ = err_h.await;

    *record.exit_code.lock() = exit_code;
    *record.status.lock() = run_status;
    record.done.notify_waiters();

    let footer = format_footer(run_status, exit_code);
    if let Err(e) = append_bytes(&record.output_path, footer.as_bytes()).await {
        tracing::warn!(
            shell_id = %record.shell_id,
            error = %e,
            "failed to append background shell footer"
        );
    }

    notify_completion(&record, run_status, exit_code).await;
}

async fn notify_completion(
    record: &BackgroundShellRecord,
    status: ShellRunStatus,
    exit_code: Option<i32>,
) {
    let Some(ops) = record.session_ops.as_ref() else {
        return;
    };
    let access = SessionAccess::same(record.session_id.as_str());
    let exit_note = match exit_code {
        Some(code) => format!(" (exit code {code})"),
        None if status == ShellRunStatus::TimedOut => " (timed out)".into(),
        None => String::new(),
    };
    let status_word = match status {
        ShellRunStatus::Completed => "completed",
        ShellRunStatus::Failed => "failed",
        ShellRunStatus::TimedOut => "timed out",
        ShellRunStatus::Killed => "was stopped",
        ShellRunStatus::Running => return,
    };
    let path = record.output_path.display();
    let tool_line = record
        .tool_call_id
        .as_ref()
        .map(|id| format!("\nTool call id: {id}"))
        .unwrap_or_default();
    let message = format!(
        "[A background shell command has finished. Review the output file and continue the \
         task.]\n\nBackground command \"{}\" {status_word}{exit_note}.\nOutput file: \
         {path}{tool_line}\nUse `read` with this path (charOffset / maxChars) to paginate. Do not \
         re-run the command for more output.",
        record.description
    );
    if let Err(e) = ops.inject_message(access, message).await {
        tracing::warn!(
            session_id = %record.session_id,
            error = %e,
            "failed to inject background shell completion notification"
        );
    }
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

fn resolve_output_dir(params: &BackgroundShellSpawnParams) -> Result<PathBuf, ToolError> {
    if let Some(dir) = &params.store_dir {
        return Ok(dir.join(BACKGROUND_SHELLS_DIR));
    }
    Ok(PathBuf::from(&params.cwd)
        .join(".astrcode")
        .join(BACKGROUND_SHELLS_DIR))
}

fn truncate_command(command: &str) -> String {
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
    params: &BackgroundShellSpawnParams,
    shell_id: &str,
    description: &str,
) -> Result<(), ToolError> {
    let header = format!(
        "---\nshell_id: {shell_id}\ncommand: {}\ndescription: {}\ncwd: {}\nshell: {}\n---\n\n",
        params.command.replace('\n', " "),
        description.replace('\n', " "),
        params.cwd.display(),
        params.shell.name,
    );
    tokio::fs::write(path, header.as_bytes())
        .await
        .map_err(|e| ToolError::Execution(format!("write background shell header: {e}")))
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
    if is_stderr {
        let marker = b"\n--- STDERR ---\n";
        if append_bytes(&path, marker).await.is_err() {
            return;
        }
    }
    let mut buf = [0u8; 8192];
    let mut kept = 0usize;
    let mut truncated = false;
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

async fn append_bytes(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(data).await?;
    Ok(())
}

async fn read_shell_status(
    shell_id: &str,
    record: &BackgroundShellRecord,
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
    let tail = tail_of_file(&record.output_path, 32 * 1024).await?;
    Ok(BackgroundShellStatus {
        shell_id: shell_id.to_string(),
        output_path: record.output_path.clone(),
        status: status_str.into(),
        exit_code,
        tail,
        running,
    })
}

async fn tail_of_file(path: &Path, max_bytes: usize) -> Result<String, ToolError> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| ToolError::Execution(format!("read background shell output: {e}")))?;
    let start = bytes.len().saturating_sub(max_bytes);
    Ok(String::from_utf8_lossy(&bytes[start..]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_command_shortens_long_input() {
        let long = "a".repeat(100);
        let t = truncate_command(&long);
        assert!(t.chars().count() <= 80);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn truncate_command_preserves_short_input() {
        let short = "echo hello";
        assert_eq!(truncate_command(short), short);
    }

    #[test]
    fn resolve_output_dir_uses_store_dir_when_provided() {
        let params = BackgroundShellSpawnParams {
            session_id: "test".into(),
            tool_call_id: None,
            command: "echo hi".into(),
            intent: None,
            cwd: PathBuf::from("/tmp"),
            shell: ShellInfo {
                family: astrcode_support::shell::ShellFamily::Posix,
                name: "bash".into(),
                path: "bash".into(),
            },
            timeout_secs: 30,
            store_dir: Some(PathBuf::from("/data/sessions/abc")),
            session_ops: None,
        };
        let dir = resolve_output_dir(&params).unwrap();
        assert_eq!(dir, PathBuf::from("/data/sessions/abc/background-shells"));
    }

    #[test]
    fn resolve_output_dir_falls_back_to_cwd_astrcode() {
        let params = BackgroundShellSpawnParams {
            session_id: "test".into(),
            tool_call_id: None,
            command: "echo hi".into(),
            intent: None,
            cwd: PathBuf::from("/tmp"),
            shell: ShellInfo {
                family: astrcode_support::shell::ShellFamily::Posix,
                name: "bash".into(),
                path: "bash".into(),
            },
            timeout_secs: 30,
            store_dir: None,
            session_ops: None,
        };
        let dir = resolve_output_dir(&params).unwrap();
        assert_eq!(dir, PathBuf::from("/tmp/.astrcode/background-shells"));
    }

    #[test]
    fn format_footer_contains_status() {
        let footer = format_footer(ShellRunStatus::Completed, Some(0));
        assert!(footer.contains("completed"));
        assert!(footer.contains("exit_code: 0"));

        let footer = format_footer(ShellRunStatus::TimedOut, None);
        assert!(footer.contains("timed_out"));
        assert!(!footer.contains("exit_code"));
    }

    /// Registry cleanup removes entries for the target session and kills running shells.
    /// This test uses a short-lived command so the shell exits naturally.
    #[tokio::test]
    async fn cleanup_session_removes_shells_and_kills_running() {
        let registry = BackgroundShellRegistry {
            shells: Mutex::new(HashMap::new()),
        };

        let temp = tempfile::tempdir().unwrap();
        let shell = astrcode_support::shell::resolve_shell();
        let echo_cmd = match shell.family {
            astrcode_support::shell::ShellFamily::PowerShell => "Write-Output hello".into(),
            astrcode_support::shell::ShellFamily::Cmd => "echo hello".into(),
            _ => "echo hello".into(),
        };

        let spawned = registry
            .spawn(BackgroundShellSpawnParams {
                session_id: "sess-1".into(),
                tool_call_id: None,
                command: echo_cmd,
                intent: None,
                cwd: temp.path().to_path_buf(),
                shell,
                timeout_secs: 10,
                store_dir: None,
                session_ops: None,
            })
            .await
            .expect("spawn should succeed");

        // Give the shell time to complete so kill_record's start_kill doesn't race.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_eq!(registry.shells.lock().len(), 1);
        registry.cleanup_session("sess-1");
        assert!(registry.shells.lock().is_empty());

        // Output file should still exist (not deleted, just record removed).
        assert!(spawned.output_path.exists());
    }

    /// cleanup_session only targets the specified session.
    #[tokio::test]
    async fn cleanup_session_is_scoped_to_target_session() {
        let registry = BackgroundShellRegistry {
            shells: Mutex::new(HashMap::new()),
        };

        let temp = tempfile::tempdir().unwrap();
        let shell = astrcode_support::shell::resolve_shell();
        let echo_cmd = match shell.family {
            astrcode_support::shell::ShellFamily::PowerShell => "Write-Output hi".into(),
            astrcode_support::shell::ShellFamily::Cmd => "echo hi".into(),
            _ => "echo hi".into(),
        };

        let params = BackgroundShellSpawnParams {
            session_id: "sess-other".into(),
            tool_call_id: None,
            command: echo_cmd,
            intent: None,
            cwd: temp.path().to_path_buf(),
            shell,
            timeout_secs: 10,
            store_dir: None,
            session_ops: None,
        };

        let _spawned = registry.spawn(params).await.expect("spawn should succeed");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        assert_eq!(registry.shells.lock().len(), 1);
        registry.cleanup_session("sess-different");
        assert_eq!(registry.shells.lock().len(), 1);
    }
}
