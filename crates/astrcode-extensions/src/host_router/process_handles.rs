//! Session-owned foreground/background/PTY process handles.

use std::{
    collections::HashMap,
    io::{Read, Write},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use astrcode_extension_sdk::{
    host::{
        HOST_PROCESS_MAX_WAIT_MS, HostProcessHandleOutput, HostProcessInputAction,
        HostProcessInputRequest, HostProcessIo, HostProcessLifetime, HostProcessListOutput,
        HostProcessReadOutput, HostProcessReadRequest, HostProcessResizeRequest,
        HostProcessStartRequest, HostProcessState, HostProcessStatusOutput,
        HostProcessTargetRequest,
    },
    s5r::ErrorPayload,
    wire::WireErrorCode,
};
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    sync::{Mutex as AsyncMutex, Notify, OwnedSemaphorePermit, Semaphore},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{
    process::{configure_process, configure_pty_process, resolve_cwd, validated_timeout},
    run_blocking_io,
};
use crate::process_supervision::{SupervisedChild, SupervisedCommand};

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

    fn take(&mut self) -> (String, usize) {
        let bytes = std::mem::take(&mut self.bytes);
        let dropped = std::mem::take(&mut self.dropped_bytes);
        (String::from_utf8_lossy(&bytes).into_owned(), dropped)
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
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

enum ProcessController {
    Pipes {
        stdin: Arc<AsyncMutex<Option<tokio::process::ChildStdin>>>,
        cancel: CancellationToken,
        task: Mutex<Option<JoinHandle<()>>>,
    },
    Pty(Arc<PtyController>),
}

enum ProcessLifetimeOwner {
    Call(CancellationToken),
    Session,
}

struct ProcessEntry {
    id: String,
    session_id: String,
    extension_id: String,
    io: HostProcessIo,
    output: Mutex<ProcessOutputState>,
    output_changed: Notify,
    requested_termination: Mutex<Option<ProcessTermination>>,
    lifetime_owner: Mutex<ProcessLifetimeOwner>,
    controller: ProcessController,
    _handle_permit: OwnedSemaphorePermit,
}

impl ProcessEntry {
    fn status(&self) -> HostProcessStatusOutput {
        if let ProcessController::Pty(controller) = &self.controller {
            controller.refresh_status(self);
        }
        let output = self.output.lock();
        HostProcessStatusOutput {
            id: self.id.clone(),
            io: self.io.clone(),
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
        if let ProcessController::Pty(controller) = &self.controller {
            controller.refresh_status(self);
        }
        let mut output = self.output.lock();
        let (stdout, stdout_dropped) = output.stdout.take();
        let (stderr, stderr_dropped) = output.stderr.take();
        let (combined, combined_dropped) = output.combined.take();
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
        match &self.controller {
            ProcessController::Pipes { cancel, .. } => cancel.cancel(),
            ProcessController::Pty(controller) => controller.kill(self),
        }
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

struct PtyController {
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    master: Mutex<Option<Box<dyn portable_pty::MasterPty + Send>>>,
    child: Mutex<Option<Box<dyn portable_pty::Child + Send + Sync>>>,
    reader: Mutex<Option<std::thread::JoinHandle<()>>>,
    timeout_cancel: CancellationToken,
    timeout_task: Mutex<Option<JoinHandle<()>>>,
}

impl PtyController {
    fn refresh_status(&self, entry: &ProcessEntry) {
        let status = {
            let mut child = self.child.lock();
            let status = child
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten())
                .map(|status| status.exit_code() as i32);
            if status.is_some() {
                child.take();
            }
            status
        };
        if status.is_some() {
            self.timeout_cancel.cancel();
            entry.finish(status, ProcessTermination::Exited);
        }
    }

    fn wait_for_exit(&self, entry: &ProcessEntry) {
        let status = self
            .child
            .lock()
            .take()
            .and_then(|mut child| match child.wait() {
                Ok(status) => Some(status),
                Err(error) => {
                    tracing::warn!(process_id = %entry.id, %error, "PTY wait failed");
                    None
                },
            })
            .map(|status| status.exit_code() as i32);
        self.timeout_cancel.cancel();
        entry.finish(status, ProcessTermination::Exited);
    }

    fn kill(&self, entry: &ProcessEntry) {
        self.timeout_cancel.cancel();
        if let Some(mut child) = self.child.lock().take() {
            if let Err(error) = child.kill() {
                tracing::warn!(process_id = %entry.id, %error, "PTY kill failed");
            }
            let status = match child.wait() {
                Ok(status) => Some(status.exit_code() as i32),
                Err(error) => {
                    tracing::warn!(process_id = %entry.id, %error, "PTY reap failed");
                    None
                },
            };
            let termination = entry
                .requested_termination
                .lock()
                .unwrap_or(ProcessTermination::Killed);
            entry.finish(status, termination);
        } else {
            let status = entry.output.lock().status;
            let termination = entry
                .requested_termination
                .lock()
                .unwrap_or(ProcessTermination::Killed);
            entry.finish(status, termination);
        }
        self.writer.lock().take();
        self.master.lock().take();
        if let Some(reader) = self.reader.lock().take()
            && let Err(error) = reader.join()
        {
            tracing::warn!(process_id = %entry.id, ?error, "PTY reader thread panicked");
        }
    }
}

impl Drop for PtyController {
    fn drop(&mut self) {
        self.timeout_cancel.cancel();
        if let Some(task) = self.timeout_task.get_mut().take() {
            task.abort();
        }
        if let Some(mut child) = self.child.get_mut().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.writer.get_mut().take();
        self.master.get_mut().take();
        self.reader.get_mut().take();
    }
}

struct ProcessStartScope {
    cwd: std::path::PathBuf,
    session_id: String,
    extension_id: String,
    process_permit: OwnedSemaphorePermit,
    handle_permit: OwnedSemaphorePermit,
    lifetime_owner: ProcessLifetimeOwner,
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
        extension_id: &str,
        call_cancellation: Option<&CancellationToken>,
    ) -> Result<HostProcessHandleOutput, ErrorPayload> {
        validate_start_request(&request)?;
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
        let scope = ProcessStartScope {
            cwd,
            session_id: session_id.to_owned(),
            extension_id: extension_id.to_owned(),
            process_permit,
            handle_permit,
            lifetime_owner,
        };
        match request.io.clone() {
            HostProcessIo::Pipes => self.start_pipes(request, scope).await,
            HostProcessIo::Pty { rows, cols } => self.start_pty(request, rows, cols, scope).await,
        }
    }

    async fn start_pipes(
        &self,
        request: HostProcessStartRequest,
        scope: ProcessStartScope,
    ) -> Result<HostProcessHandleOutput, ErrorPayload> {
        let ProcessStartScope {
            cwd,
            session_id,
            extension_id,
            process_permit,
            handle_permit,
            lifetime_owner,
        } = scope;
        let timeout = validated_timeout(request.timeout_ms)?;
        let mut command = tokio::process::Command::new(&request.command);
        command
            .args(&request.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process(&mut command);
        let mut child = SupervisedCommand::new(command)
            .spawn()
            .map_err(|error| ErrorPayload::new(WireErrorCode::SpawnFailed, error.to_string()))?;
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
            session_id,
            extension_id,
            io: HostProcessIo::Pipes,
            output: Mutex::new(ProcessOutputState {
                running: true,
                ..Default::default()
            }),
            output_changed: Notify::new(),
            requested_termination: Mutex::new(None),
            lifetime_owner: Mutex::new(lifetime_owner),
            controller: ProcessController::Pipes {
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
        let ProcessController::Pipes { task: owner, .. } = &entry.controller else {
            return Err(ErrorPayload::new(
                WireErrorCode::HostRuntimeFailed,
                "pipe process created with an invalid controller",
            ));
        };
        *owner.lock() = Some(task);
        Ok(HostProcessHandleOutput { id })
    }

    async fn start_pty(
        &self,
        request: HostProcessStartRequest,
        rows: u16,
        cols: u16,
        scope: ProcessStartScope,
    ) -> Result<HostProcessHandleOutput, ErrorPayload> {
        let ProcessStartScope {
            cwd,
            session_id,
            extension_id,
            process_permit,
            handle_permit,
            lifetime_owner,
        } = scope;
        let timeout = validated_timeout(request.timeout_ms)?;
        if rows == 0 || cols == 0 {
            return Err(ErrorPayload::new(
                WireErrorCode::InvalidInput,
                "PTY rows and cols must be greater than zero",
            ));
        }
        let command = request.command;
        let args = request.args;
        let parts = tokio::task::spawn_blocking(move || {
            let pair = NativePtySystem::default()
                .openpty(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(process_error)?;
            let mut process = CommandBuilder::new(&command);
            process.args(args);
            process.cwd(cwd);
            configure_pty_process(&mut process);
            let child = pair.slave.spawn_command(process).map_err(process_error)?;
            drop(pair.slave);
            let writer = pair.master.take_writer().map_err(process_error)?;
            let reader = pair.master.try_clone_reader().map_err(process_error)?;
            Ok::<_, ErrorPayload>((pair.master, writer, reader, child))
        })
        .await
        .map_err(|error| {
            ErrorPayload::new(
                WireErrorCode::HostRuntimeFailed,
                format!("PTY start task failed: {error}"),
            )
        })??;
        let (master, writer, mut reader, child) = parts;
        let timeout_cancel = CancellationToken::new();
        let controller = Arc::new(PtyController {
            writer: Mutex::new(Some(writer)),
            master: Mutex::new(Some(master)),
            child: Mutex::new(Some(child)),
            reader: Mutex::new(None),
            timeout_cancel: timeout_cancel.clone(),
            timeout_task: Mutex::new(None),
        });
        let id = format!("process-{}", uuid::Uuid::new_v4());
        let entry = Arc::new(ProcessEntry {
            id: id.clone(),
            session_id,
            extension_id,
            io: HostProcessIo::Pty { rows, cols },
            output: Mutex::new(ProcessOutputState {
                running: true,
                ..Default::default()
            }),
            output_changed: Notify::new(),
            requested_termination: Mutex::new(None),
            lifetime_owner: Mutex::new(lifetime_owner),
            controller: ProcessController::Pty(Arc::clone(&controller)),
            _handle_permit: handle_permit,
        });
        self.entries.lock().insert(id.clone(), Arc::clone(&entry));

        let reader_entry = Arc::clone(&entry);
        let reader_controller = Arc::clone(&controller);
        let reader_timeout_cancel = timeout_cancel.clone();
        let reader_handle = std::thread::Builder::new()
            .name(format!("astrcode-process-reader-{id}"))
            .spawn(move || {
                let _permit = process_permit;
                let mut buffer = [0_u8; 4096];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => reader_entry.append_stdout(&buffer[..read]),
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => {
                            tracing::warn!(process_id = %reader_entry.id, %error, "PTY read failed");
                            break;
                        },
                    }
                }
                reader_controller.wait_for_exit(&reader_entry);
                reader_timeout_cancel.cancel();
            })
            .map_err(|error| {
                self.entries.lock().remove(&id);
                entry.request_termination(ProcessTermination::Killed);
                controller.kill(&entry);
                ErrorPayload::new(WireErrorCode::HostRuntimeFailed, error.to_string())
            })?;
        *controller.reader.lock() = Some(reader_handle);

        let timeout_entry = Arc::downgrade(&entry);
        let timeout_controller = Arc::downgrade(&controller);
        let call_entry = Arc::clone(&entry);
        let timeout_task = tokio::spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(timeout) => {
                    let Some(entry) = timeout_entry.upgrade() else {
                        return;
                    };
                    let Some(controller) = timeout_controller.upgrade() else {
                        return;
                    };
                    entry.request_termination(ProcessTermination::TimedOut);
                    if let Err(error) = tokio::task::spawn_blocking(move || controller.kill(&entry)).await {
                        tracing::warn!(%error, "PTY timeout termination task failed");
                    }
                },
                () = call_owner_cancelled(call_entry) => {
                    let Some(entry) = timeout_entry.upgrade() else {
                        return;
                    };
                    let Some(controller) = timeout_controller.upgrade() else {
                        return;
                    };
                    entry.request_termination(ProcessTermination::Cancelled);
                    if let Err(error) = tokio::task::spawn_blocking(move || controller.kill(&entry)).await {
                        tracing::warn!(%error, "PTY cancellation task failed");
                    }
                },
                () = timeout_cancel.cancelled() => {},
            }
        });
        *controller.timeout_task.lock() = Some(timeout_task);
        Ok(HostProcessHandleOutput { id })
    }

    pub(super) async fn read(
        &self,
        request: HostProcessReadRequest,
        session_id: &str,
        extension_id: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<HostProcessReadOutput, ErrorPayload> {
        let wait_ms = request.wait_ms.unwrap_or(0);
        if wait_ms > HOST_PROCESS_MAX_WAIT_MS {
            return Err(ErrorPayload::new(
                WireErrorCode::InvalidInput,
                format!("wait_ms must not exceed {HOST_PROCESS_MAX_WAIT_MS}"),
            ));
        }
        let entry = self.owned_entry(&request.id, session_id, extension_id)?;
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
                                self.cancel_entry(
                                    Arc::clone(&entry),
                                    ProcessTermination::Cancelled,
                                )
                                .await?;
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
        extension_id: &str,
    ) -> Result<(), ErrorPayload> {
        let entry = self.owned_entry(&request.id, session_id, extension_id)?;
        match (&entry.controller, request.action) {
            (ProcessController::Pipes { stdin, .. }, HostProcessInputAction::Write { input }) => {
                let mut stdin = stdin.lock().await;
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
            (ProcessController::Pipes { stdin, .. }, HostProcessInputAction::Close) => {
                stdin.lock().await.take();
                Ok(())
            },
            (ProcessController::Pty(controller), HostProcessInputAction::Write { input }) => {
                let controller = Arc::clone(controller);
                let id = request.id;
                run_blocking_io(move || {
                    let mut writer = controller.writer.lock();
                    let writer = writer.as_mut().ok_or_else(|| process_not_running(&id))?;
                    writer.write_all(input.as_bytes()).map_err(process_error)?;
                    writer.flush().map_err(process_error)
                })
                .await
            },
            (ProcessController::Pty(_), HostProcessInputAction::Close) => Err(ErrorPayload::new(
                WireErrorCode::InvalidInput,
                "PTY process handles do not have a closable stdin pipe",
            )),
        }
    }

    pub(super) async fn resize(
        &self,
        request: HostProcessResizeRequest,
        session_id: &str,
        extension_id: &str,
    ) -> Result<(), ErrorPayload> {
        if request.rows == 0 || request.cols == 0 {
            return Err(ErrorPayload::new(
                WireErrorCode::InvalidInput,
                "PTY rows and cols must be greater than zero",
            ));
        }
        let entry = self.owned_entry(&request.id, session_id, extension_id)?;
        let ProcessController::Pty(controller) = &entry.controller else {
            return Err(ErrorPayload::new(
                WireErrorCode::InvalidInput,
                "only PTY process handles can be resized",
            ));
        };
        let controller = Arc::clone(controller);
        let id = request.id;
        run_blocking_io(move || {
            controller
                .master
                .lock()
                .as_mut()
                .ok_or_else(|| process_not_running(&id))?
                .resize(PtySize {
                    rows: request.rows,
                    cols: request.cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(process_error)
        })
        .await
    }

    pub(super) fn status(
        &self,
        request: HostProcessTargetRequest,
        session_id: &str,
        extension_id: &str,
    ) -> Result<HostProcessStatusOutput, ErrorPayload> {
        Ok(self
            .owned_entry(&request.id, session_id, extension_id)?
            .status())
    }

    pub(super) fn promote(
        &self,
        request: HostProcessTargetRequest,
        session_id: &str,
        extension_id: &str,
    ) -> Result<(), ErrorPayload> {
        self.owned_entry(&request.id, session_id, extension_id)?
            .promote()
    }

    pub(super) async fn kill(
        &self,
        request: HostProcessTargetRequest,
        session_id: &str,
        extension_id: &str,
    ) -> Result<(), ErrorPayload> {
        let entry = self.remove_owned_entry(&request.id, session_id, extension_id)?;
        self.cancel_entry(entry, ProcessTermination::Killed).await
    }

    async fn cancel_entry(
        &self,
        entry: Arc<ProcessEntry>,
        termination: ProcessTermination,
    ) -> Result<(), ErrorPayload> {
        entry.request_termination(termination);
        let pty = match &entry.controller {
            ProcessController::Pty(controller) => Some(Arc::clone(controller)),
            ProcessController::Pipes { .. } => None,
        };
        match pty {
            Some(controller) => {
                run_blocking_io(move || {
                    controller.kill(&entry);
                    Ok(())
                })
                .await
            },
            None => {
                entry.cancel(termination);
                Ok(())
            },
        }
    }

    pub(super) fn list(&self, session_id: &str, extension_id: &str) -> HostProcessListOutput {
        let mut processes = self
            .entries
            .lock()
            .values()
            .filter(|entry| entry.session_id == session_id && entry.extension_id == extension_id)
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

    pub(super) fn cleanup_extension(&self, extension_id: &str) {
        let entries = remove_matching(&self.entries, |entry| entry.extension_id == extension_id);
        for entry in entries {
            entry.cancel(ProcessTermination::Killed);
        }
    }

    fn owned_entry(
        &self,
        id: &str,
        session_id: &str,
        extension_id: &str,
    ) -> Result<Arc<ProcessEntry>, ErrorPayload> {
        self.entries
            .lock()
            .get(id)
            .filter(|entry| entry.session_id == session_id && entry.extension_id == extension_id)
            .cloned()
            .ok_or_else(|| unknown_process(id))
    }

    fn remove_owned_entry(
        &self,
        id: &str,
        session_id: &str,
        extension_id: &str,
    ) -> Result<Arc<ProcessEntry>, ErrorPayload> {
        let mut entries = self.entries.lock();
        let owned = entries.get(id).is_some_and(|entry| {
            entry.session_id == session_id && entry.extension_id == extension_id
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
    let stdout_reader = pump_output(stdout, Arc::clone(&entry), false);
    let stderr_reader = pump_output(stderr, Arc::clone(&entry), true);
    let wait = async {
        tokio::select! {
            status = child.wait() => (
                status.ok().and_then(|status| status.code()),
                ProcessTermination::Exited,
            ),
            () = cancellation.cancelled() => {
                let termination = entry
                    .requested_termination
                    .lock()
                    .unwrap_or(ProcessTermination::Killed);
                (
                    child.terminate(TERMINATION_GRACE).await.ok().and_then(|status| status.code()),
                    termination,
                )
            },
            () = call_owner_cancelled(Arc::clone(&entry)) => {
                (
                    child.terminate(TERMINATION_GRACE).await.ok().and_then(|status| status.code()),
                    ProcessTermination::Cancelled,
                )
            },
            () = tokio::time::sleep(timeout) => {
                (
                    child.terminate(TERMINATION_GRACE).await.ok().and_then(|status| status.code()),
                    ProcessTermination::TimedOut,
                )
            },
        }
    };
    let (_, _, (status, termination)) = tokio::join!(stdout_reader, stderr_reader, wait);
    stdin.lock().await.take();
    entry.finish(status, termination);
}

async fn call_owner_cancelled(entry: Arc<ProcessEntry>) {
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

fn validate_start_request(request: &HostProcessStartRequest) -> Result<(), ErrorPayload> {
    if request.command.trim().is_empty() {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
            "command must not be empty",
        ));
    }
    validated_timeout(request.timeout_ms)?;
    Ok(())
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

fn process_error(error: impl std::fmt::Display) -> ErrorPayload {
    ErrorPayload::new(WireErrorCode::ProcessFailed, error.to_string())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn call_owned_process_requires_invocation_cancellation() {
        let store = ProcessHandleStore::default();
        let mut request = HostProcessStartRequest::pipes("unused");
        request.lifetime = HostProcessLifetime::Call;
        let error = store
            .start(request, None, "session-a", "extension-a", None)
            .await
            .expect_err("call-owned process requires cancellation ownership");
        assert_eq!(error.code_enum(), Some(WireErrorCode::ContextUnavailable));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_handles_are_incremental_and_owner_scoped() {
        let workspace = tempdir().expect("workspace");
        let store = ProcessHandleStore::default();
        let mut request = HostProcessStartRequest::pipes("/bin/sh");
        request.args = vec![
            "-c".into(),
            "printf first; sleep 0.05; printf second".into(),
        ];
        let started = store
            .start(
                request,
                workspace.path().to_str(),
                "session-a",
                "extension-a",
                None,
            )
            .await
            .expect("start process");

        let denied = store
            .status(
                HostProcessTargetRequest {
                    id: started.id.clone(),
                },
                "session-b",
                "extension-a",
            )
            .expect_err("another session must not see the handle");
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
                    "extension-a",
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
        let mut promoted_request = HostProcessStartRequest::pipes("/bin/sh");
        promoted_request.args = vec!["-c".into(), "sleep 0.05; printf survived".into()];
        promoted_request.lifetime = HostProcessLifetime::Call;
        let promoted = store
            .start(
                promoted_request,
                workspace.path().to_str(),
                "session-a",
                "extension-a",
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
                "extension-a",
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
                    "extension-a",
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

        let mut stdin_request = HostProcessStartRequest::pipes("/bin/sh");
        stdin_request.args = vec!["-c".into(), "cat".into()];
        let stdin_process = store
            .start(
                stdin_request,
                workspace.path().to_str(),
                "session-a",
                "extension-a",
                None,
            )
            .await
            .expect("start stdin process");
        store
            .input(
                HostProcessInputRequest::write(&stdin_process.id, "input through pipe"),
                "session-a",
                "extension-a",
            )
            .await
            .expect("write stdin");
        store
            .input(
                HostProcessInputRequest::close(&stdin_process.id),
                "session-a",
                "extension-a",
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
                    "extension-a",
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
        let mut cancelled_request = HostProcessStartRequest::pipes("/bin/sh");
        cancelled_request.args = vec!["-c".into(), "sleep 5".into()];
        cancelled_request.lifetime = HostProcessLifetime::Call;
        let cancelled = store
            .start(
                cancelled_request,
                workspace.path().to_str(),
                "session-a",
                "extension-a",
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
                "extension-a",
                Some(&read_cancellation),
            )
            .await
            .expect_err("cancelled read must stop its call-owned process");
        assert_eq!(error.code_enum(), Some(WireErrorCode::Cancelled));
        assert!(
            store.list("session-a", "extension-a").processes.is_empty(),
            "cancelled call-owned process must release its retained handle"
        );

        let mut timeout_request = HostProcessStartRequest::pipes("/bin/sh");
        timeout_request.args = vec!["-c".into(), "sleep 1".into()];
        timeout_request.timeout_ms = Some(10);
        let timed_out = store
            .start(
                timeout_request,
                workspace.path().to_str(),
                "session-a",
                "extension-a",
                None,
            )
            .await
            .expect("start timed process");
        let timeout_output = store
            .read(
                HostProcessReadRequest {
                    id: timed_out.id,
                    wait_ms: Some(3_000),
                },
                "session-a",
                "extension-a",
                None,
            )
            .await
            .expect("read timed process");
        assert!(!timeout_output.state.is_running());
        assert!(matches!(
            timeout_output.state,
            HostProcessState::TimedOut { .. }
        ));
    }
}
