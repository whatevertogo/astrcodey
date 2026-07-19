use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use astrcode_support::shell::ShellInfo;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    sync::Notify,
};

use super::MAX_CAPTURE_BYTES_PER_STREAM;
use crate::background_shell::append_shell_output;

#[derive(Clone, Copy)]
pub(super) enum ShellOutputDiagnostic {
    SudoAuthenticationRequired,
}

impl ShellOutputDiagnostic {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::SudoAuthenticationRequired => "sudo_authentication_required",
        }
    }

    pub(super) fn message(self) -> &'static str {
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

pub(super) fn detect_shell_output_diagnostic(output: &str) -> Option<ShellOutputDiagnostic> {
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
#[derive(Default)]
pub(super) struct CapturedOutput {
    pub(super) text: String,
    pub(super) bytes_read: usize,
    pub(super) truncated: bool,
}

/// 前台执行期间可切换为写 background-shell 输出文件。
pub(super) struct BackgroundTransfer {
    path: Mutex<Option<PathBuf>>,
    notify: Notify,
}

impl BackgroundTransfer {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            path: Mutex::new(None),
            notify: Notify::new(),
        })
    }

    pub(super) fn activate(&self, path: PathBuf) {
        *self.path.lock().unwrap_or_else(|e| e.into_inner()) = Some(path);
        self.notify.notify_waiters();
    }
}

pub(super) fn is_auto_background_allowed(command: &str) -> bool {
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

#[allow(clippy::too_many_arguments)]
pub(super) fn foreground_shell_metadata(
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

pub(super) async fn capture_stream_with_background_transfer(
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
pub(super) async fn capture_stream(mut stream: impl AsyncRead + Unpin) -> CapturedOutput {
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
