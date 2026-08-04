//! s5r Peer 状态机（stdio 帧 + WireMessage）。

use std::{
    collections::HashMap,
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use futures_util::FutureExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex as AsyncMutex, Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    runtime::{cancel::CancelToken, stream::EventStream, transport::FrameTransport},
    s5r::{
        CAP_HANDLER_INVOKE, CancelMsg, ErrorPayload, EventMsg, EventPhase, InitializeMsg,
        InitializeOutput, InvokeMsg, PeerInfo, ResultKind, ResultMsg, S5R_VERSION, WIRE_CODEC_JSON,
        WIRE_CODEC_METADATA_KEY, WireMessage, encode_wire_message, parse_wire_message,
    },
};

const DEFAULT_INVOKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const DEFAULT_STREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const TASK_ABORT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
// Session admission exposes at most eight handler lanes. The ninth worker preserves one
// progress lane for nested/reentrant work while all primary lanes are occupied.
const INBOUND_WORKER_COUNT: usize = 9;
const INBOUND_WORK_QUEUE_CAPACITY: usize = 64;
const STREAM_EVENT_BUFFER_CAPACITY: usize = 64;
const OUTBOUND_INVOKE_LIMIT: usize = 64;
const REJECTION_QUEUE_CAPACITY: usize = 32;
const CANCEL_QUEUE_CAPACITY: usize = 64;

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

type PendingResultTx = oneshot::Sender<Result<ResultMsg, PeerError>>;
type PendingResults = HashMap<String, PendingResultTx>;
type PendingStreamEvents = HashMap<String, mpsc::Sender<EventMsg>>;

struct InboundCancelEntry {
    registration_id: u64,
    token: CancelToken,
}

enum InboundWork {
    Initialize(InitializeMsg),
    Invoke {
        message: InvokeMsg,
        token: CancelToken,
        registration: InboundCancelGuard,
    },
}

#[derive(Clone)]
struct InboundInvokeScope {
    id: String,
    cancellation: CancellationToken,
}

tokio::task_local! {
    static INBOUND_INVOKE_SCOPE: InboundInvokeScope;
}

pub type InitializeHandler =
    Arc<dyn Fn(InitializeMsg) -> BoxFuture<Result<InitializeOutput, ErrorPayload>> + Send + Sync>;

pub type InvokeHandler = Arc<
    dyn Fn(InvokeMsg, CancelToken) -> BoxFuture<Result<InvokeReply, ErrorPayload>> + Send + Sync,
>;

/// 观察一个出站 invoke 从发布到完成的生命周期。
pub trait OutboundInvokeTracker: Send + Sync {
    fn started(&self, request_id: &str);
    fn finished(&self, request_id: &str);
}

/// 出站 invoke 的可选控制（取消联动、父调用与 in-flight 跟踪）。
#[derive(Clone, Default)]
pub struct OutboundInvokeControl {
    pub external_cancel: Option<CancellationToken>,
    /// 显式父调用。缺省时自动继承当前入站 handler 的 request id。
    pub parent_invoke_id: Option<String>,
    pub tracker: Option<Arc<dyn OutboundInvokeTracker>>,
}

/// handler 对入站 invoke 的响应。
pub enum InvokeReply {
    Value(Value),
    Events(Vec<EventMsg>),
}

#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    #[error("{0}")]
    Msg(String),
    #[error("peer closed")]
    Closed,
    #[error("request timed out")]
    Timeout,
    #[error("peer outbound request limit reached")]
    Busy,
    #[error("payload error: {0}")]
    Payload(String),
}

pub struct Peer<T: FrameTransport + 'static> {
    transport: Arc<T>,
    peer_info: PeerInfo,
    protocol_version: String,
    next_id: AtomicU64,
    closed: AtomicBool,
    close_complete: AtomicBool,
    closed_notify: Notify,
    close_request: CancellationToken,
    close_lock: AsyncMutex<()>,
    remote_initialized: Arc<AtomicBool>,
    pending_results: Arc<Mutex<PendingResults>>,
    pending_stream_events: Arc<Mutex<PendingStreamEvents>>,
    initialize_handler: Mutex<Option<InitializeHandler>>,
    invoke_handler: Mutex<Option<InvokeHandler>>,
    read_task: Mutex<Option<JoinHandle<()>>>,
    inbound_work_tx: Mutex<Option<mpsc::Sender<InboundWork>>>,
    inbound_workers: Mutex<Vec<JoinHandle<()>>>,
    next_inbound_registration_id: AtomicU64,
    inbound_cancel: Arc<Mutex<HashMap<String, InboundCancelEntry>>>,
    outbound_invoke_permits: Arc<Semaphore>,
    rejection_tx: mpsc::Sender<WorkRejection>,
    cancel_tx: mpsc::Sender<CancelWrite>,
    writer_rxs: Mutex<Option<(mpsc::Receiver<WorkRejection>, mpsc::Receiver<CancelWrite>)>>,
    writer_tasks: Mutex<Vec<JoinHandle<()>>>,
    writers_started: AtomicBool,
}

struct OutboundTrackingGuard {
    id: String,
    tracker: Option<Arc<dyn OutboundInvokeTracker>>,
}

impl OutboundTrackingGuard {
    fn new(id: &str, control: &OutboundInvokeControl) -> Self {
        if let Some(tracker) = &control.tracker {
            tracker.started(id);
        }
        Self {
            id: id.to_string(),
            tracker: control.tracker.clone(),
        }
    }
}

impl Drop for OutboundTrackingGuard {
    fn drop(&mut self) {
        if let Some(tracker) = &self.tracker {
            tracker.finished(&self.id);
        }
    }
}

/// 出站 invoke 的公共准备产物；guard 仅用于持有 RAII 语义直到调用结束。
struct OutboundRequestPrep {
    permit: OwnedSemaphorePermit,
    id: String,
    parent_invoke_id: Option<String>,
    tracking: OutboundTrackingGuard,
    cancel_watch: AbortTaskOnDrop,
}

struct PendingResultGuard<T: FrameTransport + 'static> {
    peer: Arc<Peer<T>>,
    id: String,
    sent: bool,
    completed: bool,
    cancel_on_drop: bool,
    cancel_reason: &'static str,
}

impl<T: FrameTransport + 'static> PendingResultGuard<T> {
    fn new(peer: Arc<Peer<T>>, id: String, cancel_on_drop: bool) -> Self {
        Self {
            peer,
            id,
            sent: false,
            completed: false,
            cancel_on_drop,
            cancel_reason: "caller_dropped",
        }
    }

    fn mark_sent(&mut self) {
        self.sent = true;
    }

    fn complete(&mut self) {
        self.completed = true;
    }

    fn cancel_as(&mut self, reason: &'static str) {
        self.cancel_reason = reason;
    }
}

impl<T: FrameTransport + 'static> Drop for PendingResultGuard<T> {
    fn drop(&mut self) {
        self.peer.pending_results.lock().remove(&self.id);
        if self.sent && !self.completed && self.cancel_on_drop {
            schedule_outbound_cancel(Arc::clone(&self.peer), self.id.clone(), self.cancel_reason);
        }
    }
}

struct PendingStreamInsertGuard {
    pending: Arc<Mutex<PendingStreamEvents>>,
    id: String,
    armed: bool,
}

impl PendingStreamInsertGuard {
    fn new(pending: Arc<Mutex<PendingStreamEvents>>, id: String) -> Self {
        Self {
            pending,
            id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingStreamInsertGuard {
    fn drop(&mut self) {
        if self.armed {
            self.pending.lock().remove(&self.id);
        }
    }
}

struct InboundCancelGuard {
    pending: Arc<Mutex<HashMap<String, InboundCancelEntry>>>,
    id: String,
    registration_id: u64,
}

impl Drop for InboundCancelGuard {
    fn drop(&mut self) {
        let mut pending = self.pending.lock();
        if pending
            .get(&self.id)
            .is_some_and(|entry| entry.registration_id == self.registration_id)
        {
            pending.remove(&self.id);
        }
    }
}

struct AbortTaskOnDrop(Option<JoinHandle<()>>);

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

#[derive(Clone)]
struct WorkFailure {
    id: String,
    kind: ResultKind,
    stream: bool,
}

struct WorkRejection {
    failure: WorkFailure,
    error: ErrorPayload,
}

struct CancelWrite {
    cancel: CancelMsg,
    completion: Option<oneshot::Sender<()>>,
}

impl<T: FrameTransport + 'static> Peer<T> {
    pub fn new(transport: T, peer_info: PeerInfo) -> Arc<Self> {
        let (rejection_tx, rejection_rx) = mpsc::channel(REJECTION_QUEUE_CAPACITY);
        let (cancel_tx, cancel_rx) = mpsc::channel(CANCEL_QUEUE_CAPACITY);
        Arc::new(Self {
            transport: Arc::new(transport),
            peer_info,
            protocol_version: S5R_VERSION.to_string(),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            close_complete: AtomicBool::new(false),
            closed_notify: Notify::new(),
            close_request: CancellationToken::new(),
            close_lock: AsyncMutex::new(()),
            remote_initialized: Arc::new(AtomicBool::new(false)),
            pending_results: Arc::new(Mutex::new(HashMap::new())),
            pending_stream_events: Arc::new(Mutex::new(HashMap::new())),
            initialize_handler: Mutex::new(None),
            invoke_handler: Mutex::new(None),
            read_task: Mutex::new(None),
            inbound_work_tx: Mutex::new(None),
            inbound_workers: Mutex::new(Vec::new()),
            next_inbound_registration_id: AtomicU64::new(1),
            inbound_cancel: Arc::new(Mutex::new(HashMap::new())),
            outbound_invoke_permits: Arc::new(Semaphore::new(OUTBOUND_INVOKE_LIMIT)),
            rejection_tx,
            cancel_tx,
            writer_rxs: Mutex::new(Some((rejection_rx, cancel_rx))),
            writer_tasks: Mutex::new(Vec::new()),
            writers_started: AtomicBool::new(false),
        })
    }

    pub fn set_initialize_handler(self: &Arc<Self>, handler: InitializeHandler) {
        *self.initialize_handler.lock() = Some(handler);
    }

    pub fn set_invoke_handler(self: &Arc<Self>, handler: InvokeHandler) {
        *self.invoke_handler.lock() = Some(handler);
    }

    pub async fn start(self: &Arc<Self>) -> Result<(), PeerError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(PeerError::Closed);
        }
        if self.read_task.lock().is_some() {
            return Err(PeerError::Msg("peer already started".into()));
        }
        let (rejection_rx, cancel_rx) = self
            .writer_rxs
            .lock()
            .take()
            .ok_or_else(|| PeerError::Msg("peer writers already started".into()))?;
        let rejection_peer = Arc::clone(self);
        let cancel_peer = Arc::clone(self);
        *self.writer_tasks.lock() = vec![
            crate::runtime::task_utils::spawn_traced("peer_rejection_writer", async move {
                rejection_peer.run_rejection_writer(rejection_rx).await;
            }),
            crate::runtime::task_utils::spawn_traced("peer_cancel_writer", async move {
                cancel_peer.run_cancel_writer(cancel_rx).await;
            }),
        ];
        self.writers_started.store(true, Ordering::Release);

        let (work_tx, work_rx) = mpsc::channel(INBOUND_WORK_QUEUE_CAPACITY);
        let work_rx = Arc::new(AsyncMutex::new(work_rx));
        let mut workers = Vec::with_capacity(INBOUND_WORKER_COUNT);
        for _ in 0..INBOUND_WORKER_COUNT {
            let peer = Arc::clone(self);
            let work_rx = Arc::clone(&work_rx);
            workers.push(crate::runtime::task_utils::spawn_traced(
                "peer_inbound_worker",
                async move {
                    peer.run_inbound_worker(work_rx).await;
                },
            ));
        }
        *self.inbound_workers.lock() = workers;
        *self.inbound_work_tx.lock() = Some(work_tx.clone());
        let peer = Arc::clone(self);
        let task = crate::runtime::task_utils::spawn_traced("peer_read_loop", async move {
            peer.read_loop(work_tx).await;
        });
        *self.read_task.lock() = Some(task);
        Ok(())
    }

    pub async fn stop(self: &Arc<Self>) {
        self.begin_close();
        self.close_runtime("peer_stopped").await;
        let read_task = self.read_task.lock().take();
        if let Some(mut task) = read_task {
            match tokio::time::timeout(TASK_ABORT_TIMEOUT, &mut task).await {
                Ok(Ok(())) => {},
                Ok(Err(error)) if error.is_cancelled() => {},
                Ok(Err(error)) => {
                    tracing::warn!(%error, "peer read task failed during cleanup");
                },
                Err(_) => {
                    tracing::warn!("peer read task did not stop after cleanup; aborting");
                    task.abort();
                },
            }
        }
    }

    pub fn is_remote_initialized(&self) -> bool {
        self.remote_initialized.load(Ordering::SeqCst)
    }

    pub async fn wait_closed(&self) {
        loop {
            let notified = self.closed_notify.notified();
            if self.close_complete.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    pub async fn wait_remote_initialized(
        self: &Arc<Self>,
        timeout: std::time::Duration,
    ) -> Result<(), PeerError> {
        let start = std::time::Instant::now();
        while !self.is_remote_initialized() {
            if self.closed.load(Ordering::SeqCst) {
                return Err(PeerError::Closed);
            }
            if start.elapsed() > timeout {
                return Err(PeerError::Timeout);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(())
    }

    fn next_id(&self) -> String {
        format!("req-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    async fn send_message(&self, msg: &WireMessage) -> Result<(), PeerError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(PeerError::Closed);
        }
        let payload = encode_wire_message(msg).map_err(PeerError::Msg)?;
        match tokio::time::timeout(WRITE_TIMEOUT, self.transport.write_frame(&payload)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                self.begin_close();
                Err(PeerError::Msg(format!("write frame: {error}")))
            },
            Err(_) => {
                self.begin_close();
                Err(PeerError::Msg(format!(
                    "write frame timed out after {} ms",
                    WRITE_TIMEOUT.as_millis()
                )))
            },
        }
    }

    async fn request_result(
        self: &Arc<Self>,
        id: String,
        msg: WireMessage,
        timeout: std::time::Duration,
    ) -> Result<ResultMsg, PeerError> {
        let cancel_on_drop = matches!(&msg, WireMessage::Invoke(_));
        let (tx, rx) = oneshot::channel();
        self.pending_results.lock().insert(id.clone(), tx);
        let mut pending = PendingResultGuard::new(Arc::clone(self), id, cancel_on_drop);
        self.send_message(&msg).await?;
        pending.mark_sent();
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => {
                pending.complete();
                result
            },
            Ok(Err(_)) => Err(PeerError::Closed),
            Err(_) => {
                pending.cancel_as("request_timeout");
                Err(PeerError::Timeout)
            },
        }
    }

    pub async fn initialize(
        self: &Arc<Self>,
        handlers: Vec<crate::s5r::HandlerDescriptor>,
        metadata: Value,
    ) -> Result<InitializeOutput, PeerError> {
        let id = self.next_id();
        let mut meta = metadata;
        if let Some(obj) = meta.as_object_mut() {
            obj.insert(WIRE_CODEC_METADATA_KEY.to_string(), json!(WIRE_CODEC_JSON));
        }
        let wire_id = id.clone();
        let msg = WireMessage::Initialize(InitializeMsg {
            id: wire_id,
            protocol_version: self.protocol_version.clone(),
            peer: self.peer_info.clone(),
            handlers,
            provided_capabilities: Vec::new(),
            metadata: meta,
        });
        let result = self.request_result(id, msg, DEFAULT_INVOKE_TIMEOUT).await?;
        if !result.success {
            return Err(PeerError::Payload(
                result
                    .error
                    .map(|e| e.message)
                    .unwrap_or_else(|| "initialize failed".into()),
            ));
        }
        let output: InitializeOutput = serde_json::from_value(result.output.unwrap_or(Value::Null))
            .map_err(|e| PeerError::Msg(format!("parse InitializeOutput: {e}")))?;
        self.remote_initialized.store(true, Ordering::SeqCst);
        Ok(output)
    }

    pub async fn invoke(
        self: &Arc<Self>,
        capability: &str,
        input: Value,
        caller_extension_id: Option<&str>,
        control: OutboundInvokeControl,
    ) -> Result<Value, PeerError> {
        let OutboundRequestPrep {
            permit: _permit,
            id,
            parent_invoke_id,
            tracking: _tracking,
            cancel_watch: _cancel_watch,
        } = self.prepare_outbound_invoke(&control)?;
        let msg = WireMessage::Invoke(InvokeMsg {
            id: id.clone(),
            capability: capability.to_string(),
            input,
            stream: false,
            caller_extension_id: caller_extension_id.map(str::to_string),
            parent_invoke_id,
        });
        let result = self.request_result(id, msg, DEFAULT_INVOKE_TIMEOUT).await;
        let result = result?;
        if !result.success {
            return Err(PeerError::Payload(
                result
                    .error
                    .map(|e| e.message)
                    .unwrap_or_else(|| "invoke failed".into()),
            ));
        }
        Ok(result.output.unwrap_or(Value::Null))
    }

    async fn begin_invoke_stream(
        self: &Arc<Self>,
        capability: &str,
        input: Value,
        caller_extension_id: Option<&str>,
        control: OutboundInvokeControl,
    ) -> Result<EventStream, PeerError> {
        let OutboundRequestPrep {
            permit,
            id,
            parent_invoke_id,
            tracking,
            cancel_watch,
        } = self.prepare_outbound_invoke(&control)?;
        let wire_id = id.clone();
        let (tx, rx) = mpsc::channel(STREAM_EVENT_BUFFER_CAPACITY);
        self.pending_stream_events
            .lock()
            .insert(wire_id.clone(), tx);
        let mut insert_guard =
            PendingStreamInsertGuard::new(Arc::clone(&self.pending_stream_events), wire_id.clone());
        let msg = WireMessage::Invoke(InvokeMsg {
            id,
            capability: capability.to_string(),
            input,
            stream: true,
            caller_extension_id: caller_extension_id.map(str::to_string),
            parent_invoke_id,
        });
        self.send_message(&msg).await?;
        insert_guard.disarm();

        let peer = Arc::clone(self);
        let cleanup_id = wire_id;
        Ok(EventStream::new(
            rx,
            Box::new(move |completed| {
                drop(permit);
                peer.pending_stream_events.lock().remove(&cleanup_id);
                drop(cancel_watch);
                drop(tracking);
                if !completed {
                    schedule_outbound_cancel(peer, cleanup_id, "stream_dropped");
                }
            }),
        ))
    }

    pub async fn invoke_stream(
        self: &Arc<Self>,
        capability: &str,
        input: Value,
        caller_extension_id: Option<&str>,
    ) -> Result<EventStream, PeerError> {
        self.invoke_stream_with_control(
            capability,
            input,
            caller_extension_id,
            OutboundInvokeControl::default(),
        )
        .await
    }

    pub async fn invoke_stream_with_control(
        self: &Arc<Self>,
        capability: &str,
        input: Value,
        caller_extension_id: Option<&str>,
        control: OutboundInvokeControl,
    ) -> Result<EventStream, PeerError> {
        self.begin_invoke_stream(capability, input, caller_extension_id, control)
            .await
    }

    pub async fn invoke_stream_collect(
        self: &Arc<Self>,
        capability: &str,
        input: Value,
        caller_extension_id: Option<&str>,
    ) -> Result<Value, PeerError> {
        let mut stream = self
            .begin_invoke_stream(
                capability,
                input,
                caller_extension_id,
                OutboundInvokeControl::default(),
            )
            .await?;
        let mut last_output = Value::Null;
        let deadline = tokio::time::Instant::now() + DEFAULT_STREAM_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let event = tokio::time::timeout(remaining, stream.next_event())
                .await
                .map_err(|_| PeerError::Timeout)?
                .ok_or(PeerError::Closed)?;
            match event.phase {
                EventPhase::Completed => {
                    if !event.output.is_null() {
                        last_output = event.output;
                    }
                    return Ok(last_output);
                },
                EventPhase::Failed => {
                    return Err(PeerError::Payload(
                        event
                            .error
                            .map(|e| e.message)
                            .unwrap_or_else(|| "stream failed".into()),
                    ));
                },
                EventPhase::Delta => {
                    if !event.data.is_null() {
                        last_output = event.data;
                    }
                },
                EventPhase::Started => {},
            }
        }
    }

    pub async fn cancel_outbound(self: &Arc<Self>, request_id: &str, reason: &str) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let cancel = CancelMsg {
            id: request_id.to_string(),
            reason: reason.to_string(),
        };
        if !self.writers_started.load(Ordering::Acquire) {
            self.queue_outbound_cancel(cancel);
            return;
        }
        let (completion, completed) = oneshot::channel();
        if self
            .cancel_tx
            .send(CancelWrite {
                cancel,
                completion: Some(completion),
            })
            .await
            .is_ok()
        {
            let _ = completed.await;
        }
    }

    fn prepare_outbound_invoke(
        self: &Arc<Self>,
        control: &OutboundInvokeControl,
    ) -> Result<OutboundRequestPrep, PeerError> {
        let permit = self.acquire_outbound_permit()?;
        let id = self.next_id();
        let inherited_scope = current_inbound_invoke_scope();
        let parent_invoke_id = control
            .parent_invoke_id
            .clone()
            .or_else(|| inherited_scope.as_ref().map(|scope| scope.id.clone()));
        let tracking = OutboundTrackingGuard::new(&id, control);
        let cancel_watch = spawn_cancel_watch(
            Arc::clone(self),
            id.clone(),
            control.external_cancel.clone(),
            inherited_scope.map(|scope| scope.cancellation),
        );
        Ok(OutboundRequestPrep {
            permit,
            id,
            parent_invoke_id,
            tracking,
            cancel_watch,
        })
    }

    fn acquire_outbound_permit(&self) -> Result<OwnedSemaphorePermit, PeerError> {
        Arc::clone(&self.outbound_invoke_permits)
            .try_acquire_owned()
            .map_err(|error| match error {
                tokio::sync::TryAcquireError::Closed => PeerError::Closed,
                tokio::sync::TryAcquireError::NoPermits => PeerError::Busy,
            })
    }

    pub async fn invoke_handler(
        self: &Arc<Self>,
        handler_id: &str,
        event: Value,
        caller_extension_id: &str,
    ) -> Result<Value, PeerError> {
        let output = self
            .invoke(
                CAP_HANDLER_INVOKE,
                json!({
                    "handler_id": handler_id,
                    "event": event,
                    "caller_extension_id": caller_extension_id,
                }),
                Some(caller_extension_id),
                OutboundInvokeControl::default(),
            )
            .await?;
        Ok(output)
    }

    async fn read_loop(self: Arc<Self>, work_tx: mpsc::Sender<InboundWork>) {
        loop {
            let frame = match tokio::select! {
                biased;
                () = self.close_request.cancelled() => break,
                frame = self.transport.read_frame() => frame,
            } {
                Ok(f) => f,
                Err(error) => {
                    let _ = error;
                    break;
                },
            };
            let msg = match parse_wire_message(&frame) {
                Ok(m) => m,
                Err(error) => {
                    let _ = error;
                    continue;
                },
            };
            match msg {
                WireMessage::Result(result) => self.dispatch_result(result),
                WireMessage::Event(event) => self.dispatch_event(event),
                WireMessage::Cancel(cancel) => self.cancel_inbound(cancel),
                WireMessage::Initialize(init) => {
                    if !self.enqueue_inbound_work(&work_tx, InboundWork::Initialize(init)) {
                        break;
                    }
                },
                WireMessage::Invoke(invoke) => {
                    let work = match self.register_inbound_invoke(invoke) {
                        Ok(work) => work,
                        Err(failure) => {
                            if !self.queue_rejection(
                                failure,
                                ErrorPayload::new(
                                    "duplicate_request_id",
                                    "inbound invoke request id is already active",
                                ),
                            ) {
                                break;
                            }
                            continue;
                        },
                    };
                    if !self.enqueue_inbound_work(&work_tx, work) {
                        break;
                    }
                },
            }
        }
        self.close_runtime("peer_closed").await;
    }

    async fn run_inbound_worker(
        self: Arc<Self>,
        work_rx: Arc<AsyncMutex<mpsc::Receiver<InboundWork>>>,
    ) {
        loop {
            let work = {
                let mut work_rx = work_rx.lock().await;
                work_rx.recv().await
            };
            let Some(work) = work else {
                return;
            };
            let failure = work_failure(&work);
            if AssertUnwindSafe(Arc::clone(&self).dispatch_work(work))
                .catch_unwind()
                .await
                .is_err()
            {
                tracing::error!(request_id = %failure.id, "inbound peer handler panicked");
                self.send_work_failure(
                    failure,
                    ErrorPayload::new("handler_panicked", "inbound peer handler panicked"),
                )
                .await
                .ok();
            }
        }
    }

    async fn dispatch_work(self: Arc<Self>, work: InboundWork) {
        match work {
            InboundWork::Initialize(init) => {
                self.handle_initialize(init).await;
            },
            InboundWork::Invoke {
                message,
                token,
                registration,
            } => {
                self.handle_invoke(message, token).await;
                drop(registration);
            },
        }
    }

    fn dispatch_result(&self, result: ResultMsg) {
        if let Some(tx) = self.pending_results.lock().remove(&result.id) {
            let _ = tx.send(Ok(result));
        }
    }

    fn dispatch_event(&self, event: EventMsg) {
        let done = matches!(event.phase, EventPhase::Completed | EventPhase::Failed);
        let stream_id = event.id.clone();
        let sender = {
            let mut streams = self.pending_stream_events.lock();
            let sender = streams.get(&stream_id).cloned();
            if done {
                streams.remove(&stream_id);
            }
            sender
        };
        let Some(sender) = sender else {
            return;
        };
        if let Err(error) = sender.try_send(event) {
            self.pending_stream_events.lock().remove(&stream_id);
            if matches!(error, mpsc::error::TrySendError::Full(_)) {
                tracing::warn!(request_id = %stream_id, "stream event buffer is full; closing stream");
            }
        }
    }

    fn cancel_inbound(&self, cancel: CancelMsg) {
        let token = self
            .inbound_cancel
            .lock()
            .get(&cancel.id)
            .map(|entry| entry.token.clone());
        if let Some(token) = token {
            token.cancel(cancel.reason);
        }
    }

    fn enqueue_inbound_work(&self, work_tx: &mpsc::Sender<InboundWork>, work: InboundWork) -> bool {
        match work_tx.try_send(work) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(work)) => {
                let failure = work_failure(&work);
                drop(work);
                self.queue_rejection(
                    failure,
                    ErrorPayload::new("peer_overloaded", "inbound work queue is full"),
                )
            },
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    fn register_inbound_invoke(&self, invoke: InvokeMsg) -> Result<InboundWork, WorkFailure> {
        let mut pending = self.inbound_cancel.lock();
        if pending.contains_key(&invoke.id) {
            return Err(WorkFailure {
                id: invoke.id,
                kind: ResultKind::InvokeResult,
                stream: invoke.stream,
            });
        }
        let registration_id = self
            .next_inbound_registration_id
            .fetch_add(1, Ordering::Relaxed);
        let token = CancelToken::default();
        pending.insert(
            invoke.id.clone(),
            InboundCancelEntry {
                registration_id,
                token: token.clone(),
            },
        );
        let registration = InboundCancelGuard {
            pending: Arc::clone(&self.inbound_cancel),
            id: invoke.id.clone(),
            registration_id,
        };
        Ok(InboundWork::Invoke {
            message: invoke,
            token,
            registration,
        })
    }

    fn queue_rejection(&self, failure: WorkFailure, error: ErrorPayload) -> bool {
        match self.rejection_tx.try_send(WorkRejection { failure, error }) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!("peer rejection queue is full; closing peer");
                self.begin_close();
                false
            },
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    async fn run_rejection_writer(&self, mut rx: mpsc::Receiver<WorkRejection>) {
        while let Some(rejection) = rx.recv().await {
            if let Err(error) = self
                .send_work_failure(rejection.failure, rejection.error)
                .await
            {
                if !matches!(error, PeerError::Closed) {
                    tracing::warn!(%error, "failed to send inbound work rejection");
                }
                break;
            }
        }
    }

    async fn run_cancel_writer(&self, mut rx: mpsc::Receiver<CancelWrite>) {
        while let Some(write) = rx.recv().await {
            let result = self.send_message(&WireMessage::Cancel(write.cancel)).await;
            if let Some(completion) = write.completion {
                let _ = completion.send(());
            }
            if let Err(error) = result {
                if !matches!(error, PeerError::Closed) {
                    tracing::warn!(%error, "failed to send outbound cancellation");
                }
                break;
            }
        }
    }

    fn queue_outbound_cancel(&self, cancel: CancelMsg) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        match self.cancel_tx.try_send(CancelWrite {
            cancel,
            completion: None,
        }) {
            Ok(()) => {},
            Err(mpsc::error::TrySendError::Full(write)) => {
                tracing::warn!(request_id = %write.cancel.id, "peer cancellation queue is full; dropping best-effort cancellation");
            },
            Err(mpsc::error::TrySendError::Closed(_)) => {},
        }
    }

    async fn send_work_failure(
        &self,
        failure: WorkFailure,
        error: ErrorPayload,
    ) -> Result<(), PeerError> {
        if failure.stream && failure.kind == ResultKind::InvokeResult {
            self.send_message(&WireMessage::Event(EventMsg {
                id: failure.id,
                phase: EventPhase::Failed,
                data: Value::Null,
                output: Value::Null,
                error: Some(error),
            }))
            .await
        } else {
            self.send_message(&WireMessage::Result(ResultMsg {
                id: failure.id,
                kind: Some(failure.kind),
                success: false,
                output: None,
                error: Some(error),
            }))
            .await
        }
    }

    async fn close_runtime(&self, reason: &str) {
        self.begin_close();
        let _close_guard = self.close_lock.lock().await;
        if self.close_complete.load(Ordering::Acquire) {
            return;
        }
        self.inbound_work_tx.lock().take();
        for (_, tx) in self.pending_results.lock().drain() {
            let _ = tx.send(Err(PeerError::Closed));
        }
        self.pending_stream_events.lock().clear();
        for (_, entry) in self.inbound_cancel.lock().drain() {
            entry.token.cancel(reason);
        }
        let workers = self.inbound_workers.lock().drain(..).collect::<Vec<_>>();
        let writers = self.writer_tasks.lock().drain(..).collect::<Vec<_>>();
        abort_tasks(workers, "inbound worker").await;
        abort_tasks(writers, "peer writer").await;
        self.writers_started.store(false, Ordering::Release);
        self.close_complete.store(true, Ordering::Release);
        self.closed_notify.notify_waiters();
    }

    fn begin_close(&self) {
        self.closed.store(true, Ordering::Release);
        self.outbound_invoke_permits.close();
        self.close_request.cancel();
    }

    async fn handle_initialize(self: Arc<Self>, init: InitializeMsg) {
        let handler = self.initialize_handler.lock().clone();
        let Some(handler) = handler else {
            self.send_result(
                &init.id,
                Some(ResultKind::InitializeResult),
                false,
                None,
                Some(ErrorPayload::new(
                    "not_supported",
                    "initialize handler not configured",
                )),
            )
            .await;
            return;
        };
        let init_id = init.id.clone();
        match handler(init).await {
            Ok(output) => {
                self.remote_initialized.store(true, Ordering::SeqCst);
                self.send_result(
                    &init_id,
                    Some(ResultKind::InitializeResult),
                    true,
                    Some(serde_json::to_value(output).unwrap_or(Value::Null)),
                    None,
                )
                .await;
            },
            Err(err) => {
                self.send_result(
                    &init_id,
                    Some(ResultKind::InitializeResult),
                    false,
                    None,
                    Some(err),
                )
                .await;
            },
        }
    }

    async fn handle_invoke(self: Arc<Self>, invoke: InvokeMsg, cancel_token: CancelToken) {
        let handler = self.invoke_handler.lock().clone();
        let Some(handler) = handler else {
            self.send_result(
                &invoke.id,
                Some(ResultKind::InvokeResult),
                false,
                None,
                Some(ErrorPayload::new(
                    "not_supported",
                    "invoke handler not configured",
                )),
            )
            .await;
            return;
        };
        let invoke_id = invoke.id.clone();
        let invoke_stream = invoke.stream;
        let scope = InboundInvokeScope {
            id: invoke_id.clone(),
            cancellation: cancel_token.cancellation_token(),
        };
        let result = INBOUND_INVOKE_SCOPE
            .scope(scope, handler(invoke, cancel_token))
            .await;
        match result {
            Ok(InvokeReply::Value(value)) => {
                self.send_result(
                    &invoke_id,
                    Some(ResultKind::InvokeResult),
                    true,
                    Some(value),
                    None,
                )
                .await;
            },
            Ok(InvokeReply::Events(events)) => {
                for event in events {
                    let _ = self.send_message(&WireMessage::Event(event)).await;
                }
            },
            Err(err) => {
                if invoke_stream {
                    let _ = self
                        .send_message(&WireMessage::Event(EventMsg {
                            id: invoke_id.clone(),
                            phase: EventPhase::Failed,
                            data: Value::Null,
                            output: Value::Null,
                            error: Some(err),
                        }))
                        .await;
                } else {
                    self.send_result(
                        &invoke_id,
                        Some(ResultKind::InvokeResult),
                        false,
                        None,
                        Some(err),
                    )
                    .await;
                }
            },
        }
    }

    async fn send_result(
        &self,
        id: &str,
        kind: Option<ResultKind>,
        success: bool,
        output: Option<Value>,
        error: Option<ErrorPayload>,
    ) {
        let msg = WireMessage::Result(ResultMsg {
            id: id.to_string(),
            kind,
            success,
            output,
            error,
        });
        let _ = self.send_message(&msg).await;
    }
}

fn current_inbound_invoke_scope() -> Option<InboundInvokeScope> {
    INBOUND_INVOKE_SCOPE.try_with(Clone::clone).ok()
}

fn spawn_cancel_watch<T: FrameTransport + 'static>(
    peer: Arc<Peer<T>>,
    request_id: String,
    external: Option<CancellationToken>,
    inherited: Option<CancellationToken>,
) -> AbortTaskOnDrop {
    let task = match (external, inherited) {
        (None, None) => None,
        (Some(cancellation), None) | (None, Some(cancellation)) => Some(
            crate::runtime::task_utils::spawn_traced("peer_external_cancel_watch", async move {
                cancellation.cancelled().await;
                peer.cancel_outbound(&request_id, "caller_cancelled").await;
            }),
        ),
        (Some(external), Some(inherited)) => Some(crate::runtime::task_utils::spawn_traced(
            "peer_external_cancel_watch",
            async move {
                tokio::select! {
                    () = external.cancelled() => {},
                    () = inherited.cancelled() => {},
                }
                peer.cancel_outbound(&request_id, "caller_cancelled").await;
            },
        )),
    };
    AbortTaskOnDrop(task)
}

fn schedule_outbound_cancel<T: FrameTransport + 'static>(
    peer: Arc<Peer<T>>,
    request_id: String,
    reason: &'static str,
) {
    peer.queue_outbound_cancel(CancelMsg {
        id: request_id,
        reason: reason.to_string(),
    });
}

async fn abort_tasks(tasks: Vec<JoinHandle<()>>, task_group: &'static str) {
    for task in &tasks {
        task.abort();
    }
    let joined = async move {
        for task in tasks {
            match task.await {
                Ok(()) => {},
                Err(error) if error.is_cancelled() => {},
                Err(error) => {
                    tracing::warn!(%error, task_group, "peer task failed during cleanup");
                },
            }
        }
    };
    if tokio::time::timeout(TASK_ABORT_TIMEOUT, joined)
        .await
        .is_err()
    {
        tracing::warn!(task_group, "peer tasks did not stop after abort");
    }
}

fn work_failure(work: &InboundWork) -> WorkFailure {
    match work {
        InboundWork::Initialize(init) => WorkFailure {
            id: init.id.clone(),
            kind: ResultKind::InitializeResult,
            stream: false,
        },
        InboundWork::Invoke { message, .. } => WorkFailure {
            id: message.id.clone(),
            kind: ResultKind::InvokeResult,
            stream: message.stream,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        time::Duration,
    };

    use tokio::sync::{Notify, mpsc, oneshot};

    use super::*;

    struct ChannelTransport {
        incoming: AsyncMutex<mpsc::Receiver<Vec<u8>>>,
        outgoing: mpsc::Sender<Vec<u8>>,
    }

    impl ChannelTransport {
        fn harness() -> (Self, mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) {
            Self::harness_with_outgoing_capacity(256)
        }

        fn harness_with_outgoing_capacity(
            outgoing_capacity: usize,
        ) -> (Self, mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>) {
            let (incoming_tx, incoming_rx) = mpsc::channel(256);
            let (outgoing_tx, outgoing_rx) = mpsc::channel(outgoing_capacity);
            (
                Self {
                    incoming: AsyncMutex::new(incoming_rx),
                    outgoing: outgoing_tx,
                },
                incoming_tx,
                outgoing_rx,
            )
        }

        fn pair() -> (Self, Self) {
            let (a_to_b_tx, a_to_b_rx) = mpsc::channel(256);
            let (b_to_a_tx, b_to_a_rx) = mpsc::channel(256);
            (
                Self {
                    incoming: AsyncMutex::new(b_to_a_rx),
                    outgoing: a_to_b_tx,
                },
                Self {
                    incoming: AsyncMutex::new(a_to_b_rx),
                    outgoing: b_to_a_tx,
                },
            )
        }
    }

    #[async_trait::async_trait]
    impl FrameTransport for ChannelTransport {
        async fn read_frame(&self) -> Result<Vec<u8>, io::Error> {
            self.incoming.lock().await.recv().await.ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "test transport closed")
            })
        }

        async fn write_frame(&self, payload: &[u8]) -> Result<(), io::Error> {
            self.outgoing
                .send(payload.to_vec())
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "test transport closed"))
        }
    }

    fn test_peer(transport: ChannelTransport, name: &str) -> Arc<Peer<ChannelTransport>> {
        Peer::new(
            transport,
            PeerInfo {
                name: name.into(),
                role: "test".into(),
                version: None,
            },
        )
    }

    fn invoke_message(id: impl Into<String>) -> WireMessage {
        WireMessage::Invoke(InvokeMsg {
            id: id.into(),
            capability: "test.block".into(),
            input: Value::Null,
            stream: false,
            caller_extension_id: None,
            parent_invoke_id: None,
        })
    }

    async fn next_wire(outgoing: &mut mpsc::Receiver<Vec<u8>>) -> WireMessage {
        let payload = tokio::time::timeout(Duration::from_secs(1), outgoing.recv())
            .await
            .expect("wire message timeout")
            .expect("wire channel closed");
        parse_wire_message(&payload).unwrap()
    }

    #[tokio::test]
    async fn nested_invoke_propagates_parent_id_and_parent_cancellation() {
        let (a_transport, b_transport) = ChannelTransport::pair();
        let a = test_peer(a_transport, "a");
        let b = test_peer(b_transport, "b");
        let (nested_started_tx, nested_started_rx) = oneshot::channel();
        let nested_started_tx = Arc::new(Mutex::new(Some(nested_started_tx)));
        let observed_parent = Arc::new(Mutex::new(None::<String>));
        let observed_outer = Arc::new(Mutex::new(None::<String>));
        let nested_cancelled = Arc::new(Notify::new());

        let observed_parent_in_handler = Arc::clone(&observed_parent);
        let nested_started_in_handler = Arc::clone(&nested_started_tx);
        let nested_cancelled_in_handler = Arc::clone(&nested_cancelled);
        a.set_invoke_handler(Arc::new(move |invoke, token| {
            *observed_parent_in_handler.lock() = invoke.parent_invoke_id.clone();
            if let Some(started) = nested_started_in_handler.lock().take() {
                let _ = started.send(());
            }
            let nested_cancelled = Arc::clone(&nested_cancelled_in_handler);
            Box::pin(async move {
                token.cancellation_token().cancelled().await;
                nested_cancelled.notify_one();
                Err(ErrorPayload::new("cancelled", "nested call cancelled"))
            })
        }));

        let nested_peer = Arc::clone(&b);
        let observed_outer_in_handler = Arc::clone(&observed_outer);
        b.set_invoke_handler(Arc::new(move |invoke, _token| {
            *observed_outer_in_handler.lock() = Some(invoke.id.clone());
            let nested_peer = Arc::clone(&nested_peer);
            Box::pin(async move {
                nested_peer
                    .invoke(
                        "test.nested",
                        Value::Null,
                        None,
                        OutboundInvokeControl::default(),
                    )
                    .await
                    .map(InvokeReply::Value)
                    .map_err(|error| ErrorPayload::new("nested_failed", error.to_string()))
            })
        }));

        a.start().await.unwrap();
        b.start().await.unwrap();
        let cancellation = CancellationToken::new();
        let invoke_a = Arc::clone(&a);
        let invoke_task = tokio::spawn(async move {
            invoke_a
                .invoke(
                    "test.outer",
                    Value::Null,
                    None,
                    OutboundInvokeControl {
                        external_cancel: Some(cancellation.clone()),
                        ..Default::default()
                    },
                )
                .await
        });

        nested_started_rx.await.unwrap();
        let outer_id = observed_outer.lock().clone().unwrap();
        assert_eq!(observed_parent.lock().as_deref(), Some(outer_id.as_str()));

        // Cancel the outer request. The nested request inherits that cancellation through
        // its inbound task-local scope and receives its own directional Cancel message.
        let cancel_id = outer_id.clone();
        a.cancel_outbound(&cancel_id, "test_cancel").await;
        tokio::time::timeout(Duration::from_secs(1), nested_cancelled.notified())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), invoke_task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        a.stop().await;
        b.stop().await;
    }

    #[derive(Default)]
    struct RecordingTracker {
        started: Mutex<Vec<String>>,
        finished: Mutex<Vec<String>>,
    }

    impl OutboundInvokeTracker for RecordingTracker {
        fn started(&self, request_id: &str) {
            self.started.lock().push(request_id.into());
        }

        fn finished(&self, request_id: &str) {
            self.finished.lock().push(request_id.into());
        }
    }

    #[tokio::test]
    async fn dropped_requests_and_streams_release_registrations_and_cancel_remote() {
        let (transport, _incoming, mut outgoing) = ChannelTransport::harness();
        let peer = test_peer(transport, "drop-test");
        peer.start().await.unwrap();
        let tracker = Arc::new(RecordingTracker::default());
        let invoke_peer = Arc::clone(&peer);
        let invoke_tracker = Arc::clone(&tracker);
        let invoke = tokio::spawn(async move {
            invoke_peer
                .invoke(
                    "test.pending",
                    Value::Null,
                    None,
                    OutboundInvokeControl {
                        tracker: Some(invoke_tracker),
                        ..Default::default()
                    },
                )
                .await
        });
        let WireMessage::Invoke(invoke_message) = next_wire(&mut outgoing).await else {
            panic!("expected invoke");
        };
        assert_eq!(peer.pending_results.lock().len(), 1);
        assert_eq!(
            peer.outbound_invoke_permits.available_permits(),
            OUTBOUND_INVOKE_LIMIT - 1
        );
        invoke.abort();
        assert!(invoke.await.unwrap_err().is_cancelled());
        let WireMessage::Cancel(cancel) = next_wire(&mut outgoing).await else {
            panic!("expected cancel");
        };
        assert_eq!(cancel.id, invoke_message.id);
        assert!(peer.pending_results.lock().is_empty());
        assert_eq!(
            peer.outbound_invoke_permits.available_permits(),
            OUTBOUND_INVOKE_LIMIT
        );
        assert_eq!(*tracker.started.lock(), vec![invoke_message.id.clone()]);
        assert_eq!(*tracker.finished.lock(), vec![invoke_message.id.clone()]);

        let mut stream = peer
            .invoke_stream_with_control(
                "test.stream",
                Value::Null,
                None,
                OutboundInvokeControl {
                    tracker: Some(tracker.clone()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let WireMessage::Invoke(stream_message) = next_wire(&mut outgoing).await else {
            panic!("expected stream invoke");
        };
        assert_eq!(
            peer.outbound_invoke_permits.available_permits(),
            OUTBOUND_INVOKE_LIMIT - 1
        );
        for index in 0..=STREAM_EVENT_BUFFER_CAPACITY {
            peer.dispatch_event(EventMsg {
                id: stream_message.id.clone(),
                phase: EventPhase::Delta,
                data: json!(index),
                output: Value::Null,
                error: None,
            });
        }
        assert!(peer.pending_stream_events.lock().is_empty());
        for _ in 0..STREAM_EVENT_BUFFER_CAPACITY {
            assert!(stream.next_event().await.is_some());
        }
        assert!(stream.next_event().await.is_none());
        let WireMessage::Cancel(cancel) = next_wire(&mut outgoing).await else {
            panic!("expected stream cancel");
        };
        assert_eq!(cancel.id, stream_message.id);
        assert_eq!(
            peer.outbound_invoke_permits.available_permits(),
            OUTBOUND_INVOKE_LIMIT
        );
        assert_eq!(
            *tracker.finished.lock(),
            vec![invoke_message.id, stream_message.id]
        );

        let held_permits = (0..OUTBOUND_INVOKE_LIMIT)
            .map(|_| peer.acquire_outbound_permit().unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(
            peer.invoke(
                "test.overloaded",
                Value::Null,
                None,
                OutboundInvokeControl::default()
            )
            .await,
            Err(PeerError::Busy)
        ));
        drop(held_permits);
        assert_eq!(
            peer.outbound_invoke_permits.available_permits(),
            OUTBOUND_INVOKE_LIMIT
        );
        peer.stop().await;
    }

    #[tokio::test]
    async fn stop_closes_pending_results_and_streams() {
        let (transport, _incoming, mut outgoing) = ChannelTransport::harness();
        let peer = test_peer(transport, "stop-test");
        peer.start().await.unwrap();
        let invoke_peer = Arc::clone(&peer);
        let invoke = tokio::spawn(async move {
            invoke_peer
                .invoke(
                    "test.pending",
                    Value::Null,
                    None,
                    OutboundInvokeControl::default(),
                )
                .await
        });
        let WireMessage::Invoke(_) = next_wire(&mut outgoing).await else {
            panic!("expected invoke");
        };

        let mut stream = peer
            .invoke_stream("test.stream", Value::Null, None)
            .await
            .unwrap();
        let WireMessage::Invoke(_) = next_wire(&mut outgoing).await else {
            panic!("expected stream invoke");
        };

        peer.stop().await;

        assert!(matches!(invoke.await.unwrap(), Err(PeerError::Closed)));
        assert!(stream.next_event().await.is_none());
        assert!(peer.pending_results.lock().is_empty());
        assert!(peer.pending_stream_events.lock().is_empty());
        assert!(peer.inbound_cancel.lock().is_empty());
    }

    #[tokio::test]
    async fn transport_eof_notifies_waiters_and_closes_runtime_state() {
        let (transport, incoming, _outgoing) = ChannelTransport::harness();
        let peer = test_peer(transport, "eof-test");
        peer.start().await.unwrap();
        drop(incoming);

        tokio::time::timeout(Duration::from_secs(1), peer.wait_closed())
            .await
            .expect("peer should observe transport EOF");
        assert!(peer.closed.load(Ordering::Acquire));
        assert!(peer.inbound_workers.lock().is_empty());
        peer.stop().await;
    }

    #[tokio::test]
    async fn rejection_backpressure_does_not_block_result_dispatch() {
        let (transport, incoming, mut outgoing) =
            ChannelTransport::harness_with_outgoing_capacity(1);
        let peer = test_peer(transport, "rejection-backpressure");
        let active = Arc::new(AtomicUsize::new(0));
        let handler_active = Arc::clone(&active);
        peer.set_invoke_handler(Arc::new(move |_invoke, _token| {
            let active = Arc::clone(&handler_active);
            Box::pin(async move {
                active.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<Result<InvokeReply, ErrorPayload>>().await
            })
        }));
        peer.start().await.unwrap();

        let outbound_peer = Arc::clone(&peer);
        let outbound = tokio::spawn(async move {
            outbound_peer
                .invoke(
                    "test.outbound",
                    Value::Null,
                    None,
                    OutboundInvokeControl::default(),
                )
                .await
        });
        let WireMessage::Invoke(outbound_message) = next_wire(&mut outgoing).await else {
            panic!("expected outbound invoke");
        };

        for index in 0..INBOUND_WORKER_COUNT {
            incoming
                .send(encode_wire_message(&invoke_message(format!("active-{index}"))).unwrap())
                .await
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while active.load(Ordering::SeqCst) != INBOUND_WORKER_COUNT {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        for index in 0..INBOUND_WORK_QUEUE_CAPACITY {
            incoming
                .send(encode_wire_message(&invoke_message(format!("queued-{index}"))).unwrap())
                .await
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            let expected = INBOUND_WORKER_COUNT + INBOUND_WORK_QUEUE_CAPACITY;
            while peer.inbound_cancel.lock().len() != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        incoming
            .send(encode_wire_message(&invoke_message("overflow-1")).unwrap())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while outgoing.len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        incoming
            .send(encode_wire_message(&invoke_message("overflow-2")).unwrap())
            .await
            .unwrap();
        incoming
            .send(
                encode_wire_message(&WireMessage::Result(ResultMsg {
                    id: outbound_message.id,
                    kind: Some(ResultKind::InvokeResult),
                    success: true,
                    output: Some(json!({"ok": true})),
                    error: None,
                }))
                .unwrap(),
            )
            .await
            .unwrap();

        let result = tokio::time::timeout(Duration::from_secs(1), outbound)
            .await
            .expect("Result dispatch must not wait for the rejection writer")
            .unwrap()
            .unwrap();
        assert_eq!(result["ok"], true);
        peer.stop().await;
    }

    #[tokio::test]
    async fn saturated_invoke_workers_do_not_block_result_dispatch() {
        let (transport, incoming, mut outgoing) = ChannelTransport::harness();
        let peer = test_peer(transport, "bounded-workers");
        let active = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let queued_cancelled = Arc::new(AtomicBool::new(false));
        let queued_handled = Arc::new(Notify::new());
        let handler_active = Arc::clone(&active);
        let handler_release = Arc::clone(&release);
        let handler_queued_cancelled = Arc::clone(&queued_cancelled);
        let handler_queued_handled = Arc::clone(&queued_handled);
        peer.set_invoke_handler(Arc::new(move |invoke, token| {
            let active = Arc::clone(&handler_active);
            let release = Arc::clone(&handler_release);
            let queued_cancelled = Arc::clone(&handler_queued_cancelled);
            let queued_handled = Arc::clone(&handler_queued_handled);
            Box::pin(async move {
                if invoke.id == "queued-cancel" {
                    queued_cancelled.store(token.is_cancelled(), Ordering::SeqCst);
                    queued_handled.notify_one();
                    return Ok(InvokeReply::Value(Value::Null));
                }
                active.fetch_add(1, Ordering::SeqCst);
                release.notified().await;
                Ok(InvokeReply::Value(Value::Null))
            })
        }));
        peer.start().await.unwrap();
        assert_eq!(peer.inbound_workers.lock().len(), INBOUND_WORKER_COUNT);

        for index in 0..INBOUND_WORKER_COUNT {
            incoming
                .send(encode_wire_message(&invoke_message(format!("inbound-{index}"))).unwrap())
                .await
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            while active.load(Ordering::SeqCst) != INBOUND_WORKER_COUNT {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let outbound_peer = Arc::clone(&peer);
        let outbound = tokio::spawn(async move {
            outbound_peer
                .invoke(
                    "test.outbound",
                    Value::Null,
                    None,
                    OutboundInvokeControl::default(),
                )
                .await
        });
        let WireMessage::Invoke(outbound_message) = next_wire(&mut outgoing).await else {
            panic!("expected outbound invoke");
        };
        incoming
            .send(
                encode_wire_message(&WireMessage::Result(ResultMsg {
                    id: outbound_message.id,
                    kind: Some(ResultKind::InvokeResult),
                    success: true,
                    output: Some(json!({"ok": true})),
                    error: None,
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), outbound)
            .await
            .expect("Result must bypass saturated invoke workers")
            .unwrap()
            .unwrap();
        assert_eq!(result["ok"], true);

        incoming
            .send(encode_wire_message(&invoke_message("queued-cancel")).unwrap())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !peer.inbound_cancel.lock().contains_key("queued-cancel") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        incoming
            .send(
                encode_wire_message(&WireMessage::Cancel(CancelMsg {
                    id: "queued-cancel".into(),
                    reason: "cancel_before_worker".into(),
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !peer
                .inbound_cancel
                .lock()
                .get("queued-cancel")
                .is_some_and(|entry| entry.token.is_cancelled())
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(1), queued_handled.notified())
            .await
            .unwrap();
        assert!(
            queued_cancelled.load(Ordering::SeqCst),
            "a queued invoke must retain cancellation received before a worker starts it"
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while peer.inbound_cancel.lock().contains_key("queued-cancel") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        release.notify_waiters();
        peer.stop().await;
        assert!(peer.inbound_workers.lock().is_empty());
    }
}
