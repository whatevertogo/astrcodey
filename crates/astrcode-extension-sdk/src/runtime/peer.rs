//! s5r Peer 状态机（stdio 帧 + WireMessage）。

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex as AsyncMutex, oneshot},
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

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

type PendingResultTx = oneshot::Sender<Result<ResultMsg, PeerError>>;
type PendingResults = HashMap<String, PendingResultTx>;

pub type InitializeHandler =
    Arc<dyn Fn(InitializeMsg) -> BoxFuture<Result<InitializeOutput, ErrorPayload>> + Send + Sync>;

pub type InvokeHandler = Arc<
    dyn Fn(InvokeMsg, CancelToken) -> BoxFuture<Result<InvokeReply, ErrorPayload>> + Send + Sync,
>;

/// 出站 invoke 的可选控制（取消联动、in-flight 跟踪）。
#[derive(Clone, Default)]
pub struct OutboundInvokeControl {
    pub external_cancel: Option<CancellationToken>,
    pub track_outbound_id: Option<Arc<RwLock<Option<String>>>>,
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
    #[error("payload error: {0}")]
    Payload(String),
}

pub struct Peer<T: FrameTransport + 'static> {
    transport: Arc<T>,
    peer_info: PeerInfo,
    protocol_version: String,
    next_id: AtomicU64,
    closed: AtomicBool,
    remote_initialized: Arc<AtomicBool>,
    pending_results: Arc<AsyncMutex<PendingResults>>,
    pending_stream_events:
        Arc<AsyncMutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<EventMsg>>>>,
    initialize_handler: Mutex<Option<InitializeHandler>>,
    invoke_handler: Mutex<Option<InvokeHandler>>,
    read_task: Mutex<Option<JoinHandle<()>>>,
    inbound_cancel: Arc<AsyncMutex<HashMap<String, CancellationToken>>>,
}

impl<T: FrameTransport + 'static> Peer<T> {
    pub fn new(transport: T, peer_info: PeerInfo) -> Arc<Self> {
        Arc::new(Self {
            transport: Arc::new(transport),
            peer_info,
            protocol_version: S5R_VERSION.to_string(),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            remote_initialized: Arc::new(AtomicBool::new(false)),
            pending_results: Arc::new(AsyncMutex::new(HashMap::new())),
            pending_stream_events: Arc::new(AsyncMutex::new(HashMap::new())),
            initialize_handler: Mutex::new(None),
            invoke_handler: Mutex::new(None),
            read_task: Mutex::new(None),
            inbound_cancel: Arc::new(AsyncMutex::new(HashMap::new())),
        })
    }

    pub fn set_initialize_handler(self: &Arc<Self>, handler: InitializeHandler) {
        *self.initialize_handler.lock() = Some(handler);
    }

    pub fn set_invoke_handler(self: &Arc<Self>, handler: InvokeHandler) {
        *self.invoke_handler.lock() = Some(handler);
    }

    pub async fn start(self: &Arc<Self>) -> Result<(), PeerError> {
        let peer = Arc::clone(self);
        let task = tokio::spawn(async move {
            peer.read_loop().await;
        });
        *self.read_task.lock() = Some(task);
        Ok(())
    }

    pub async fn stop(self: &Arc<Self>) {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(task) = self.read_task.lock().take() {
            task.abort();
        }
        let mut pending = self.pending_results.lock().await;
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(PeerError::Closed));
        }
    }

    pub fn is_remote_initialized(&self) -> bool {
        self.remote_initialized.load(Ordering::SeqCst)
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
        if self.closed.load(Ordering::SeqCst) {
            return Err(PeerError::Closed);
        }
        let payload = encode_wire_message(msg).map_err(PeerError::Msg)?;
        self.transport
            .write_frame(&payload)
            .await
            .map_err(|e| PeerError::Msg(format!("write frame: {e}")))
    }

    async fn request_result(
        self: &Arc<Self>,
        id: String,
        msg: WireMessage,
        timeout: std::time::Duration,
    ) -> Result<ResultMsg, PeerError> {
        let (tx, rx) = oneshot::channel();
        self.pending_results.lock().await.insert(id.clone(), tx);
        self.send_message(&msg).await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                self.pending_results.lock().await.remove(&id);
                Err(PeerError::Closed)
            },
            Err(_) => {
                self.pending_results.lock().await.remove(&id);
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
        let id = self.next_id();
        let wire_id = id.clone();
        if let Some(slot) = &control.track_outbound_id {
            *slot.write() = Some(wire_id.clone());
        }
        let cancel_watch = control.external_cancel.map(|ct| {
            let peer = Arc::clone(self);
            let id_for_cancel = wire_id.clone();
            tokio::spawn(async move {
                ct.cancelled().await;
                peer.cancel(&id_for_cancel, "caller_cancelled").await;
            })
        });
        let msg = WireMessage::Invoke(InvokeMsg {
            id: wire_id,
            capability: capability.to_string(),
            input,
            stream: false,
            caller_extension_id: caller_extension_id.map(str::to_string),
        });
        let result = self.request_result(id, msg, DEFAULT_INVOKE_TIMEOUT).await;
        if let Some(slot) = &control.track_outbound_id {
            *slot.write() = None;
        }
        if let Some(watch) = cancel_watch {
            watch.abort();
        }
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
        &self,
        capability: &str,
        input: Value,
        caller_extension_id: Option<&str>,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<EventMsg>, PeerError> {
        let id = self.next_id();
        let wire_id = id.clone();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.pending_stream_events.lock().await.insert(wire_id, tx);
        let msg = WireMessage::Invoke(InvokeMsg {
            id,
            capability: capability.to_string(),
            input,
            stream: true,
            caller_extension_id: caller_extension_id.map(str::to_string),
        });
        self.send_message(&msg).await?;
        Ok(rx)
    }

    pub async fn invoke_stream(
        self: &Arc<Self>,
        capability: &str,
        input: Value,
        caller_extension_id: Option<&str>,
    ) -> Result<EventStream, PeerError> {
        let rx = self
            .begin_invoke_stream(capability, input, caller_extension_id)
            .await?;
        Ok(EventStream::new(rx))
    }

    pub async fn invoke_stream_collect(
        self: &Arc<Self>,
        capability: &str,
        input: Value,
        caller_extension_id: Option<&str>,
    ) -> Result<Value, PeerError> {
        let mut rx = self
            .begin_invoke_stream(capability, input, caller_extension_id)
            .await?;
        let mut last_output = Value::Null;
        let deadline = tokio::time::Instant::now() + DEFAULT_STREAM_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let event = tokio::time::timeout(remaining, rx.recv())
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

    pub async fn cancel(self: &Arc<Self>, request_id: &str, reason: &str) {
        let msg = WireMessage::Cancel(CancelMsg {
            id: request_id.to_string(),
            reason: reason.to_string(),
        });
        let _ = self.send_message(&msg).await;
        if let Some(token) = self.inbound_cancel.lock().await.remove(request_id) {
            token.cancel();
        }
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

    async fn read_loop(self: Arc<Self>) {
        loop {
            let frame = match self.transport.read_frame().await {
                Ok(f) => f,
                Err(e) => {
                    let _ = e;
                    break;
                },
            };
            let msg = match parse_wire_message(&frame) {
                Ok(m) => m,
                Err(e) => {
                    let _ = e;
                    continue;
                },
            };
            let peer = Arc::clone(&self);
            tokio::spawn(async move {
                peer.dispatch_inbound(msg).await;
            });
        }
        self.closed.store(true, Ordering::SeqCst);
        let mut pending = self.pending_results.lock().await;
        for (_, tx) in pending.drain() {
            let _ = tx.send(Err(PeerError::Closed));
        }
    }

    async fn dispatch_inbound(self: Arc<Self>, msg: WireMessage) {
        match msg {
            WireMessage::Result(result) => {
                if let Some(tx) = self.pending_results.lock().await.remove(&result.id) {
                    let _ = tx.send(Ok(result));
                }
            },
            WireMessage::Event(event) => {
                let mut streams = self.pending_stream_events.lock().await;
                if let Some(tx) = streams.get(&event.id) {
                    let done = matches!(event.phase, EventPhase::Completed | EventPhase::Failed);
                    let stream_id = event.id.clone();
                    let _ = tx.send(event);
                    if done {
                        streams.remove(&stream_id);
                    }
                }
            },
            WireMessage::Cancel(cancel) => {
                if let Some(token) = self.inbound_cancel.lock().await.remove(&cancel.id) {
                    token.cancel();
                }
            },
            WireMessage::Initialize(init) => {
                self.handle_initialize(init).await;
            },
            WireMessage::Invoke(invoke) => {
                self.handle_invoke(invoke).await;
            },
        }
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

    async fn handle_invoke(self: Arc<Self>, invoke: InvokeMsg) {
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
        let cancel_token = CancelToken::default();
        let host_cancel = CancellationToken::new();
        self.inbound_cancel
            .lock()
            .await
            .insert(invoke_id.clone(), host_cancel.clone());
        let child_token = cancel_token.clone();
        let watch = host_cancel.clone();
        tokio::spawn(async move {
            watch.cancelled().await;
            child_token.cancel("host_cancel");
        });
        let result = handler(invoke, cancel_token).await;
        self.inbound_cancel.lock().await.remove(&invoke_id);
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
