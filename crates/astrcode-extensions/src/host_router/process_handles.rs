//! Session-owned process handles.

use std::{collections::HashMap, process::Stdio, sync::Arc, time::Duration};

use astrcode_extension_sdk::{
    host::{
        HOST_PROCESS_MAX_WAIT_MS, HostProcessHandleOutput, HostProcessInputAction,
        HostProcessInputRequest, HostProcessLifetime, HostProcessListOutput, HostProcessReadOutput,
        HostProcessReadRequest, HostProcessStartRequest, HostProcessState, HostProcessStatusOutput,
        HostProcessTargetRequest,
    },
    wire::{ErrorPayload, WireErrorCode},
};
use parking_lot::Mutex;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    sync::{Mutex as AsyncMutex, Notify, OwnedSemaphorePermit, Semaphore},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{
    ExtensionInstanceId,
    process::{resolve_cwd, spawn_supervised, validated_timeout},
};
use crate::process_supervision::SupervisedChild;

const MAX_SESSION_PROCESSES: usize = 8;
const MAX_SESSION_PROCESS_HANDLES: usize = 64;
const MAX_BUFFER_BYTES: usize = 1024 * 1024;
const TERMINATION_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
enum ProcessTermination {
    Exited,
    TimedOut,
    Killed,
    Cancelled,
}

#[derive(Default)]
struct IncrementalOutput {
    bytes: Vec<u8>,
    dropped_bytes: usize,
}

impl IncrementalOutput {
    fn append(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > MAX_BUFFER_BYTES {
            let overflow = self.bytes.len() - MAX_BUFFER_BYTES;
            self.bytes.drain(..overflow);
            self.dropped_bytes = self.dropped_bytes.saturating_add(overflow);
        }
    }

    fn take(&mut self, retain_incomplete_utf8: bool) -> (String, usize) {
        let mut bytes = std::mem::take(&mut self.bytes);
        if retain_incomplete_utf8 && let Some(incomplete_start) = incomplete_utf8_start(&bytes) {
            self.bytes = bytes.split_off(incomplete_start);
        }
        let dropped = std::mem::take(&mut self.dropped_bytes);
        (String::from_utf8_lossy(&bytes).into_owned(), dropped)
    }

    fn is_empty(&self) -> bool {
        self.dropped_bytes == 0
            && (self.bytes.is_empty() || incomplete_utf8_start(&self.bytes) == Some(0))
    }
}

fn incomplete_utf8_start(bytes: &[u8]) -> Option<usize> {
    (bytes.len().saturating_sub(3)..bytes.len()).find(|start| {
        std::str::from_utf8(&bytes[*start..])
            .is_err_and(|error| error.valid_up_to() == 0 && error.error_len().is_none())
    })
}

#[derive(Default)]
struct ProcessOutputState {
    stdout: IncrementalOutput,
    stderr: IncrementalOutput,
    combined: IncrementalOutput,
    running: bool,
    status: Option<i32>,
    termination: Option<ProcessTermination>,
}

struct ProcessController {
    stdin: Arc<AsyncMutex<Option<tokio::process::ChildStdin>>>,
    cancel: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
}

enum ProcessLifetimeOwner {
    Call(CancellationToken),
    Session,
}

struct ProcessEntry {
    id: String,
    session_id: String,
    extension_instance_id: ExtensionInstanceId,
    output: Mutex<ProcessOutputState>,
    output_changed: Notify,
    requested_termination: Mutex<Option<ProcessTermination>>,
    lifetime_owner: Mutex<ProcessLifetimeOwner>,
    controller: ProcessController,
    _handle_permit: OwnedSemaphorePermit,
}

impl ProcessEntry {
    fn status(&self) -> HostProcessStatusOutput {
        let output = self.output.lock();
        HostProcessStatusOutput {
            id: self.id.clone(),
            state: process_state(&output),
        }
    }

    fn has_unread_output_or_stopped(&self) -> bool {
        let output = self.output.lock();
        !output.running
            || !output.stdout.is_empty()
            || !output.stderr.is_empty()
            || !output.combined.is_empty()
    }

    fn take_output(&self) -> HostProcessReadOutput {
        let mut output = self.output.lock();
        let retain_incomplete_utf8 = output.running;
        let (stdout, stdout_dropped) = output.stdout.take(retain_incomplete_utf8);
        let (stderr, stderr_dropped) = output.stderr.take(retain_incomplete_utf8);
        let (combined, combined_dropped) = output.combined.take(retain_incomplete_utf8);
        HostProcessReadOutput {
            id: self.id.clone(),
            stdout,
            stderr,
            combined,
            dropped_bytes: stdout_dropped
                .saturating_add(stderr_dropped)
                .max(combined_dropped),
            state: process_state(&output),
        }
    }

    fn append_stdout(&self, chunk: &[u8]) {
        let mut output = self.output.lock();
        output.stdout.append(chunk);
        output.combined.append(chunk);
        drop(output);
        self.output_changed.notify_waiters();
    }

    fn append_stderr(&self, chunk: &[u8]) {
        let mut output = self.output.lock();
        output.stderr.append(chunk);
        output.combined.append(chunk);
        drop(output);
        self.output_changed.notify_waiters();
    }

    fn finish(&self, status: Option<i32>, termination: ProcessTermination) {
        let mut output = self.output.lock();
        if !output.running && output.termination.is_some() {
            return;
        }
        output.running = false;
        if status.is_some() || output.status.is_none() {
            output.status = status;
        }
        output.termination = Some(termination);
        drop(output);
        self.output_changed.notify_waiters();
    }

    fn cancel(&self, termination: ProcessTermination) {
        self.request_termination(termination);
        self.controller.cancel.cancel();
    }

    fn request_termination(&self, termination: ProcessTermination) {
        self.requested_termination.lock().get_or_insert(termination);
    }

    fn is_call_owned(&self) -> bool {
        matches!(*self.lifetime_owner.lock(), ProcessLifetimeOwner::Call(_))
    }

    fn promote(&self) -> Result<(), ErrorPayload> {
        let mut owner = self.lifetime_owner.lock();
        match &*owner {
            ProcessLifetimeOwner::Session => Ok(()),
            ProcessLifetimeOwner::Call(cancellation) if cancellation.is_cancelled() => {
                Err(ErrorPayload::new(
                    WireErrorCode::Cancelled,
                    "process invocation was cancelled before it could be promoted",
                ))
            },
            ProcessLifetimeOwner::Call(_) => {
                *owner = ProcessLifetimeOwner::Session;
                Ok(())
            },
        }
    }
}

fn process_state(output: &ProcessOutputState) -> HostProcessState {
    match output.termination {
        None => HostProcessState::Running {},
        Some(ProcessTermination::Exited) => HostProcessState::Exited {
            status: output.status,
        },
        Some(ProcessTermination::TimedOut) => HostProcessState::TimedOut {
            status: output.status,
        },
        Some(ProcessTermination::Killed) => HostProcessState::Killed {
            status: output.status,
        },
        Some(ProcessTermination::Cancelled) => HostProcessState::Cancelled {
            status: output.status,
        },
    }
}

pub(super) struct ProcessHandleStore {
    entries: Mutex<HashMap<String, Arc<ProcessEntry>>>,
    session_process_permits: Mutex<HashMap<String, Arc<Semaphore>>>,
    session_handle_permits: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl Default for ProcessHandleStore {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            session_process_permits: Mutex::new(HashMap::new()),
            session_handle_permits: Mutex::new(HashMap::new()),
        }
    }
}

impl ProcessHandleStore {
    pub(super) async fn start(
        &self,
        request: HostProcessStartRequest,
        working_dir: Option<&str>,
        session_id: &str,
        extension_instance_id: ExtensionInstanceId,
        call_cancellation: Option<&CancellationToken>,
    ) -> Result<HostProcessHandleOutput, ErrorPayload> {
        let timeout = validate_start_request(&request)?;
        let lifetime_owner = match request.lifetime {
            HostProcessLifetime::Call => {
                ProcessLifetimeOwner::Call(call_cancellation.cloned().ok_or_else(|| {
                    ErrorPayload::new(
                        WireErrorCode::ContextUnavailable,
                        "call-owned process requires invocation cancellation",
                    )
                })?)
            },
            HostProcessLifetime::Session => ProcessLifetimeOwner::Session,
        };
        let cwd = resolve_cwd(working_dir, request.cwd.as_deref())?;
        let handle_permit = self
            .session_handle_permits
            .lock()
            .entry(session_id.to_owned())
            .or_insert_with(|| Arc::new(Semaphore::new(MAX_SESSION_PROCESS_HANDLES)))
            .clone()
            .try_acquire_owned()
            .map_err(|_| process_handle_limit_error())?;
        let process_permit = self
            .session_process_permits
            .lock()
            .entry(session_id.to_owned())
            .or_insert_with(|| Arc::new(Semaphore::new(MAX_SESSION_PROCESSES)))
            .clone()
            .try_acquire_owned()
            .map_err(|_| process_limit_error())?;
        let mut command = tokio::process::Command::new(&request.command);
        command
            .args(&request.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = spawn_supervised(command)?;
        let stdin = child.take_stdin().ok_or_else(|| {
            ErrorPayload::new(WireErrorCode::ProcessFailed, "child stdin pipe unavailable")
        })?;
        let stdout = child.take_stdout().ok_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::ProcessFailed,
                "child stdout pipe unavailable",
            )
        })?;
        let stderr = child.take_stderr().ok_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::ProcessFailed,
                "child stderr pipe unavailable",
            )
        })?;
        let stdin = Arc::new(AsyncMutex::new(Some(stdin)));
        let cancel = CancellationToken::new();
        let id = format!("process-{}", uuid::Uuid::new_v4());
        let entry = Arc::new(ProcessEntry {
            id: id.clone(),
            session_id: session_id.to_owned(),
            extension_instance_id,
            output: Mutex::new(ProcessOutputState {
                running: true,
                ..Default::default()
            }),
            output_changed: Notify::new(),
            requested_termination: Mutex::new(None),
            lifetime_owner: Mutex::new(lifetime_owner),
            controller: ProcessController {
                stdin: Arc::clone(&stdin),
                cancel: cancel.clone(),
                task: Mutex::new(None),
            },
            _handle_permit: handle_permit,
        });
        self.entries.lock().insert(id.clone(), Arc::clone(&entry));

        let task_entry = Arc::clone(&entry);
        let task = tokio::spawn(async move {
            let _permit = process_permit;
            run_pipe_process(task_entry, child, stdout, stderr, stdin, cancel, timeout).await;
        });
        *entry.controller.task.lock() = Some(task);
        Ok(HostProcessHandleOutput { id })
    }

    pub(super) async fn read(
        &self,
        request: HostProcessReadRequest,
        session_id: &str,
        extension_instance_id: ExtensionInstanceId,
        cancellation: Option<&CancellationToken>,
    ) -> Result<HostProcessReadOutput, ErrorPayload> {
        let wait_ms = request.wait_ms.unwrap_or(0);
        if wait_ms > HOST_PROCESS_MAX_WAIT_MS {
            return Err(ErrorPayload::new(
                WireErrorCode::InvalidInput,
                format!("wait_ms must not exceed {HOST_PROCESS_MAX_WAIT_MS}"),
            ));
        }
        let entry = self.owned_entry(&request.id, session_id, extension_instance_id)?;
        if wait_ms > 0 {
            let notified = entry.output_changed.notified();
            if !entry.has_unread_output_or_stopped() {
                let wait = tokio::time::sleep(Duration::from_millis(wait_ms));
                tokio::pin!(wait);
                if let Some(cancellation) = cancellation {
                    tokio::select! {
                        () = cancellation.cancelled() => {
                            if entry.is_call_owned() {
                                self.remove_entry_if_same(&request.id, &entry);
                                entry.cancel(ProcessTermination::Cancelled);
                            }
                            return Err(ErrorPayload::new(WireErrorCode::Cancelled, "process read cancelled"));
                        },
                        () = notified => {},
                        () = &mut wait => {},
                    }
                } else {
                    tokio::select! {
                        () = notified => {},
                        () = &mut wait => {},
                    }
                }
            }
        }
        let output = entry.take_output();
        if !output.state.is_running() {
            self.remove_entry_if_same(&request.id, &entry);
        }
        Ok(output)
    }

    pub(super) async fn input(
        &self,
        request: HostProcessInputRequest,
        session_id: &str,
        extension_instance_id: ExtensionInstanceId,
    ) -> Result<(), ErrorPayload> {
        let entry = self.owned_entry(&request.id, session_id, extension_instance_id)?;
        match request.action {
            HostProcessInputAction::Write { input } => {
                let mut stdin = entry.controller.stdin.lock().await;
                let stdin = stdin
                    .as_mut()
                    .ok_or_else(|| process_not_running(&request.id))?;
                stdin.write_all(input.as_bytes()).await.map_err(|error| {
                    ErrorPayload::new(WireErrorCode::StdinFailed, error.to_string())
                })?;
                stdin.flush().await.map_err(|error| {
                    ErrorPayload::new(WireErrorCode::StdinFailed, error.to_string())
                })
            },
            HostProcessInputAction::Close => {
                entry.controller.stdin.lock().await.take();
                Ok(())
            },
        }
    }

    pub(super) fn status(
        &self,
        request: HostProcessTargetRequest,
        session_id: &str,
        extension_instance_id: ExtensionInstanceId,
    ) -> Result<HostProcessStatusOutput, ErrorPayload> {
        Ok(self
            .owned_entry(&request.id, session_id, extension_instance_id)?
            .status())
    }

    pub(super) fn promote(
        &self,
        request: HostProcessTargetRequest,
        session_id: &str,
        extension_instance_id: ExtensionInstanceId,
    ) -> Result<(), ErrorPayload> {
        self.owned_entry(&request.id, session_id, extension_instance_id)?
            .promote()
    }

    pub(super) fn kill(
        &self,
        request: HostProcessTargetRequest,
        session_id: &str,
        extension_instance_id: ExtensionInstanceId,
    ) -> Result<(), ErrorPayload> {
        let entry = self.remove_owned_entry(&request.id, session_id, extension_instance_id)?;
        entry.cancel(ProcessTermination::Killed);
        Ok(())
    }

    pub(super) fn list(
        &self,
        session_id: &str,
        extension_instance_id: ExtensionInstanceId,
    ) -> HostProcessListOutput {
        let mut processes = self
            .entries
            .lock()
            .values()
            .filter(|entry| {
                entry.session_id == session_id
                    && entry.extension_instance_id == extension_instance_id
            })
            .map(|entry| entry.status())
            .collect::<Vec<_>>();
        processes.sort_by(|left, right| left.id.cmp(&right.id));
        HostProcessListOutput { processes }
    }

    pub(super) fn cleanup_session(&self, session_id: &str) {
        let entries = remove_matching(&self.entries, |entry| entry.session_id == session_id);
        self.session_process_permits.lock().remove(session_id);
        self.session_handle_permits.lock().remove(session_id);
        for entry in entries {
            entry.cancel(ProcessTermination::Killed);
        }
    }

    pub(super) fn cleanup_extension(&self, extension_instance_id: ExtensionInstanceId) {
        let entries = remove_matching(&self.entries, |entry| {
            entry.extension_instance_id == extension_instance_id
        });
        for entry in entries {
            entry.cancel(ProcessTermination::Killed);
        }
    }

    fn owned_entry(
        &self,
        id: &str,
        session_id: &str,
        extension_instance_id: ExtensionInstanceId,
    ) -> Result<Arc<ProcessEntry>, ErrorPayload> {
        self.entries
            .lock()
            .get(id)
            .filter(|entry| {
                entry.session_id == session_id
                    && entry.extension_instance_id == extension_instance_id
            })
            .cloned()
            .ok_or_else(|| unknown_process(id))
    }

    fn remove_owned_entry(
        &self,
        id: &str,
        session_id: &str,
        extension_instance_id: ExtensionInstanceId,
    ) -> Result<Arc<ProcessEntry>, ErrorPayload> {
        let mut entries = self.entries.lock();
        let owned = entries.get(id).is_some_and(|entry| {
            entry.session_id == session_id && entry.extension_instance_id == extension_instance_id
        });
        if !owned {
            return Err(unknown_process(id));
        }
        entries.remove(id).ok_or_else(|| unknown_process(id))
    }

    fn remove_entry_if_same(&self, id: &str, expected: &Arc<ProcessEntry>) {
        let mut entries = self.entries.lock();
        if entries
            .get(id)
            .is_some_and(|entry| Arc::ptr_eq(entry, expected))
        {
            entries.remove(id);
        }
    }
}

impl Drop for ProcessHandleStore {
    fn drop(&mut self) {
        for entry in self.entries.get_mut().drain().map(|(_, entry)| entry) {
            entry.cancel(ProcessTermination::Killed);
        }
    }
}

async fn run_pipe_process(
    entry: Arc<ProcessEntry>,
    mut child: SupervisedChild,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    stdin: Arc<AsyncMutex<Option<tokio::process::ChildStdin>>>,
    cancellation: CancellationToken,
    timeout: Duration,
) {
    let readers = async {
        tokio::join!(
            pump_output(stdout, Arc::clone(&entry), false),
            pump_output(stderr, Arc::clone(&entry), true),
        );
    };
    tokio::pin!(readers);
    let deadline = tokio::time::Instant::now() + timeout;

    enum FirstCompletion {
        Child(Option<i32>),
        Readers,
        Terminate(ProcessTermination),
    }

    let first = tokio::select! {
        status = child.wait() => FirstCompletion::Child(status.ok().and_then(|status| status.code())),
        () = &mut readers => FirstCompletion::Readers,
        termination = requested_termination(Arc::clone(&entry), cancellation.clone(), deadline) => {
            FirstCompletion::Terminate(termination)
        },
    };
    let (status, termination) = match first {
        FirstCompletion::Child(status) => {
            let termination = tokio::select! {
                () = &mut readers => ProcessTermination::Exited,
                termination = requested_termination(Arc::clone(&entry), cancellation, deadline) => {
                    let status = child
                        .terminate(TERMINATION_GRACE)
                        .await
                        .ok()
                        .and_then(|status| status.code())
                        .or(status);
                    readers.await;
                    stdin.lock().await.take();
                    entry.finish(status, termination);
                    return;
                },
            };
            (status, termination)
        },
        FirstCompletion::Readers => {
            tokio::select! {
                status = child.wait() => (
                    status.ok().and_then(|status| status.code()),
                    ProcessTermination::Exited,
                ),
                termination = requested_termination(Arc::clone(&entry), cancellation, deadline) => (
                    child.terminate(TERMINATION_GRACE).await.ok().and_then(|status| status.code()),
                    termination,
                ),
            }
        },
        FirstCompletion::Terminate(termination) => {
            let status = child
                .terminate(TERMINATION_GRACE)
                .await
                .ok()
                .and_then(|status| status.code());
            readers.await;
            (status, termination)
        },
    };
    stdin.lock().await.take();
    entry.finish(status, termination);
}

async fn requested_termination(
    entry: Arc<ProcessEntry>,
    cancellation: CancellationToken,
    deadline: tokio::time::Instant,
) -> ProcessTermination {
    tokio::select! {
        () = cancellation.cancelled() => entry
            .requested_termination
            .lock()
            .unwrap_or(ProcessTermination::Killed),
        () = call_owner_cancelled(&entry) => ProcessTermination::Cancelled,
        () = tokio::time::sleep_until(deadline) => ProcessTermination::TimedOut,
    }
}

async fn call_owner_cancelled(entry: &ProcessEntry) {
    let cancellation = {
        let owner = entry.lifetime_owner.lock();
        match &*owner {
            ProcessLifetimeOwner::Call(cancellation) => Some(cancellation.clone()),
            ProcessLifetimeOwner::Session => None,
        }
    };
    let Some(cancellation) = cancellation else {
        return std::future::pending().await;
    };
    cancellation.cancelled().await;
    if !entry.is_call_owned() {
        std::future::pending().await
    }
}

async fn pump_output(mut stream: impl AsyncRead + Unpin, entry: Arc<ProcessEntry>, stderr: bool) {
    let mut buffer = [0_u8; 8192];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) => return,
            Ok(read) if stderr => entry.append_stderr(&buffer[..read]),
            Ok(read) => entry.append_stdout(&buffer[..read]),
            Err(error) => {
                tracing::warn!(process_id = %entry.id, %error, "process output read failed");
                return;
            },
        }
    }
}

fn validate_start_request(request: &HostProcessStartRequest) -> Result<Duration, ErrorPayload> {
    if request.command.trim().is_empty() {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
            "command must not be empty",
        ));
    }
    validated_timeout(request.timeout_ms)
}

fn remove_matching(
    entries: &Mutex<HashMap<String, Arc<ProcessEntry>>>,
    predicate: impl Fn(&ProcessEntry) -> bool,
) -> Vec<Arc<ProcessEntry>> {
    let mut entries = entries.lock();
    let ids = entries
        .iter()
        .filter(|(_, entry)| predicate(entry))
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    ids.into_iter()
        .filter_map(|id| entries.remove(&id))
        .collect()
}

fn unknown_process(id: &str) -> ErrorPayload {
    ErrorPayload::new(
        WireErrorCode::InvalidInput,
        format!("unknown process handle: {id}"),
    )
}

fn process_not_running(id: &str) -> ErrorPayload {
    ErrorPayload::new(
        WireErrorCode::ProcessFailed,
        format!("process is no longer running: {id}"),
    )
}

fn process_limit_error() -> ErrorPayload {
    ErrorPayload::new(
        WireErrorCode::PeerBusy,
        format!("at most {MAX_SESSION_PROCESSES} session processes may run concurrently"),
    )
}

fn process_handle_limit_error() -> ErrorPayload {
    ErrorPayload::new(
        WireErrorCode::PeerBusy,
        format!(
            "at most {MAX_SESSION_PROCESS_HANDLES} process handles may be retained per session; \
             read completed output or close unused handles"
        ),
    )
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn incremental_output_retains_incomplete_utf8_until_the_next_read() {
        let mut output = IncrementalOutput::default();
        let character = "你".as_bytes();
        output.append(&character[..2]);

        assert_eq!(output.take(true), (String::new(), 0));
        assert!(output.is_empty());

        output.append(&character[2..]);
        assert_eq!(output.take(true), ("你".into(), 0));
    }

    #[tokio::test]
    async fn call_owned_process_requires_invocation_cancellation() {
        let store = ProcessHandleStore::default();
        let owner = ExtensionInstanceId::new();
        let mut request = HostProcessStartRequest::new("unused");
        request.lifetime = HostProcessLifetime::Call;
        let error = store
            .start(request, None, "session-a", owner, None)
            .await
            .expect_err("call-owned process requires cancellation ownership");
        assert_eq!(error.code_enum(), Some(WireErrorCode::ContextUnavailable));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_handles_are_incremental_and_owner_scoped() {
        let workspace = tempdir().expect("workspace");
        let store = ProcessHandleStore::default();
        let owner = ExtensionInstanceId::new();
        let replacement_owner = ExtensionInstanceId::new();
        let mut request = HostProcessStartRequest::new("/bin/sh");
        request.args = vec![
            "-c".into(),
            "printf first; sleep 0.05; printf second".into(),
        ];
        let started = store
            .start(request, workspace.path().to_str(), "session-a", owner, None)
            .await
            .expect("start process");

        let denied = store
            .status(
                HostProcessTargetRequest {
                    id: started.id.clone(),
                },
                "session-b",
                owner,
            )
            .expect_err("another session must not see the handle");
        assert_eq!(denied.code_enum(), Some(WireErrorCode::InvalidInput));
        let denied = store
            .status(
                HostProcessTargetRequest {
                    id: started.id.clone(),
                },
                "session-a",
                replacement_owner,
            )
            .expect_err("a replacement generation must not see the old handle");
        assert_eq!(denied.code_enum(), Some(WireErrorCode::InvalidInput));

        let mut output = String::new();
        let mut running = true;
        let mut termination = None;
        for _ in 0..4 {
            let read = store
                .read(
                    HostProcessReadRequest {
                        id: started.id.clone(),
                        wait_ms: Some(1_000),
                    },
                    "session-a",
                    owner,
                    None,
                )
                .await
                .expect("read process output");
            output.push_str(&read.combined);
            running = read.state.is_running();
            termination = Some(read.state);
            if !running {
                break;
            }
        }

        assert_eq!(output, "firstsecond");
        assert!(!running);
        assert_eq!(
            termination,
            Some(HostProcessState::Exited { status: Some(0) })
        );

        let call_cancellation = CancellationToken::new();
        let mut promoted_request = HostProcessStartRequest::new("/bin/sh");
        promoted_request.args = vec!["-c".into(), "sleep 0.05; printf survived".into()];
        promoted_request.lifetime = HostProcessLifetime::Call;
        let promoted = store
            .start(
                promoted_request,
                workspace.path().to_str(),
                "session-a",
                owner,
                Some(&call_cancellation),
            )
            .await
            .expect("start call-owned process");
        store
            .promote(
                HostProcessTargetRequest {
                    id: promoted.id.clone(),
                },
                "session-a",
                owner,
            )
            .expect("promote process");
        call_cancellation.cancel();

        let mut promoted_output = String::new();
        let mut promoted_running = true;
        for _ in 0..4 {
            let read = store
                .read(
                    HostProcessReadRequest {
                        id: promoted.id.clone(),
                        wait_ms: Some(1_000),
                    },
                    "session-a",
                    owner,
                    None,
                )
                .await
                .expect("read promoted process");
            promoted_output.push_str(&read.combined);
            promoted_running = read.state.is_running();
            if !promoted_running {
                break;
            }
        }
        assert_eq!(promoted_output, "survived");
        assert!(!promoted_running);

        let mut stdin_request = HostProcessStartRequest::new("/bin/sh");
        stdin_request.args = vec!["-c".into(), "cat".into()];
        let stdin_process = store
            .start(
                stdin_request,
                workspace.path().to_str(),
                "session-a",
                owner,
                None,
            )
            .await
            .expect("start stdin process");
        store
            .input(
                HostProcessInputRequest::write(&stdin_process.id, "input through pipe"),
                "session-a",
                owner,
            )
            .await
            .expect("write stdin");
        store
            .input(
                HostProcessInputRequest::close(&stdin_process.id),
                "session-a",
                owner,
            )
            .await
            .expect("close stdin");
        let mut stdin_output = String::new();
        let mut stdin_running = true;
        for _ in 0..3 {
            let output = store
                .read(
                    HostProcessReadRequest {
                        id: stdin_process.id.clone(),
                        wait_ms: Some(1_000),
                    },
                    "session-a",
                    owner,
                    None,
                )
                .await
                .expect("read stdin process");
            stdin_output.push_str(&output.combined);
            stdin_running = output.state.is_running();
            if !stdin_running {
                break;
            }
        }
        assert_eq!(stdin_output, "input through pipe");
        assert!(!stdin_running);

        let owner_cancellation = CancellationToken::new();
        let mut cancelled_request = HostProcessStartRequest::new("/bin/sh");
        cancelled_request.args = vec!["-c".into(), "sleep 5".into()];
        cancelled_request.lifetime = HostProcessLifetime::Call;
        let cancelled = store
            .start(
                cancelled_request,
                workspace.path().to_str(),
                "session-a",
                owner,
                Some(&owner_cancellation),
            )
            .await
            .expect("start cancellable process");
        let read_cancellation = CancellationToken::new();
        read_cancellation.cancel();
        let error = store
            .read(
                HostProcessReadRequest {
                    id: cancelled.id,
                    wait_ms: Some(1_000),
                },
                "session-a",
                owner,
                Some(&read_cancellation),
            )
            .await
            .expect_err("cancelled read must stop its call-owned process");
        assert_eq!(error.code_enum(), Some(WireErrorCode::Cancelled));
        assert!(
            store.list("session-a", owner).processes.is_empty(),
            "cancelled call-owned process must release its retained handle"
        );

        let mut old_generation_request = HostProcessStartRequest::new("/bin/sh");
        old_generation_request.args = vec!["-c".into(), "sleep 5".into()];
        let old_generation = store
            .start(
                old_generation_request,
                workspace.path().to_str(),
                "session-a",
                owner,
                None,
            )
            .await
            .expect("start old-generation process");
        let mut replacement_request = HostProcessStartRequest::new("/bin/sh");
        replacement_request.args = vec!["-c".into(), "sleep 5".into()];
        let replacement = store
            .start(
                replacement_request,
                workspace.path().to_str(),
                "session-a",
                replacement_owner,
                None,
            )
            .await
            .expect("start replacement-generation process");
        store.cleanup_extension(owner);
        assert!(store.list("session-a", owner).processes.is_empty());
        assert_eq!(
            store.list("session-a", replacement_owner).processes,
            [HostProcessStatusOutput {
                id: replacement.id,
                state: HostProcessState::Running {},
            }]
        );
        assert!(
            store
                .status(
                    HostProcessTargetRequest {
                        id: old_generation.id,
                    },
                    "session-a",
                    owner,
                )
                .is_err()
        );
        store.cleanup_extension(replacement_owner);

        let mut timeout_request = HostProcessStartRequest::new("/bin/sh");
        timeout_request.args = vec!["-c".into(), "sleep 1 & printf parent-exited".into()];
        timeout_request.timeout_ms = Some(10);
        let timed_out = store
            .start(
                timeout_request,
                workspace.path().to_str(),
                "session-a",
                owner,
                None,
            )
            .await
            .expect("start timed process");
        let first_timeout_output = store
            .read(
                HostProcessReadRequest {
                    id: timed_out.id.clone(),
                    wait_ms: Some(3_000),
                },
                "session-a",
                owner,
                None,
            )
            .await
            .expect("read direct child output");
        let mut combined = first_timeout_output.combined;
        let state = if first_timeout_output.state.is_running() {
            let timeout_output = store
                .read(
                    HostProcessReadRequest {
                        id: timed_out.id,
                        wait_ms: Some(3_000),
                    },
                    "session-a",
                    owner,
                    None,
                )
                .await
                .expect("read timed process");
            combined.push_str(&timeout_output.combined);
            timeout_output.state
        } else {
            first_timeout_output.state
        };
        assert!(!state.is_running());
        assert!(matches!(state, HostProcessState::TimedOut { .. }));
        assert_eq!(combined, "parent-exited");
    }
}
