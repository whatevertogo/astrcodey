use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

use futures_util::Stream;
use serde_json::Value;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, mpsc, oneshot},
    task::JoinSet,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use crate::{
    FrameTransport, TerminalStream, WireErrorCode,
    peer::{PeerError, Ready},
    protocol::{
        CapabilityDescriptor, ErrorPayload, FeatureName, HandlerDescriptor, InvokeMsg,
        ModelStreamEvent, PeerInfo, ResultKind, ResultMsg, StreamMsg, WireMessage,
        encode_wire_message, parse_wire_message,
    },
};

const COMMAND_QUEUE_CAPACITY: usize = 256;
const STREAM_BUFFER_CAPACITY: usize = 32;
const STREAM_FORWARD_BUFFER_CAPACITY: usize = 256;
const STREAM_BACKPRESSURE_TIMEOUT: Duration = Duration::from_secs(30);
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const CANCELLED_REQUEST_CAPACITY: usize = COMMAND_QUEUE_CAPACITY;
const MAX_IN_FLIGHT_REQUESTS: usize = COMMAND_QUEUE_CAPACITY;

type UnaryResult = Result<Value, ErrorPayload>;

enum PendingRequest {
    Unary {
        response: oneshot::Sender<UnaryResult>,
        _permit: OwnedSemaphorePermit,
    },
    Stream(PendingStream),
}

struct PendingStream {
    events: mpsc::Sender<ModelStreamEvent>,
    failure: Arc<Mutex<Option<ErrorPayload>>>,
    started: bool,
    _permit: OwnedSemaphorePermit,
}

#[derive(Default)]
struct CancelledRequests {
    ids: HashSet<String>,
    order: VecDeque<String>,
}

impl CancelledRequests {
    fn insert(&mut self, id: String) {
        if !self.ids.insert(id.clone()) {
            return;
        }
        self.order.push_back(id);
        while self.ids.len() > CANCELLED_REQUEST_CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.ids.remove(&oldest);
            }
        }
    }

    fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    fn remove(&mut self, id: &str) {
        self.ids.remove(id);
    }
}

enum DriverCommand {
    Invoke {
        message: InvokeMsg,
        response: oneshot::Sender<UnaryResult>,
        accepted: oneshot::Sender<Result<(), ErrorPayload>>,
        permit: OwnedSemaphorePermit,
    },
    InvokeStream {
        message: InvokeMsg,
        output: mpsc::Sender<ModelStreamEvent>,
        failure: Arc<Mutex<Option<ErrorPayload>>>,
        accepted: oneshot::Sender<Result<(), ErrorPayload>>,
        permit: OwnedSemaphorePermit,
    },
}

enum ControlCommand {
    Cancel { id: String, reason: &'static str },
}

enum TaskCompletion {
    Inbound(String),
    StreamForward,
}

/// Cloneable request surface for a running [`PeerDriver`].
pub struct PeerHandle<T> {
    transport: Arc<T>,
    state: Arc<Ready>,
    command_tx: mpsc::Sender<DriverCommand>,
    control_tx: mpsc::UnboundedSender<ControlCommand>,
    next_request_id: Arc<AtomicU64>,
    outbound_permits: Arc<Semaphore>,
}

impl<T> Clone for PeerHandle<T> {
    fn clone(&self) -> Self {
        Self {
            transport: Arc::clone(&self.transport),
            state: Arc::clone(&self.state),
            command_tx: self.command_tx.clone(),
            control_tx: self.control_tx.clone(),
            next_request_id: Arc::clone(&self.next_request_id),
            outbound_permits: Arc::clone(&self.outbound_permits),
        }
    }
}

impl<T> PeerHandle<T>
where
    T: FrameTransport + 'static,
{
    pub fn negotiated_features(&self) -> &BTreeSet<FeatureName> {
        &self.state.negotiated_features
    }

    pub fn remote_peer(&self) -> &PeerInfo {
        &self.state.remote_peer
    }

    pub fn remote_handlers(&self) -> &[HandlerDescriptor] {
        &self.state.remote_handlers
    }

    pub fn remote_capabilities(&self) -> &[CapabilityDescriptor] {
        &self.state.remote_capabilities
    }

    pub fn remote_metadata(&self) -> &Value {
        &self.state.remote_metadata
    }

    pub fn nested(&self, parent_invoke_id: impl Into<String>) -> NestedPeer<T> {
        NestedPeer {
            handle: self.clone(),
            parent_invoke_id: parent_invoke_id.into(),
        }
    }

    pub async fn invoke(
        &self,
        operation: impl Into<String>,
        input: Value,
    ) -> Result<Value, InvokeError> {
        let id = self.allocate_request_id();
        self.invoke_with_id_and_parent(id, operation.into(), input, None)
            .await
    }

    pub fn allocate_request_id(&self) -> String {
        format!(
            "invoke-{}",
            self.next_request_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    pub async fn invoke_with_id(
        &self,
        id: impl Into<String>,
        operation: impl Into<String>,
        input: Value,
    ) -> Result<Value, InvokeError> {
        self.invoke_with_id_and_parent(id.into(), operation.into(), input, None)
            .await
    }

    pub async fn invoke_stream(
        &self,
        operation: impl Into<String>,
        input: Value,
    ) -> Result<PeerStream, InvokeError> {
        self.invoke_stream_with_parent(operation.into(), input, None)
            .await
    }

    async fn invoke_with_parent(
        &self,
        operation: String,
        input: Value,
        parent_invoke_id: Option<String>,
    ) -> Result<Value, InvokeError> {
        let id = self.allocate_request_id();
        self.invoke_with_id_and_parent(id, operation, input, parent_invoke_id)
            .await
    }

    async fn invoke_with_id_and_parent(
        &self,
        id: String,
        operation: String,
        input: Value,
        parent_invoke_id: Option<String>,
    ) -> Result<Value, InvokeError> {
        self.validate_outbound(&id, &operation, parent_invoke_id.as_deref(), false)?;
        let permit = self
            .acquire_outbound_permit(parent_invoke_id.is_some())
            .await?;
        let (response_tx, response_rx) = oneshot::channel();
        let (accepted_tx, accepted_rx) = oneshot::channel();
        self.command_tx
            .send(DriverCommand::Invoke {
                message: InvokeMsg {
                    id: id.clone(),
                    operation,
                    input,
                    stream: false,
                    parent_invoke_id,
                },
                response: response_tx,
                accepted: accepted_tx,
                permit,
            })
            .await
            .map_err(|_| InvokeError::DriverUnavailable)?;
        accepted_rx
            .await
            .map_err(|_| InvokeError::DriverUnavailable)?
            .map_err(InvokeError::Local)?;

        let mut cancel = CancelOnDrop::new(id, self.control_tx.clone());
        let result = response_rx.await.map_err(|_| InvokeError::PeerClosed)?;
        cancel.disarm();
        result.map_err(InvokeError::Remote)
    }

    async fn invoke_stream_with_parent(
        &self,
        operation: String,
        input: Value,
        parent_invoke_id: Option<String>,
    ) -> Result<PeerStream, InvokeError> {
        let id = self.allocate_request_id();
        self.validate_outbound(&id, &operation, parent_invoke_id.as_deref(), true)?;
        let permit = self
            .acquire_outbound_permit(parent_invoke_id.is_some())
            .await?;
        let (output_tx, output_rx) = mpsc::channel(STREAM_BUFFER_CAPACITY);
        let failure = Arc::new(Mutex::new(None));
        let (accepted_tx, accepted_rx) = oneshot::channel();
        self.command_tx
            .send(DriverCommand::InvokeStream {
                message: InvokeMsg {
                    id: id.clone(),
                    operation,
                    input,
                    stream: true,
                    parent_invoke_id,
                },
                output: output_tx,
                failure: Arc::clone(&failure),
                accepted: accepted_tx,
                permit,
            })
            .await
            .map_err(|_| InvokeError::DriverUnavailable)?;
        accepted_rx
            .await
            .map_err(|_| InvokeError::DriverUnavailable)?
            .map_err(InvokeError::Local)?;
        Ok(PeerStream {
            id,
            stream: TerminalStream::new(model_event_stream(output_rx), failure),
            control_tx: self.control_tx.clone(),
            terminal: false,
            started_at: Instant::now(),
            first_delta_observed: false,
        })
    }

    fn validate_outbound(
        &self,
        id: &str,
        operation: &str,
        parent_invoke_id: Option<&str>,
        stream: bool,
    ) -> Result<(), InvokeError> {
        if id.is_empty() || operation.is_empty() {
            return Err(InvokeError::Local(ErrorPayload::new(
                WireErrorCode::InvalidRequest,
                "request id and operation must not be empty",
            )));
        }
        if parent_invoke_id.is_some()
            && !self
                .state
                .negotiated_features
                .contains(&FeatureName::nested_invoke_v1())
        {
            return Err(InvokeError::Local(ErrorPayload::new(
                WireErrorCode::UnsupportedFeature,
                "nested invoke was not negotiated",
            )));
        }
        if stream
            && !self
                .state
                .negotiated_features
                .contains(&FeatureName::model_stream_v1())
        {
            return Err(InvokeError::Local(ErrorPayload::new(
                WireErrorCode::UnsupportedFeature,
                "model stream was not negotiated",
            )));
        }
        Ok(())
    }

    async fn acquire_outbound_permit(
        &self,
        nested: bool,
    ) -> Result<OwnedSemaphorePermit, InvokeError> {
        let permits = Arc::clone(&self.outbound_permits);
        if !nested {
            return permits
                .acquire_owned()
                .await
                .map_err(|_| InvokeError::DriverUnavailable);
        }

        permits.try_acquire_owned().map_err(|error| match error {
            TryAcquireError::Closed => InvokeError::DriverUnavailable,
            TryAcquireError::NoPermits => InvokeError::Local(ErrorPayload::new(
                WireErrorCode::PeerOverloaded,
                "peer has reached its in-flight request limit",
            )),
        })
    }
}

/// A handle that automatically attaches the current inbound invocation as the parent.
pub struct NestedPeer<T> {
    handle: PeerHandle<T>,
    parent_invoke_id: String,
}

impl<T> Clone for NestedPeer<T> {
    fn clone(&self) -> Self {
        Self {
            handle: self.handle.clone(),
            parent_invoke_id: self.parent_invoke_id.clone(),
        }
    }
}

impl<T> NestedPeer<T>
where
    T: FrameTransport + 'static,
{
    pub async fn invoke(
        &self,
        operation: impl Into<String>,
        input: Value,
    ) -> Result<Value, InvokeError> {
        self.handle
            .invoke_with_parent(operation.into(), input, Some(self.parent_invoke_id.clone()))
            .await
    }

    pub async fn invoke_stream(
        &self,
        operation: impl Into<String>,
        input: Value,
    ) -> Result<PeerStream, InvokeError> {
        self.handle
            .invoke_stream_with_parent(operation.into(), input, Some(self.parent_invoke_id.clone()))
            .await
    }
}

/// A single inbound request plus its cancellation and nested-call context.
pub struct InboundInvoke<T> {
    pub request: InvokeMsg,
    pub cancellation: CancellationToken,
    pub nested: NestedPeer<T>,
}

pub type ModelEventStream = Pin<Box<dyn Stream<Item = ModelStreamEvent> + Send>>;

/// Result returned by an inbound invocation handler.
pub enum InvocationResponse {
    Unary(Value),
    Stream(ModelEventStream),
}

pub fn model_event_stream(receiver: mpsc::Receiver<ModelStreamEvent>) -> ModelEventStream {
    Box::pin(ReceiverModelStream { receiver })
}

struct ReceiverModelStream {
    receiver: mpsc::Receiver<ModelStreamEvent>,
}

impl Stream for ReceiverModelStream {
    type Item = ModelStreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(context)
    }
}

#[async_trait::async_trait]
pub trait PeerInvokeHandler<T>: Send + Sync
where
    T: FrameTransport + 'static,
{
    async fn invoke(
        &self,
        invocation: InboundInvoke<T>,
    ) -> Result<InvocationResponse, ErrorPayload>;
}

/// Explicit owner of peer reads, pending calls, inbound tasks, and cancellation state.
pub struct PeerDriver<T> {
    transport: Arc<T>,
    state: Arc<Ready>,
    command_rx: mpsc::Receiver<DriverCommand>,
    control_rx: mpsc::UnboundedReceiver<ControlCommand>,
    handle: PeerHandle<T>,
    inbound_permits: Arc<Semaphore>,
}

pub(crate) fn runtime_parts<T>(transport: Arc<T>, state: Ready) -> (PeerHandle<T>, PeerDriver<T>)
where
    T: FrameTransport + 'static,
{
    let state = Arc::new(state);
    let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let outbound_permits = Arc::new(Semaphore::new(MAX_IN_FLIGHT_REQUESTS));
    let handle = PeerHandle {
        transport: Arc::clone(&transport),
        state: Arc::clone(&state),
        command_tx,
        control_tx,
        next_request_id: Arc::new(AtomicU64::new(1)),
        outbound_permits,
    };
    let driver = PeerDriver {
        transport,
        state,
        command_rx,
        control_rx,
        handle: handle.clone(),
        inbound_permits: Arc::new(Semaphore::new(MAX_IN_FLIGHT_REQUESTS)),
    };
    (handle, driver)
}

impl<T> PeerDriver<T>
where
    T: FrameTransport + 'static,
{
    pub async fn run<H>(self, handler: Arc<H>) -> Result<(), PeerError>
    where
        H: PeerInvokeHandler<T> + 'static,
    {
        self.run_until(handler, CancellationToken::new()).await
    }

    pub async fn run_until<H>(
        mut self,
        handler: Arc<H>,
        shutdown: CancellationToken,
    ) -> Result<(), PeerError>
    where
        H: PeerInvokeHandler<T> + 'static,
    {
        let mut pending = HashMap::<String, PendingRequest>::new();
        let mut cancelled = CancelledRequests::default();
        let mut inbound = HashMap::<String, CancellationToken>::new();
        let mut tasks = JoinSet::<Result<TaskCompletion, PeerError>>::new();
        let result = loop {
            tokio::select! {
                () = shutdown.cancelled() => break Ok(()),
                Some(control) = self.control_rx.recv() => {
                    if let Err(error) = self
                        .handle_control(control, &mut pending, &mut cancelled)
                        .await
                    {
                        break Err(error);
                    }
                }
                Some(command) = self.command_rx.recv() => {
                    if let Err(error) = self.handle_command(
                        command,
                        &mut pending,
                        &inbound,
                        &mut tasks,
                    ).await {
                        break Err(error);
                    }
                }
                Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                    match joined {
                        Ok(Ok(TaskCompletion::Inbound(id))) => {
                            inbound.remove(&id);
                        }
                        Ok(Ok(TaskCompletion::StreamForward)) => {}
                        Ok(Err(error)) => break Err(error),
                        Err(error) => {
                            break Err(PeerError::Protocol(format!(
                                "peer-owned invocation task failed: {error}"
                            )));
                        }
                    }
                }
                frame = crate::frame::read_traced_frame(self.transport.as_ref()) => {
                    let message = match frame {
                        Ok(frame) => match parse_wire_message(&frame) {
                            Ok(message) => message,
                            Err(error) => break Err(error.into()),
                        },
                        Err(error) => break Err(error.into()),
                    };
                    if let Err(error) = self.handle_message(
                        message,
                        Arc::clone(&handler),
                        &mut pending,
                        &mut cancelled,
                        &mut inbound,
                        &mut tasks,
                    ).await {
                        break Err(error);
                    }
                }
            }
        };

        for cancellation in inbound.values() {
            cancellation.cancel();
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        for request in pending.values() {
            if let PendingRequest::Stream(stream) = request {
                set_stream_failure(
                    &stream.failure,
                    ErrorPayload::new(
                        WireErrorCode::PeerClosed,
                        "peer closed before the stream completed",
                    ),
                );
            }
        }
        pending.clear();
        result
    }

    async fn handle_command(
        &self,
        command: DriverCommand,
        pending: &mut HashMap<String, PendingRequest>,
        inbound: &HashMap<String, CancellationToken>,
        tasks: &mut JoinSet<Result<TaskCompletion, PeerError>>,
    ) -> Result<(), PeerError> {
        match command {
            DriverCommand::Invoke {
                message,
                response,
                accepted,
                permit,
            } => {
                if accepted.is_closed() {
                    return Ok(());
                }
                if let Err(error) = validate_parent(&message, inbound) {
                    let _ = accepted.send(Err(error));
                    return Ok(());
                }
                if pending.contains_key(&message.id) {
                    let _ = accepted.send(Err(ErrorPayload::new(
                        WireErrorCode::DuplicateRequestId,
                        "duplicate outbound request id",
                    )));
                    return Ok(());
                }
                let id = message.id.clone();
                pending.insert(
                    id.clone(),
                    PendingRequest::Unary {
                        response,
                        _permit: permit,
                    },
                );
                match self.write(&WireMessage::Invoke(message)).await {
                    Ok(()) => {
                        let _ = accepted.send(Ok(()));
                    },
                    Err(error) => {
                        pending.remove(&id);
                        let _ = accepted.send(Err(transport_payload(&error)));
                        return Err(error);
                    },
                }
            },
            DriverCommand::InvokeStream {
                message,
                output,
                failure,
                accepted,
                permit,
            } => {
                if accepted.is_closed() {
                    return Ok(());
                }
                if let Err(error) = validate_parent(&message, inbound) {
                    let _ = accepted.send(Err(error));
                    return Ok(());
                }
                if pending.contains_key(&message.id) {
                    let _ = accepted.send(Err(ErrorPayload::new(
                        WireErrorCode::DuplicateRequestId,
                        "duplicate outbound request id",
                    )));
                    return Ok(());
                }
                let id = message.id.clone();
                let (forward_tx, forward_rx) = mpsc::channel(STREAM_FORWARD_BUFFER_CAPACITY);
                pending.insert(
                    id.clone(),
                    PendingRequest::Stream(PendingStream {
                        events: forward_tx,
                        failure: Arc::clone(&failure),
                        started: false,
                        _permit: permit,
                    }),
                );
                let control_tx = self.handle.control_tx.clone();
                let stream_id = id.clone();
                tasks.spawn(async move {
                    forward_stream(stream_id, forward_rx, output, failure, control_tx).await;
                    Ok(TaskCompletion::StreamForward)
                });
                match self.write(&WireMessage::Invoke(message)).await {
                    Ok(()) => {
                        let _ = accepted.send(Ok(()));
                    },
                    Err(error) => {
                        pending.remove(&id);
                        let _ = accepted.send(Err(transport_payload(&error)));
                        return Err(error);
                    },
                }
            },
        }
        Ok(())
    }

    async fn handle_control(
        &self,
        control: ControlCommand,
        pending: &mut HashMap<String, PendingRequest>,
        cancelled: &mut CancelledRequests,
    ) -> Result<(), PeerError> {
        match control {
            ControlCommand::Cancel { id, reason } => {
                pending.remove(&id);
                cancelled.insert(id.clone());
                self.write(&WireMessage::Cancel(crate::protocol::CancelMsg {
                    id,
                    reason: reason.into(),
                }))
                .await?;
            },
        }
        Ok(())
    }

    async fn handle_message<H>(
        &self,
        message: WireMessage,
        handler: Arc<H>,
        pending: &mut HashMap<String, PendingRequest>,
        cancelled: &mut CancelledRequests,
        inbound: &mut HashMap<String, CancellationToken>,
        tasks: &mut JoinSet<Result<TaskCompletion, PeerError>>,
    ) -> Result<(), PeerError>
    where
        H: PeerInvokeHandler<T> + 'static,
    {
        match message {
            WireMessage::Result(result) => route_result(result, pending, cancelled),
            WireMessage::Stream(stream) => {
                route_stream(stream, pending, cancelled, &self.handle.control_tx)
            },
            WireMessage::Cancel(cancel) => {
                if let Some(cancellation) = inbound.get(&cancel.id) {
                    cancellation.cancel();
                }
                Ok(())
            },
            WireMessage::Invoke(request) => {
                if inbound.contains_key(&request.id) {
                    self.write(&failed_result(
                        request.id,
                        WireErrorCode::DuplicateRequestId,
                        "duplicate inbound request id",
                    ))
                    .await?;
                    return Ok(());
                }
                if let Err(error) =
                    validate_inbound_features(&request, &self.state.negotiated_features, pending)
                {
                    self.write(&WireMessage::Result(ResultMsg {
                        id: request.id,
                        kind: ResultKind::Invoke,
                        success: false,
                        output: None,
                        error: Some(error),
                    }))
                    .await?;
                    return Ok(());
                }
                let permit = match Arc::clone(&self.inbound_permits).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        self.write(&failed_result(
                            request.id,
                            WireErrorCode::PeerOverloaded,
                            "peer has reached its in-flight request limit",
                        ))
                        .await?;
                        return Ok(());
                    },
                };
                let cancellation = CancellationToken::new();
                inbound.insert(request.id.clone(), cancellation.clone());
                let nested = self.handle.nested(request.id.clone());
                let transport = Arc::clone(&self.transport);
                tasks.spawn(async move {
                    run_inbound(
                        transport,
                        handler,
                        InboundInvoke {
                            request,
                            cancellation,
                            nested,
                        },
                        permit,
                    )
                    .await
                    .map(TaskCompletion::Inbound)
                });
                Ok(())
            },
            WireMessage::Initialize(_) => Err(PeerError::UnexpectedMessage(
                "post-initialize invoke, result, stream, or cancel message",
            )),
        }
    }

    async fn write(&self, message: &WireMessage) -> Result<(), PeerError> {
        write_message(self.transport.as_ref(), message).await
    }
}

async fn run_inbound<T, H>(
    transport: Arc<T>,
    handler: Arc<H>,
    invocation: InboundInvoke<T>,
    _permit: OwnedSemaphorePermit,
) -> Result<String, PeerError>
where
    T: FrameTransport + 'static,
    H: PeerInvokeHandler<T> + 'static,
{
    let id = invocation.request.id.clone();
    let wants_stream = invocation.request.stream;
    let cancellation = invocation.cancellation.clone();
    match handler.invoke(invocation).await {
        Ok(InvocationResponse::Unary(output)) if !wants_stream => {
            write_message(
                transport.as_ref(),
                &WireMessage::Result(ResultMsg {
                    id: id.clone(),
                    kind: ResultKind::Invoke,
                    success: true,
                    output: Some(output),
                    error: None,
                }),
            )
            .await?;
        },
        Ok(InvocationResponse::Stream(mut stream)) if wants_stream => {
            let mut started = false;
            loop {
                let event = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => break,
                    event = timeout(STREAM_IDLE_TIMEOUT, futures_util::StreamExt::next(&mut stream)) => match event {
                        Ok(Some(event)) => event,
                        Ok(None) => ModelStreamEvent::Failed {
                            error: ErrorPayload::new(
                                WireErrorCode::StreamClosed,
                                "stream producer closed before a terminal event",
                            ),
                        },
                        Err(_) => ModelStreamEvent::Failed {
                            error: {
                                tracing::warn!(stream_id = %id, "S5R stream exceeded its idle deadline");
                                ErrorPayload::new(
                                    WireErrorCode::StreamIdleTimeout,
                                    "stream producer exceeded the idle deadline",
                                )
                            },
                        },
                    },
                };
                let event = validate_outbound_stream_event(event, &mut started);
                let terminal = event.is_terminal();
                write_message(
                    transport.as_ref(),
                    &WireMessage::Stream(StreamMsg {
                        id: id.clone(),
                        event,
                    }),
                )
                .await?;
                if terminal {
                    break;
                }
            }
        },
        Ok(_) => {
            write_message(
                transport.as_ref(),
                &failed_result(
                    id.clone(),
                    WireErrorCode::InvalidResponse,
                    "handler response mode does not match invoke mode",
                ),
            )
            .await?;
        },
        Err(error) => {
            write_message(
                transport.as_ref(),
                &WireMessage::Result(ResultMsg {
                    id: id.clone(),
                    kind: ResultKind::Invoke,
                    success: false,
                    output: None,
                    error: Some(error),
                }),
            )
            .await?;
        },
    }
    Ok(id)
}

fn validate_outbound_stream_event(event: ModelStreamEvent, started: &mut bool) -> ModelStreamEvent {
    match event {
        ModelStreamEvent::Started if !*started => {
            *started = true;
            ModelStreamEvent::Started
        },
        ModelStreamEvent::Failed { .. } => event,
        ModelStreamEvent::Started => invalid_stream_event("stream started more than once"),
        _ if !*started => invalid_stream_event("stream event arrived before started"),
        _ => event,
    }
}

fn invalid_stream_event(message: &'static str) -> ModelStreamEvent {
    ModelStreamEvent::Failed {
        error: ErrorPayload::new(WireErrorCode::InvalidResponse, message),
    }
}

fn route_result(
    result: ResultMsg,
    pending: &mut HashMap<String, PendingRequest>,
    cancelled: &mut CancelledRequests,
) -> Result<(), PeerError> {
    if result.kind != ResultKind::Invoke {
        return Err(PeerError::UnexpectedMessage("invoke result"));
    }
    if cancelled.contains(&result.id) {
        cancelled.remove(&result.id);
        return Ok(());
    }
    let Some(request) = pending.remove(&result.id) else {
        return Err(PeerError::Protocol(format!(
            "result references unknown request {}",
            result.id
        )));
    };
    let response = if result.success {
        result
            .output
            .ok_or_else(|| PeerError::Protocol("successful invoke result has no output".into()))
    } else {
        let error = result.error.unwrap_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::InvalidResponse,
                "failed invoke result has no error payload",
            )
        });
        if error.code_enum().is_none() {
            tracing::warn!(code = %error.code, "peer returned an unknown S5R wire error code");
        }
        Err(PeerError::Remote(error))
    };
    match request {
        PendingRequest::Unary {
            response: sender, ..
        } => match response {
            Ok(output) => {
                let _ = sender.send(Ok(output));
            },
            Err(PeerError::Remote(error)) => {
                let _ = sender.send(Err(error));
            },
            Err(error) => return Err(error),
        },
        PendingRequest::Stream(stream) => match response {
            Err(PeerError::Remote(error)) => {
                set_stream_failure(&stream.failure, error);
            },
            Ok(_) => {
                set_stream_failure(
                    &stream.failure,
                    ErrorPayload::new(
                        WireErrorCode::InvalidResponse,
                        "stream invoke returned a unary result",
                    ),
                );
            },
            Err(error) => return Err(error),
        },
    }
    Ok(())
}

fn route_stream(
    stream: StreamMsg,
    pending: &mut HashMap<String, PendingRequest>,
    cancelled: &mut CancelledRequests,
    control_tx: &mpsc::UnboundedSender<ControlCommand>,
) -> Result<(), PeerError> {
    if cancelled.contains(&stream.id) {
        if stream.event.is_terminal() {
            cancelled.remove(&stream.id);
        }
        return Ok(());
    }
    let Some(PendingRequest::Stream(request)) = pending.get_mut(&stream.id) else {
        return Err(PeerError::Protocol(format!(
            "stream event references unknown stream {}",
            stream.id
        )));
    };
    let valid = match &stream.event {
        ModelStreamEvent::Started if !request.started => {
            request.started = true;
            true
        },
        ModelStreamEvent::Failed { .. } => true,
        ModelStreamEvent::Started => false,
        _ => request.started,
    };
    if !valid {
        let failure = ErrorPayload::new(
            WireErrorCode::InvalidResponse,
            "stream event ordering is invalid",
        );
        set_stream_failure(&request.failure, failure);
        let _ = control_tx.send(ControlCommand::Cancel {
            id: stream.id.clone(),
            reason: "invalid_stream_order",
        });
        pending.remove(&stream.id);
        return Ok(());
    }
    let terminal = stream.event.is_terminal();
    if let Err(error) = request.events.try_send(stream.event) {
        let failure = ErrorPayload::new(
            WireErrorCode::PeerOverloaded,
            format!("stream forwarding queue is full: {error}"),
        );
        set_stream_failure(&request.failure, failure);
        let _ = control_tx.send(ControlCommand::Cancel {
            id: stream.id.clone(),
            reason: "stream_forward_queue_full",
        });
        pending.remove(&stream.id);
        return Ok(());
    }
    if terminal {
        pending.remove(&stream.id);
    }
    Ok(())
}

async fn forward_stream(
    id: String,
    mut input: mpsc::Receiver<ModelStreamEvent>,
    output: mpsc::Sender<ModelStreamEvent>,
    failure: Arc<Mutex<Option<ErrorPayload>>>,
    control_tx: mpsc::UnboundedSender<ControlCommand>,
) {
    while let Some(event) = input.recv().await {
        if timeout(STREAM_BACKPRESSURE_TIMEOUT, output.send(event))
            .await
            .is_err()
        {
            tracing::warn!(stream_id = %id, "S5R stream exceeded its backpressure deadline");
            set_stream_failure(
                &failure,
                ErrorPayload::new(
                    WireErrorCode::BackpressureTimeout,
                    "stream consumer did not release capacity before the backpressure deadline",
                ),
            );
            let _ = control_tx.send(ControlCommand::Cancel {
                id: id.clone(),
                reason: "backpressure_timeout",
            });
            break;
        }
    }
}

fn validate_parent(
    message: &InvokeMsg,
    inbound: &HashMap<String, CancellationToken>,
) -> Result<(), ErrorPayload> {
    if let Some(parent) = &message.parent_invoke_id
        && !inbound.contains_key(parent)
    {
        return Err(ErrorPayload::new(
            WireErrorCode::UnknownParentInvoke,
            format!("parent invoke {parent} is not active"),
        ));
    }
    Ok(())
}

fn validate_inbound_features(
    request: &InvokeMsg,
    features: &BTreeSet<FeatureName>,
    pending: &HashMap<String, PendingRequest>,
) -> Result<(), ErrorPayload> {
    if request.operation.is_empty() {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidRequest,
            "operation must not be empty",
        ));
    }
    if let Some(parent) = &request.parent_invoke_id {
        if !features.contains(&FeatureName::nested_invoke_v1()) {
            return Err(ErrorPayload::new(
                WireErrorCode::UnsupportedFeature,
                "nested invoke was not negotiated",
            ));
        }
        if !pending.contains_key(parent) {
            return Err(ErrorPayload::new(
                WireErrorCode::UnknownParentInvoke,
                format!("parent invoke {parent} is not active"),
            ));
        }
    }
    if request.stream && !features.contains(&FeatureName::model_stream_v1()) {
        return Err(ErrorPayload::new(
            WireErrorCode::UnsupportedFeature,
            "model stream was not negotiated",
        ));
    }
    Ok(())
}

fn failed_result(id: String, code: WireErrorCode, message: &'static str) -> WireMessage {
    WireMessage::Result(ResultMsg {
        id,
        kind: ResultKind::Invoke,
        success: false,
        output: None,
        error: Some(ErrorPayload::new(code, message)),
    })
}

async fn write_message<T>(transport: &T, message: &WireMessage) -> Result<(), PeerError>
where
    T: FrameTransport + ?Sized,
{
    let payload = encode_wire_message(message)?;
    crate::frame::write_traced_frame(transport, &payload).await?;
    Ok(())
}

fn transport_payload(error: &PeerError) -> ErrorPayload {
    ErrorPayload::new(WireErrorCode::Transport, error.to_string())
}

fn set_stream_failure(failure: &Mutex<Option<ErrorPayload>>, error: ErrorPayload) {
    let mut failure = failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if failure.is_none() {
        *failure = Some(error);
    }
}

struct CancelOnDrop {
    id: String,
    control_tx: mpsc::UnboundedSender<ControlCommand>,
    armed: bool,
}

impl CancelOnDrop {
    fn new(id: String, control_tx: mpsc::UnboundedSender<ControlCommand>) -> Self {
        Self {
            id,
            control_tx,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.control_tx.send(ControlCommand::Cancel {
                id: self.id.clone(),
                reason: "caller_dropped",
            });
        }
    }
}

pub struct PeerStream {
    id: String,
    stream: TerminalStream,
    control_tx: mpsc::UnboundedSender<ControlCommand>,
    terminal: bool,
    started_at: Instant,
    first_delta_observed: bool,
}

impl Stream for PeerStream {
    type Item = ModelStreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminal {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.stream).poll_next(context) {
            Poll::Ready(Some(event)) => {
                if !self.first_delta_observed
                    && matches!(
                        &event,
                        ModelStreamEvent::ContentDelta { .. }
                            | ModelStreamEvent::ThinkingDelta { .. }
                            | ModelStreamEvent::ToolCallStart { .. }
                            | ModelStreamEvent::ToolCallDelta { .. }
                    )
                {
                    self.first_delta_observed = true;
                    tracing::debug!(
                        stream_id = %self.id,
                        ttft_ms = self.started_at.elapsed().as_millis(),
                        "S5R stream delivered its first delta"
                    );
                }
                if event.is_terminal() {
                    self.terminal = true;
                }
                Poll::Ready(Some(event))
            },
            Poll::Ready(None) => {
                self.terminal = true;
                Poll::Ready(None)
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for PeerStream {
    fn drop(&mut self) {
        if !self.terminal {
            let _ = self.control_tx.send(ControlCommand::Cancel {
                id: self.id.clone(),
                reason: "stream_dropped",
            });
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InvokeError {
    #[error("local invoke rejected: {0}")]
    Local(ErrorPayload),
    #[error("remote invoke failed: {0}")]
    Remote(ErrorPayload),
    #[error("S5R peer driver is not running")]
    DriverUnavailable,
    #[error("S5R peer closed before the invoke completed")]
    PeerClosed,
}

#[cfg(test)]
mod tests {
    use std::{io, sync::atomic::AtomicBool};

    use futures_util::StreamExt;
    use tokio::sync::{Mutex as AsyncMutex, Notify};

    use super::*;
    use crate::{
        Peer, PeerHandshake, PeerInfo,
        frame::FrameError,
        protocol::{FeatureName, ModelStreamEvent},
    };

    struct ChannelTransport {
        inbound: AsyncMutex<mpsc::Receiver<Vec<u8>>>,
        outbound: mpsc::Sender<Vec<u8>>,
    }

    impl ChannelTransport {
        fn pair() -> (Self, Self) {
            let (left_tx, left_rx) = mpsc::channel(64);
            let (right_tx, right_rx) = mpsc::channel(64);
            (
                Self {
                    inbound: AsyncMutex::new(left_rx),
                    outbound: right_tx,
                },
                Self {
                    inbound: AsyncMutex::new(right_rx),
                    outbound: left_tx,
                },
            )
        }
    }

    #[async_trait::async_trait]
    impl FrameTransport for ChannelTransport {
        async fn read_frame(&self) -> Result<Vec<u8>, FrameError> {
            self.inbound.lock().await.recv().await.ok_or_else(|| {
                FrameError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "in-memory peer closed",
                ))
            })
        }

        async fn write_frame(&self, payload: &[u8]) -> Result<(), FrameError> {
            self.outbound.send(payload.to_vec()).await.map_err(|_| {
                FrameError::Io(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "in-memory peer closed",
                ))
            })
        }
    }

    struct HostHandler;

    #[async_trait::async_trait]
    impl PeerInvokeHandler<ChannelTransport> for HostHandler {
        async fn invoke(
            &self,
            invocation: InboundInvoke<ChannelTransport>,
        ) -> Result<InvocationResponse, ErrorPayload> {
            match invocation.request.operation.as_str() {
                "host.echo" => Ok(InvocationResponse::Unary(invocation.request.input)),
                _ => Err(ErrorPayload::new(
                    WireErrorCode::UnknownCapability,
                    "unknown host operation",
                )),
            }
        }
    }

    struct WorkerHandler {
        cancellation_observed: Arc<AtomicBool>,
        cancellation_notify: Arc<Notify>,
        invocation_started: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl PeerInvokeHandler<ChannelTransport> for WorkerHandler {
        async fn invoke(
            &self,
            invocation: InboundInvoke<ChannelTransport>,
        ) -> Result<InvocationResponse, ErrorPayload> {
            match invocation.request.operation.as_str() {
                "worker.nested" => invocation
                    .nested
                    .invoke("host.echo", invocation.request.input)
                    .await
                    .map(InvocationResponse::Unary)
                    .map_err(|error| {
                        ErrorPayload::new(WireErrorCode::NestedFailed, error.to_string())
                    }),
                "worker.stream" => {
                    let (sender, receiver) = mpsc::channel(4);
                    for event in [
                        ModelStreamEvent::Started,
                        ModelStreamEvent::ContentDelta {
                            content: "hello".into(),
                        },
                        ModelStreamEvent::Completed {
                            output: serde_json::json!({ "text": "hello" }),
                        },
                    ] {
                        sender.try_send(event).unwrap();
                    }
                    Ok(InvocationResponse::Stream(model_event_stream(receiver)))
                },
                "worker.wait" => {
                    let (sender, receiver) = mpsc::channel(1);
                    sender.try_send(ModelStreamEvent::Started).unwrap();
                    let cancellation = invocation.cancellation.clone();
                    let observed = Arc::clone(&self.cancellation_observed);
                    let notify = Arc::clone(&self.cancellation_notify);
                    tokio::spawn(async move {
                        cancellation.cancelled().await;
                        observed.store(true, Ordering::SeqCst);
                        notify.notify_one();
                        drop(sender);
                    });
                    Ok(InvocationResponse::Stream(model_event_stream(receiver)))
                },
                "worker.cancel_unary" => {
                    self.invocation_started.notify_one();
                    invocation.cancellation.cancelled().await;
                    self.cancellation_notify.notify_one();
                    Err(ErrorPayload::new(
                        WireErrorCode::Cancelled,
                        "invocation cancelled",
                    ))
                },
                _ => Err(ErrorPayload::new(
                    WireErrorCode::UnknownHandler,
                    "unknown worker operation",
                )),
            }
        }
    }

    fn peer_info(name: &str, role: &str) -> PeerInfo {
        PeerInfo {
            name: name.into(),
            role: role.into(),
            version: None,
        }
    }

    #[tokio::test]
    async fn ready_peer_routes_nested_streaming_and_drop_cancellation() {
        let (host_transport, worker_transport) = ChannelTransport::pair();
        let host = Peer::new(host_transport, peer_info("host", "host"));
        let worker = Peer::new(worker_transport, peer_info("worker", "extension"));
        let features = BTreeSet::from([
            FeatureName::nested_invoke_v1(),
            FeatureName::model_stream_v1(),
        ]);
        let mut handshake = PeerHandshake::new("initialize-1");
        handshake.supported_features = features.clone();
        handshake.required_features = features.clone();
        let (host, worker) = tokio::join!(
            host.initialize(handshake),
            worker.accept(
                features,
                BTreeSet::new(),
                Vec::new(),
                Vec::new(),
                Value::Null,
            )
        );
        let (host_handle, host_driver) = host.unwrap().into_runtime();
        let (_, worker_driver) = worker.unwrap().into_runtime();
        assert_eq!(host_handle.remote_peer().name, "worker");
        assert!(host_handle.remote_handlers().is_empty());
        assert!(host_handle.remote_capabilities().is_empty());
        assert_eq!(host_handle.remote_metadata(), &Value::Null);

        let cancellation_observed = Arc::new(AtomicBool::new(false));
        let cancellation_notify = Arc::new(Notify::new());
        let invocation_started = Arc::new(Notify::new());
        let host_shutdown = CancellationToken::new();
        let worker_shutdown = CancellationToken::new();
        let host_task =
            tokio::spawn(host_driver.run_until(Arc::new(HostHandler), host_shutdown.clone()));
        let worker_task = tokio::spawn(worker_driver.run_until(
            Arc::new(WorkerHandler {
                cancellation_observed: Arc::clone(&cancellation_observed),
                cancellation_notify: Arc::clone(&cancellation_notify),
                invocation_started: Arc::clone(&invocation_started),
            }),
            worker_shutdown.clone(),
        ));

        let nested = host_handle
            .invoke("worker.nested", serde_json::json!({ "value": 7 }))
            .await
            .unwrap();
        assert_eq!(nested, serde_json::json!({ "value": 7 }));

        let mut stream = host_handle
            .invoke_stream("worker.stream", Value::Null)
            .await
            .unwrap();
        assert!(matches!(
            stream.next().await,
            Some(ModelStreamEvent::Started)
        ));
        assert!(matches!(
            stream.next().await,
            Some(ModelStreamEvent::ContentDelta { content }) if content == "hello"
        ));
        assert!(matches!(
            stream.next().await,
            Some(ModelStreamEvent::Completed { .. })
        ));
        assert!(stream.next().await.is_none());

        let mut cancellable = host_handle
            .invoke_stream("worker.wait", Value::Null)
            .await
            .unwrap();
        assert!(matches!(
            cancellable.next().await,
            Some(ModelStreamEvent::Started)
        ));
        drop(cancellable);
        timeout(Duration::from_secs(1), cancellation_notify.notified())
            .await
            .unwrap();
        assert!(cancellation_observed.load(Ordering::SeqCst));

        let cancel_handle = host_handle.clone();
        let cancelled = tokio::spawn(async move {
            cancel_handle
                .invoke("worker.cancel_unary", Value::Null)
                .await
        });
        timeout(Duration::from_secs(1), invocation_started.notified())
            .await
            .unwrap();
        cancelled.abort();
        let _ = cancelled.await;
        timeout(Duration::from_secs(1), cancellation_notify.notified())
            .await
            .unwrap();

        let after_cancel = host_handle
            .invoke("worker.nested", serde_json::json!({ "still_open": true }))
            .await
            .unwrap();
        assert_eq!(after_cancel, serde_json::json!({ "still_open": true }));

        let unconsumed = host_handle
            .invoke_stream("worker.stream", Value::Null)
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while host_handle.outbound_permits.available_permits() != MAX_IN_FLIGHT_REQUESTS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(unconsumed);

        let saturated = Arc::clone(&host_handle.outbound_permits)
            .acquire_many_owned(MAX_IN_FLIGHT_REQUESTS as u32)
            .await
            .unwrap();
        assert!(matches!(
            host_handle
                .nested("active-parent")
                .invoke("worker.nested", Value::Null)
                .await,
            Err(InvokeError::Local(error))
                if error.code == WireErrorCode::PeerOverloaded.as_str()
        ));
        let blocked_handle = host_handle.clone();
        let mut blocked = Box::pin(blocked_handle.invoke(
            "worker.nested",
            serde_json::json!({ "after_capacity": true }),
        ));
        assert!(
            timeout(Duration::from_millis(20), &mut blocked)
                .await
                .is_err()
        );
        drop(saturated);
        assert_eq!(
            timeout(Duration::from_secs(1), blocked)
                .await
                .unwrap()
                .unwrap(),
            serde_json::json!({ "after_capacity": true })
        );

        host_shutdown.cancel();
        worker_shutdown.cancel();
        timeout(Duration::from_secs(1), host_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(1), worker_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
