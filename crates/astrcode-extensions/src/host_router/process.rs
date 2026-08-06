//! 受并发、时间和输出上限约束的扩展子进程执行器。

use std::{future::Future, process::Stdio, time::Duration};

use astrcode_extension_sdk::{
    host::{HOST_PROCESS_DEFAULT_TIMEOUT_MS, HOST_PROCESS_MAX_TIMEOUT_MS, HostProcessRequest},
    s5r::ErrorPayload,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Semaphore, SemaphorePermit},
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;

use super::{
    capability::ProcessCapability, decode_host_input, path::canonicalize_workspace_path,
    run_blocking_io,
};

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

pub(super) struct ProcessGroup {
    runner: ProcessRunner,
    default_working_dir: Option<String>,
}

impl ProcessGroup {
    pub(super) fn new(default_working_dir: Option<String>) -> Self {
        Self {
            runner: ProcessRunner::default(),
            default_working_dir,
        }
    }

    pub(super) async fn invoke(
        &self,
        capability: ProcessCapability,
        input: Value,
        working_dir: Option<&str>,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<Value, ErrorPayload> {
        match capability {
            ProcessCapability::Spawn => {
                let request = decode_host_input(input)?;
                let working_dir = working_dir
                    .map(str::to_owned)
                    .or_else(|| self.default_working_dir.clone());
                self.runner
                    .spawn(request, working_dir.as_deref(), cancel_token)
                    .await
            },
        }
    }

    pub(super) fn is_available(&self, working_dir: Option<&str>) -> bool {
        working_dir.is_some() || self.default_working_dir.is_some()
    }
}

struct ProcessRunner {
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
    async fn spawn(
        &self,
        request: HostProcessRequest,
        working_dir: Option<&str>,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<Value, ErrorPayload> {
        let timeout = validated_timeout(request.timeout_ms)?;
        let deadline = Instant::now() + timeout;
        let _permit = self.acquire_permit(deadline, cancel_token).await?;
        if request.command.is_empty() {
            return Err(ErrorPayload::new(
                "invalid_input",
                "command must not be empty",
            ));
        }
        if request
            .stdin
            .as_ref()
            .is_some_and(|value| value.len() > MAX_STDIN_BYTES)
        {
            return Err(ErrorPayload::new(
                "input_too_large",
                format!("stdin exceeds {MAX_STDIN_BYTES} bytes"),
            ));
        }
        let HostProcessRequest {
            command,
            args,
            cwd: relative_cwd,
            stdin,
            timeout_ms: _,
        } = request;
        let working_dir = working_dir.map(str::to_owned);
        let cwd =
            run_blocking_io(move || resolve_cwd(working_dir.as_deref(), relative_cwd.as_deref()))
                .await?;
        ensure_spawn_active(deadline, cancel_token)?;

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
    let path = canonicalize_workspace_path(root, relative_cwd.unwrap_or("."))?;
    if !path.is_dir() {
        return Err(ErrorPayload::new(
            "invalid_input",
            "process cwd must be an existing directory",
        ));
    }
    Ok(path)
}

fn validated_timeout(timeout_ms: Option<u64>) -> Result<Duration, ErrorPayload> {
    let timeout_ms = timeout_ms.unwrap_or(HOST_PROCESS_DEFAULT_TIMEOUT_MS);
    if !(1..=HOST_PROCESS_MAX_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(ErrorPayload::new(
            "invalid_input",
            format!("timeout_ms must be between 1 and {HOST_PROCESS_MAX_TIMEOUT_MS}"),
        ));
    }
    Ok(Duration::from_millis(timeout_ms))
}

fn ensure_spawn_active(
    deadline: Instant,
    cancel_token: Option<&CancellationToken>,
) -> Result<(), ErrorPayload> {
    if cancel_token.is_some_and(CancellationToken::is_cancelled) {
        return Err(cancelled());
    }
    if Instant::now() >= deadline {
        return Err(ErrorPayload::new("timeout", "process timed out"));
    }
    Ok(())
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

    #[test]
    fn spawn_checkpoint_rejects_cancellation_and_elapsed_deadline() {
        let token = CancellationToken::new();
        token.cancel();
        let cancelled = ensure_spawn_active(Instant::now() + Duration::from_secs(1), Some(&token))
            .expect_err("cancelled process must not spawn");
        assert_eq!(cancelled.code, "cancelled");

        let timed_out = ensure_spawn_active(Instant::now() - Duration::from_secs(1), None)
            .expect_err("expired process must not spawn");
        assert_eq!(timed_out.code, "timeout");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn drains_output_while_writing_stdin() {
        let workspace = tempdir().expect("workspace");
        let mut request = HostProcessRequest::new("/bin/sh");
        request.args = vec![
            "-c".into(),
            "dd if=/dev/zero bs=131072 count=1 2>/dev/null; cat >/dev/null".into(),
        ];
        request.stdin = Some("x".repeat(128 * 1024));
        request.timeout_ms = Some(5_000);
        let output = ProcessRunner::default()
            .spawn(request, workspace.path().to_str(), None)
            .await
            .expect("process should not deadlock on full stdin and stdout pipes");

        assert_eq!(output["success"], true);
        assert_eq!(output["stdout"].as_str().expect("stdout").len(), 128 * 1024);
    }

    #[tokio::test]
    async fn executes_process_in_workspace() {
        let workspace = tempdir().expect("workspace");
        let runner = ProcessRunner::default();
        let mut request = HostProcessRequest::new("rustc");
        request.args = vec!["--version".into()];
        let output = runner
            .spawn(request, workspace.path().to_str(), None)
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
        let mut request = HostProcessRequest::new("rustc");
        request.cwd = Some("..".into());
        let error = runner
            .spawn(request, workspace.path().to_str(), None)
            .await
            .expect_err("parent cwd must be rejected");

        assert_eq!(error.code, "permission_denied");
    }
}
