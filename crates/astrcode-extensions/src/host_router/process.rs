//! 受并发、时间和输出上限约束的扩展子进程执行器。

use std::{future::Future, process::Stdio, time::Duration};

use astrcode_extension_sdk::s5r::ErrorPayload;
use astrcode_support::hostpaths::resolve_under_workspace_root;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Semaphore, SemaphorePermit},
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CONCURRENT_PROCESSES: usize = 8;
const MAX_STREAM_BYTES: usize = 1024 * 1024;
const MAX_COMBINED_BYTES: usize = 1024 * 1024;
const MAX_STDIN_BYTES: usize = 1024 * 1024;

const NONINTERACTIVE_ENV: &[(&str, &str)] = &[
    ("PAGER", "cat"),
    ("MANPAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("TERM", "dumb"),
    ("PIP_PROGRESS_BAR", "off"),
];
const SAFE_INHERITED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "TMPDIR",
    "TEMP",
    "TMP",
    "SYSTEMROOT",
    "COMSPEC",
    "PATHEXT",
];

pub(super) struct ProcessRunner {
    permits: Semaphore,
}

impl Default for ProcessRunner {
    fn default() -> Self {
        Self {
            permits: Semaphore::new(MAX_CONCURRENT_PROCESSES),
        }
    }
}

impl ProcessRunner {
    pub(super) async fn spawn(
        &self,
        input: Value,
        working_dir: Option<&str>,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<Value, ErrorPayload> {
        let timeout = bounded_timeout(&input);
        let deadline = Instant::now() + timeout;
        let _permit = self.acquire_permit(deadline, cancel_token).await?;
        let command = required_string(&input, "command")?;
        let args = string_array(&input, "args")?;
        let stdin = input.get("stdin").and_then(Value::as_str);
        if stdin.is_some_and(|value| value.len() > MAX_STDIN_BYTES) {
            return Err(ErrorPayload::new(
                "input_too_large",
                format!("stdin exceeds {MAX_STDIN_BYTES} bytes"),
            ));
        }
        let cwd = resolve_cwd(working_dir, input.get("cwd").and_then(Value::as_str))?;

        let mut process = tokio::process::Command::new(command);
        process
            .args(args)
            .current_dir(cwd)
            .kill_on_drop(true)
            .env_clear()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in safe_child_env() {
            process.env(key, value);
        }
        for (key, value) in NONINTERACTIVE_ENV {
            process.env(key, value);
        }
        if stdin.is_some() {
            process.stdin(Stdio::piped());
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            process.as_std_mut().process_group(0);
        }

        let mut child = process
            .spawn()
            .map_err(|error| ErrorPayload::new("spawn_failed", error.to_string()))?;
        let child_pid = child.id();
        let mut child_stdin = child.stdin.take();
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| ErrorPayload::new("process_failed", "child stdout pipe unavailable"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| ErrorPayload::new("process_failed", "child stderr pipe unavailable"))?;

        let write_stdin = async move {
            if let (Some(content), Some(mut pipe)) = (stdin, child_stdin.take()) {
                pipe.write_all(content.as_bytes())
                    .await
                    .map_err(|error| ErrorPayload::new("stdin_failed", error.to_string()))?;
            }
            Ok::<(), ErrorPayload>(())
        };
        let collect_output = async {
            let mut stdout_bytes = Vec::new();
            let mut stderr_bytes = Vec::new();
            let mut combined = Vec::new();
            let mut stdout_truncated = false;
            let mut stderr_truncated = false;
            let mut combined_truncated = false;
            let mut stdout_buffer = [0_u8; 8192];
            let mut stderr_buffer = [0_u8; 8192];
            let mut stdout_open = true;
            let mut stderr_open = true;
            while stdout_open || stderr_open {
                tokio::select! {
                    read = read_if_open(&mut stdout, &mut stdout_buffer, stdout_open) => {
                        match read {
                            Ok(0) => stdout_open = false,
                            Ok(read) => {
                                stdout_truncated |= append_bounded(
                                    &mut stdout_bytes,
                                    &stdout_buffer[..read],
                                    MAX_STREAM_BYTES,
                                );
                                combined_truncated |= append_bounded(
                                    &mut combined,
                                    &stdout_buffer[..read],
                                    MAX_COMBINED_BYTES,
                                );
                            },
                            Err(error) => {
                                return Err(ErrorPayload::new("stdout_failed", error.to_string()));
                            },
                        }
                    },
                    read = read_if_open(&mut stderr, &mut stderr_buffer, stderr_open) => {
                        match read {
                            Ok(0) => stderr_open = false,
                            Ok(read) => {
                                stderr_truncated |= append_bounded(
                                    &mut stderr_bytes,
                                    &stderr_buffer[..read],
                                    MAX_STREAM_BYTES,
                                );
                                combined_truncated |= append_bounded(
                                    &mut combined,
                                    &stderr_buffer[..read],
                                    MAX_COMBINED_BYTES,
                                );
                            },
                            Err(error) => {
                                return Err(ErrorPayload::new("stderr_failed", error.to_string()));
                            },
                        }
                    },
                }
            }
            Ok((
                stdout_bytes,
                stderr_bytes,
                combined,
                stdout_truncated,
                stderr_truncated,
                combined_truncated,
            ))
        };
        let collect = async {
            let ((), output) = tokio::try_join!(write_stdin, collect_output)?;
            let status = child
                .wait()
                .await
                .map_err(|error| ErrorPayload::new("process_failed", error.to_string()))?;
            Ok::<_, ErrorPayload>((output, status))
        };

        let outcome = run_until_deadline(collect, deadline, cancel_token).await;
        match outcome {
            Ok(Ok((
                (stdout, stderr, combined, stdout_truncated, stderr_truncated, combined_truncated),
                status,
            ))) => Ok(json!({
                "status": status.code(),
                "success": status.success(),
                "stdout": String::from_utf8_lossy(&stdout),
                "stderr": String::from_utf8_lossy(&stderr),
                "combined": String::from_utf8_lossy(&combined),
                "stdout_truncated": stdout_truncated,
                "stderr_truncated": stderr_truncated,
                "combined_truncated": combined_truncated,
            })),
            Ok(Err(error)) | Err(error) => {
                terminate_child(&mut child, child_pid).await;
                Err(error)
            },
        }
    }

    async fn acquire_permit<'a>(
        &'a self,
        deadline: Instant,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<SemaphorePermit<'a>, ErrorPayload> {
        let acquire = async {
            timeout_at(deadline, self.permits.acquire())
                .await
                .map_err(|_| {
                    ErrorPayload::new("timeout", "process timed out waiting for capacity")
                })?
                .map_err(|_| ErrorPayload::new("backend_unavailable", "process runner stopped"))
        };
        match cancel_token {
            Some(token) => {
                tokio::select! {
                    biased;
                    () = token.cancelled() => Err(cancelled()),
                    result = acquire => result,
                }
            },
            None => acquire.await,
        }
    }
}

async fn run_until_deadline<F, T>(
    operation: F,
    deadline: Instant,
    cancel_token: Option<&CancellationToken>,
) -> Result<Result<T, ErrorPayload>, ErrorPayload>
where
    F: Future<Output = Result<T, ErrorPayload>>,
{
    let timed = async {
        timeout_at(deadline, operation)
            .await
            .map_err(|_| ErrorPayload::new("timeout", "process timed out"))
    };
    match cancel_token {
        Some(token) => {
            tokio::select! {
                biased;
                () = token.cancelled() => Err(cancelled()),
                result = timed => result,
            }
        },
        None => timed.await,
    }
}

async fn read_if_open<R>(reader: &mut R, buffer: &mut [u8], open: bool) -> std::io::Result<usize>
where
    R: AsyncReadExt + Unpin,
{
    if open {
        reader.read(buffer).await
    } else {
        std::future::pending().await
    }
}

async fn terminate_child(child: &mut tokio::process::Child, child_pid: Option<u32>) {
    kill_process_group(child_pid);
    #[cfg(not(unix))]
    let _ = child.start_kill();
    let _ = child.wait().await;
}

fn resolve_cwd(
    working_dir: Option<&str>,
    relative_cwd: Option<&str>,
) -> Result<std::path::PathBuf, ErrorPayload> {
    let root = working_dir
        .ok_or_else(|| ErrorPayload::new("backend_unavailable", "working_dir not set"))?;
    let path = resolve_under_workspace_root(root, relative_cwd.unwrap_or("."))
        .map_err(|error| ErrorPayload::new(error.code, error.message))?;
    if !path.is_dir() {
        return Err(ErrorPayload::new(
            "invalid_input",
            "process cwd must be an existing directory",
        ));
    }
    Ok(path)
}

fn required_string<'a>(input: &'a Value, key: &str) -> Result<&'a str, ErrorPayload> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ErrorPayload::new("invalid_input", format!("{key} must be a string")))
}

fn string_array<'a>(input: &'a Value, key: &str) -> Result<Vec<&'a str>, ErrorPayload> {
    let Some(value) = input.get(key) else {
        return Ok(Vec::new());
    };
    value
        .as_array()
        .ok_or_else(|| ErrorPayload::new("invalid_input", format!("{key} must be an array")))?
        .iter()
        .map(|value| {
            value.as_str().ok_or_else(|| {
                ErrorPayload::new("invalid_input", format!("{key} values must be strings"))
            })
        })
        .collect()
}

fn bounded_timeout(input: &Value) -> Duration {
    input
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_TIMEOUT)
        .min(MAX_TIMEOUT)
}

fn append_bounded(target: &mut Vec<u8>, chunk: &[u8], limit: usize) -> bool {
    let accepted = limit.saturating_sub(target.len()).min(chunk.len());
    target.extend_from_slice(&chunk[..accepted]);
    accepted < chunk.len()
}

fn safe_child_env() -> impl Iterator<Item = (String, String)> {
    std::env::vars()
        .filter(|(key, _)| SAFE_INHERITED_ENV.contains(&key.as_str()) || key.starts_with("LC_"))
}

fn cancelled() -> ErrorPayload {
    ErrorPayload::new("cancelled", "process cancelled")
}

#[cfg(unix)]
fn kill_process_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // SAFETY: the child was started as the leader of its own process group.
        unsafe {
            let _ = libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: Option<u32>) {}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn drains_output_while_writing_stdin() {
        let workspace = tempdir().expect("workspace");
        let output = ProcessRunner::default()
            .spawn(
                json!({
                    "command": "/bin/sh",
                    "args": ["-c", "dd if=/dev/zero bs=131072 count=1 2>/dev/null; cat >/dev/null"],
                    "stdin": "x".repeat(128 * 1024),
                    "timeout_ms": 5_000
                }),
                workspace.path().to_str(),
                None,
            )
            .await
            .expect("process should not deadlock on full stdin and stdout pipes");

        assert_eq!(output["success"], true);
        assert_eq!(output["stdout"].as_str().expect("stdout").len(), 128 * 1024);
    }

    #[tokio::test]
    async fn executes_process_in_workspace() {
        let workspace = tempdir().expect("workspace");
        let runner = ProcessRunner::default();
        let output = runner
            .spawn(
                json!({ "command": "rustc", "args": ["--version"] }),
                workspace.path().to_str(),
                None,
            )
            .await
            .expect("rustc should run");

        assert_eq!(output["success"], true);
        assert!(
            output["stdout"]
                .as_str()
                .is_some_and(|text| text.contains("rustc"))
        );
    }

    #[tokio::test]
    async fn rejects_cwd_outside_workspace() {
        let workspace = tempdir().expect("workspace");
        let runner = ProcessRunner::default();
        let error = runner
            .spawn(
                json!({ "command": "rustc", "cwd": ".." }),
                workspace.path().to_str(),
                None,
            )
            .await
            .expect_err("parent cwd must be rejected");

        assert_eq!(error.code, "permission_denied");
    }
}
