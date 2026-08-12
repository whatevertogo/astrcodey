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
        ErrorPayload, FeatureName, InvokeMsg, ModelStreamEvent, ResultKind, ResultMsg, StreamMsg,
        WireMessage, encode_wire_message, parse_wire_message,
    },
};

const COMMAND_QUEUE_CAPACITY: usize = 256;
const WRITE_QUEUE_CAPACITY: usize = 256;
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
        written: oneshot::Sender<Result<(), ErrorPayload>>,
        caller_cancellation: CancellationToken,
        permit: OwnedSemaphorePermit,
    },
    InvokeStream {
        message: InvokeMsg,
        output: mpsc::Sender<ModelStreamEvent>,
        failure: Arc<Mutex<Option<ErrorPayload>>>,
        written: oneshot::Sender<Result<(), ErrorPayload>>,
        caller_cancellation: CancellationToken,
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

struct WriteRequest {
    message: WireMessage,
    written: Option<oneshot::Sender<Result<(), ErrorPayload>>>,
}

/// Sole post-handshake frame writer. FIFO ordering prevents a cancel from overtaking its invoke.
#[derive(Clone)]
struct WritePump {
    sender: mpsc::Sender<WriteRequest>,
}

impl WritePump {
    fn try_write(&self, message: WireMessage) -> Result<(), PeerError> {
        self.sender
            .try_send(WriteRequest {
                message,
                written: None,
            })
            .map_err(write_queue_error)
    }

    fn try_write_with_receipt(
        &self,
        message: WireMessage,
        written: oneshot::Sender<Result<(), ErrorPayload>>,
    ) -> bool {
        match self.sender.try_send(WriteRequest {
            message,
            written: Some(written),
        }) {
            Ok(()) => true,
            Err(error) => {
                let (request, error) = write_queue_rejection(error);
                if let Some(written) = request.written {
                    let _ = written.send(Err(error));
                }
                false
            },
        }
    }

    async fn write(&self, message: WireMessage) -> Result<(), PeerError> {
        let (written_tx, written_rx) = oneshot::channel();
        self.sender
            .send(WriteRequest {
                message,
                written: Some(written_tx),
            })
            .await
            .map_err(|_| PeerError::Protocol("peer write pump is unavailable".into()))?;
        written_rx
            .await
            .map_err(|_| PeerError::Protocol("peer write pump stopped before completion".into()))?
            .map_err(|error| PeerError::Protocol(format!("peer frame write failed: {error}")))
    }
}

/// Cloneable request surface for a running [`PeerDriver`].
pub struct PeerHandle {
    state: Arc<Ready>,
    command_tx: mpsc::Sender<DriverCommand>,
    control_tx: mpsc::UnboundedSender<ControlCommand>,
    next_request_id: Arc<AtomicU64>,
    outbound_permits: Arc<Semaphore>,
    parent_invoke_id: Option<Arc<str>>,
}

impl Clone for PeerHandle {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
            command_tx: self.command_tx.clone(),
            control_tx: self.control_tx.clone(),
            next_request_id: Arc::clone(&self.next_request_id),
            outbound_permits: Arc::clone(&self.outbound_permits),
            parent_invoke_id: self.parent_invoke_id.clone(),
        }
    }
}

impl PeerHandle {
    pub fn host_supports(&self, operation: &str) -> bool {
        self.state
            .host_operations
            .iter()
            .any(|supported| supported == operation)
    }

    pub fn nested(&self, parent_invoke_id: impl Into<String>) -> Self {
        let mut nested = self.clone();
        nested.parent_invoke_id = Some(Arc::from(parent_invoke_id.into()));
        nested
    }

    pub async fn invoke(
        &self,
        operation: impl Into<String>,
        input: Value,
    ) -> Result<Value, InvokeError> {
        let id = self.allocate_request_id();
        self.invoke_with_id_and_parent(id, operation.into(), input)
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
        self.invoke_with_id_and_parent(id.into(), operation.into(), input)
            .await
    }

    pub async fn invoke_stream(
        &self,
        operation: impl Into<String>,
        input: Value,
    ) -> Result<PeerStream, InvokeError> {
        self.invoke_stream_with_parent(operation.into(), input)
            .await
    }

    async fn invoke_with_id_and_parent(
        &self,
        id: String,
        operation: String,
        input: Value,
    ) -> Result<Value, InvokeError> {
        let parent_invoke_id = self.parent_invoke_id.as_deref();
        self.validate_outbound(&id, &operation, parent_invoke_id, false)?;
        let permit = self
            .acquire_outbound_permit(parent_invoke_id.is_some())
            .await?;
        let caller_cancellation = CancellationToken::new();
        let mut cancel = CancelOnDrop::new(
            id.clone(),
            self.control_tx.clone(),
            caller_cancellation.clone(),
        );
        let (response_tx, response_rx) = oneshot::channel();
        let (written_tx, written_rx) = oneshot::channel();
        self.command_tx
            .send(DriverCommand::Invoke {
                message: InvokeMsg {
                    id: id.clone(),
                    operation,
                    input,
                    stream: false,
                    parent_invoke_id: parent_invoke_id.map(str::to_owned),
                },
                response: response_tx,
                written: written_tx,
                caller_cancellation,
                permit,
            })
            .await
            .map_err(|_| InvokeError::DriverUnavailable)?;
        written_rx
            .await
            .map_err(|_| InvokeError::DriverUnavailable)?
            .map_err(InvokeError::Local)?;

        let result = response_rx.await.map_err(|_| InvokeError::PeerClosed)?;
        cancel.disarm();
        result.map_err(InvokeError::Remote)
    }

    async fn invoke_stream_with_parent(
        &self,
        operation: String,
        input: Value,
    ) -> Result<PeerStream, InvokeError> {
        let id = self.allocate_request_id();
        let parent_invoke_id = self.parent_invoke_id.as_deref();
        self.validate_outbound(&id, &operation, parent_invoke_id, true)?;
        let permit = self
            .acquire_outbound_permit(parent_invoke_id.is_some())
            .await?;
        let caller_cancellation = CancellationToken::new();
        let mut cancel = CancelOnDrop::new(
            id.clone(),
            self.control_tx.clone(),
            caller_cancellation.clone(),
        );
        let (output_tx, output_rx) = mpsc::channel(STREAM_BUFFER_CAPACITY);
        let failure = Arc::new(Mutex::new(None));
        let (written_tx, written_rx) = oneshot::channel();
        self.command_tx
            .send(DriverCommand::InvokeStream {
                message: InvokeMsg {
                    id: id.clone(),
                    operation,
                    input,
                    stream: true,
                    parent_invoke_id: parent_invoke_id.map(str::to_owned),
                },
                output: output_tx,
                failure: Arc::clone(&failure),
                written: written_tx,
                caller_cancellation,
                permit,
            })
            .await
            .map_err(|_| InvokeError::DriverUnavailable)?;
        written_rx
            .await
            .map_err(|_| InvokeError::DriverUnavailable)?
            .map_err(InvokeError::Local)?;
        cancel.disarm();
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

/// Cancellation state for one inbound invocation, including the peer-provided reason.
#[derive(Clone, Default)]
pub struct InvocationCancellation {
    token: CancellationToken,
    reason: Arc<Mutex<Option<String>>>,
}

impl InvocationCancellation {
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }

    pub fn reason(&self) -> Option<String> {
        self.reason
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.token.clone()
    }

    fn cancel(&self, reason: impl Into<String>) {
        let mut stored_reason = self
            .reason
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if stored_reason.is_none() {
            *stored_reason = Some(reason.into());
        }
        drop(stored_reason);
        self.token.cancel();
    }
}

/// A single inbound request plus its cancellation and nested-call context.
pub struct InboundInvoke {
    pub request: InvokeMsg,
    pub cancellation: InvocationCancellation,
    pub nested: PeerHandle,
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
pub trait PeerInvokeHandler: Send + Sync {
    async fn invoke(&self, invocation: InboundInvoke) -> Result<InvocationResponse, ErrorPayload>;
}

/// Explicit owner of peer reads, pending calls, inbound tasks, and cancellation state.
pub struct PeerDriver<T> {
    transport: Arc<T>,
    state: Arc<Ready>,
    command_rx: mpsc::Receiver<DriverCommand>,
    control_rx: mpsc::UnboundedReceiver<ControlCommand>,
    write_pump: WritePump,
    write_rx: Option<mpsc::Receiver<WriteRequest>>,
    handle: PeerHandle,
    inbound_permits: Arc<Semaphore>,
}

pub(crate) fn runtime_parts<T>(transport: Arc<T>, state: Ready) -> (PeerHandle, PeerDriver<T>)
where
    T: FrameTransport + 'static,
{
    let state = Arc::new(state);
    let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let (write_tx, write_rx) = mpsc::channel(WRITE_QUEUE_CAPACITY);
    let outbound_permits = Arc::new(Semaphore::new(MAX_IN_FLIGHT_REQUESTS));
    let handle = PeerHandle {
        state: Arc::clone(&state),
        command_tx,
        control_tx,
        next_request_id: Arc::new(AtomicU64::new(1)),
        outbound_permits,
        parent_invoke_id: None,
    };
    let driver = PeerDriver {
        transport,
        state,
        command_rx,
        control_rx,
        write_pump: WritePump { sender: write_tx },
        write_rx: Some(write_rx),
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
        H: PeerInvokeHandler + 'static,
    {
        self.run_until(handler, CancellationToken::new()).await
    }

    pub async fn run_until<H>(
        mut self,
        handler: Arc<H>,
        shutdown: CancellationToken,
    ) -> Result<(), PeerError>
    where
        H: PeerInvokeHandler + 'static,
    {
        let Some(write_rx) = self.write_rx.take() else {
            return Err(PeerError::Protocol(
                "peer driver can only be run once".into(),
            ));
        };
        let mut writer = JoinSet::new();
        writer.spawn(run_write_pump(Arc::clone(&self.transport), write_rx));
        let mut pending = HashMap::<String, PendingRequest>::new();
        let mut cancelled = CancelledRequests::default();
        let mut inbound = HashMap::<String, InvocationCancellation>::new();
        let mut tasks = JoinSet::<Result<TaskCompletion, PeerError>>::new();
        let result = loop {
            tokio::select! {
                () = shutdown.cancelled() => break Ok(()),
                Some(control) = self.control_rx.recv() => {
                    if let Err(error) = self
                        .handle_control(control, &mut pending, &mut cancelled)
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
                    ) {
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
                Some(joined) = writer.join_next() => {
                    match joined {
                        Ok(Ok(())) => {
                            break Err(PeerError::Protocol(
                                "peer write pump stopped unexpectedly".into(),
                            ));
                        }
                        Ok(Err(error)) => break Err(error),
                        Err(error) => {
                            break Err(PeerError::Protocol(format!(
                                "peer write pump task failed: {error}"
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
                    ) {
                        break Err(error);
                    }
                }
            }
        };

        for cancellation in inbound.values() {
            cancellation.cancel("peer_driver_stopped");
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
        writer.abort_all();
        while writer.join_next().await.is_some() {}
        result
    }

    fn handle_command(
        &self,
        command: DriverCommand,
        pending: &mut HashMap<String, PendingRequest>,
        inbound: &HashMap<String, InvocationCancellation>,
        tasks: &mut JoinSet<Result<TaskCompletion, PeerError>>,
    ) -> Result<(), PeerError> {
        match command {
            DriverCommand::Invoke {
                message,
                response,
                written,
                caller_cancellation,
                permit,
            } => {
                if caller_cancellation.is_cancelled() {
                    return Ok(());
                }
                if let Err(error) = validate_parent(&message, inbound) {
                    let _ = written.send(Err(error));
                    return Ok(());
                }
                if pending.contains_key(&message.id) {
                    let _ = written.send(Err(ErrorPayload::new(
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
                self.start_invoke_write(id, message, written, pending);
            },
            DriverCommand::InvokeStream {
                message,
                output,
                failure,
                written,
                caller_cancellation,
                permit,
            } => {
                if caller_cancellation.is_cancelled() {
                    return Ok(());
                }
                if let Err(error) = validate_parent(&message, inbound) {
                    let _ = written.send(Err(error));
                    return Ok(());
                }
                if pending.contains_key(&message.id) {
                    let _ = written.send(Err(ErrorPayload::new(
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
                self.start_invoke_write(id, message, written, pending);
            },
        }
        Ok(())
    }

    fn start_invoke_write(
        &self,
        id: String,
        message: InvokeMsg,
        written: oneshot::Sender<Result<(), ErrorPayload>>,
        pending: &mut HashMap<String, PendingRequest>,
    ) {
        if !self
            .write_pump
            .try_write_with_receipt(WireMessage::Invoke(message), written)
        {
            pending.remove(&id);
        }
    }

    fn handle_control(
        &self,
        control: ControlCommand,
        pending: &mut HashMap<String, PendingRequest>,
        cancelled: &mut CancelledRequests,
    ) -> Result<(), PeerError> {
        match control {
            ControlCommand::Cancel { id, reason } => {
                if pending.remove(&id).is_none() {
                    return Ok(());
                }
                cancelled.insert(id.clone());
                self.write_pump
                    .try_write(WireMessage::Cancel(crate::protocol::CancelMsg {
                        id,
                        reason: reason.into(),
                    }))?;
            },
        }
        Ok(())
    }

    fn handle_message<H>(
        &self,
        message: WireMessage,
        handler: Arc<H>,
        pending: &mut HashMap<String, PendingRequest>,
        cancelled: &mut CancelledRequests,
        inbound: &mut HashMap<String, InvocationCancellation>,
        tasks: &mut JoinSet<Result<TaskCompletion, PeerError>>,
    ) -> Result<(), PeerError>
    where
        H: PeerInvokeHandler + 'static,
    {
        match message {
            WireMessage::Result(result) => route_result(result, pending, cancelled),
            WireMessage::Stream(stream) => {
                route_stream(stream, pending, cancelled, &self.handle.control_tx)
            },
            WireMessage::Cancel(cancel) => {
                if let Some(cancellation) = inbound.get(&cancel.id) {
                    cancellation.cancel(cancel.reason);
                }
                Ok(())
            },
            WireMessage::Invoke(request) => {
                if inbound.contains_key(&request.id) {
                    self.write_pump.try_write(failed_result(
                        request.id,
                        WireErrorCode::DuplicateRequestId,
                        "duplicate inbound request id",
                    ))?;
                    return Ok(());
                }
                if let Err(error) =
                    validate_inbound_features(&request, &self.state.negotiated_features, pending)
                {
                    self.write_pump
                        .try_write(WireMessage::Result(ResultMsg::failure(
                            request.id,
                            ResultKind::Invoke,
                            error,
                        )))?;
                    return Ok(());
                }
                let permit = match Arc::clone(&self.inbound_permits).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        self.write_pump.try_write(failed_result(
                            request.id,
                            WireErrorCode::PeerOverloaded,
                            "peer has reached its in-flight request limit",
                        ))?;
                        return Ok(());
                    },
                };
                let cancellation = InvocationCancellation::default();
                inbound.insert(request.id.clone(), cancellation.clone());
                let nested = self.handle.nested(request.id.clone());
                let write_pump = self.write_pump.clone();
                tasks.spawn(async move {
                    run_inbound(
                        write_pump,
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
            WireMessage::Initialize(_) | WireMessage::Activate(_) => Err(
                PeerError::UnexpectedMessage("runtime invoke, result, stream, or cancel message"),
            ),
        }
    }
}

async fn run_inbound<H>(
    write_pump: WritePump,
    handler: Arc<H>,
    invocation: InboundInvoke,
    _permit: OwnedSemaphorePermit,
) -> Result<String, PeerError>
where
    H: PeerInvokeHandler + 'static,
{
    let id = invocation.request.id.clone();
    let wants_stream = invocation.request.stream;
    let cancellation = invocation.cancellation.clone();
    let response = tokio::select! {
        biased;
        response = handler.invoke(invocation) => response,
        () = cancellation.cancelled() => return Ok(id),
    };
    match response {
        Ok(InvocationResponse::Unary(output)) if !wants_stream => {
            write_pump
                .write(WireMessage::Result(ResultMsg::success(
                    id.clone(),
                    ResultKind::Invoke,
                    output,
                )))
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
                write_pump
                    .write(WireMessage::Stream(StreamMsg {
                        id: id.clone(),
                        event,
                    }))
                    .await?;
                if terminal {
                    break;
                }
            }
        },
        Ok(_) => {
            write_pump
                .write(failed_result(
                    id.clone(),
                    WireErrorCode::InvalidResponse,
                    "handler response mode does not match invoke mode",
                ))
                .await?;
        },
        Err(error) => {
            write_pump
                .write(WireMessage::Result(ResultMsg::failure(
                    id.clone(),
                    ResultKind::Invoke,
                    error,
                )))
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
    if result.kind() != ResultKind::Invoke {
        return Err(PeerError::UnexpectedMessage("invoke result"));
    }
    if cancelled.contains(result.id()) {
        cancelled.remove(result.id());
        return Ok(());
    }
    let Some(request) = pending.remove(result.id()) else {
        return Err(PeerError::Protocol(format!(
            "result references unknown request {}",
            result.id()
        )));
    };
    let response = match result {
        ResultMsg::Success { output, .. } => Ok(output),
        ResultMsg::Failure { error, .. } => {
            if error.code_enum().is_none() {
                tracing::warn!(code = %error.code, "peer returned an unknown S5R wire error code");
            }
            Err(PeerError::Remote(error))
        },
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
    inbound: &HashMap<String, InvocationCancellation>,
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
    WireMessage::Result(ResultMsg::failure(
        id,
        ResultKind::Invoke,
        ErrorPayload::new(code, message),
    ))
}

async fn run_write_pump<T>(
    transport: Arc<T>,
    mut receiver: mpsc::Receiver<WriteRequest>,
) -> Result<(), PeerError>
where
    T: FrameTransport + 'static,
{
    while let Some(WriteRequest { message, written }) = receiver.recv().await {
        match write_message(transport.as_ref(), &message).await {
            Ok(()) => {
                if let Some(written) = written {
                    let _ = written.send(Ok(()));
                }
            },
            Err(error) => {
                if let Some(written) = written {
                    let _ = written.send(Err(transport_payload(&error)));
                }
                return Err(error);
            },
        }
    }
    Ok(())
}

fn write_queue_error(error: mpsc::error::TrySendError<WriteRequest>) -> PeerError {
    let message = match error {
        mpsc::error::TrySendError::Full(_) => "peer write queue is full",
        mpsc::error::TrySendError::Closed(_) => "peer write pump is unavailable",
    };
    PeerError::Protocol(message.into())
}

fn write_queue_rejection(
    error: mpsc::error::TrySendError<WriteRequest>,
) -> (WriteRequest, ErrorPayload) {
    match error {
        mpsc::error::TrySendError::Full(request) => (
            request,
            ErrorPayload::new(WireErrorCode::PeerOverloaded, "peer write queue is full"),
        ),
        mpsc::error::TrySendError::Closed(request) => (
            request,
            ErrorPayload::new(WireErrorCode::PeerClosed, "peer write pump is unavailable"),
        ),
    }
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
    caller_cancellation: CancellationToken,
    armed: bool,
}

impl CancelOnDrop {
    fn new(
        id: String,
        control_tx: mpsc::UnboundedSender<ControlCommand>,
        caller_cancellation: CancellationToken,
    ) -> Self {
        Self {
            id,
            control_tx,
            caller_cancellation,
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
            self.caller_cancellation.cancel();
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
    use std::{
        io,
        sync::atomic::{AtomicBool, AtomicUsize},
    };

    use futures_util::StreamExt;
    use tokio::sync::{Mutex as AsyncMutex, Notify};

    use super::*;
    use crate::{
        HostInitialization, Peer, PeerInfo, WorkerInitialization,
        frame::FrameError,
        manifest::InitializeManifest,
        protocol::{FeatureName, InvokeMsg, ModelStreamEvent, WireMessage, encode_wire_message},
    };

    struct WriteGate {
        armed: AtomicBool,
        started: Notify,
        release: Semaphore,
    }

    struct DropNotify(Arc<Notify>);

    impl Drop for DropNotify {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }

    impl WriteGate {
        fn new() -> Self {
            Self {
                armed: AtomicBool::new(false),
                started: Notify::new(),
                release: Semaphore::new(0),
            }
        }

        fn arm(&self) {
            assert!(!self.armed.swap(true, Ordering::AcqRel));
        }

        async fn wait_if_armed(&self) {
            if self.armed.swap(false, Ordering::AcqRel) {
                self.started.notify_one();
                self.release.acquire().await.unwrap().forget();
            }
        }
    }

    struct ChannelTransport {
        inbound: AsyncMutex<mpsc::Receiver<Vec<u8>>>,
        outbound: mpsc::Sender<Vec<u8>>,
        write_gate: Arc<WriteGate>,
    }

    impl ChannelTransport {
        fn pair() -> (Self, Self) {
            let (left_tx, left_rx) = mpsc::channel(64);
            let (right_tx, right_rx) = mpsc::channel(64);
            (
                Self {
                    inbound: AsyncMutex::new(left_rx),
                    outbound: right_tx,
                    write_gate: Arc::new(WriteGate::new()),
                },
                Self {
                    inbound: AsyncMutex::new(right_rx),
                    outbound: left_tx,
                    write_gate: Arc::new(WriteGate::new()),
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
            self.write_gate.wait_if_armed().await;
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
    impl PeerInvokeHandler for HostHandler {
        async fn invoke(
            &self,
            invocation: InboundInvoke,
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

    struct SaturatingHostHandler {
        started: Arc<AtomicUsize>,
        started_notify: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl PeerInvokeHandler for SaturatingHostHandler {
        async fn invoke(
            &self,
            invocation: InboundInvoke,
        ) -> Result<InvocationResponse, ErrorPayload> {
            if invocation.request.operation != "host.block" {
                return HostHandler.invoke(invocation).await;
            }
            self.started.fetch_add(1, Ordering::SeqCst);
            self.started_notify.notify_waiters();
            invocation.cancellation.cancelled().await;
            Err(ErrorPayload::new(
                WireErrorCode::Cancelled,
                "blocking host invocation cancelled",
            ))
        }
    }

    struct CancellationObservingHostHandler {
        started: Arc<Notify>,
        cancelled: Arc<Notify>,
        reason: Arc<Mutex<Option<String>>>,
    }

    #[async_trait::async_trait]
    impl PeerInvokeHandler for CancellationObservingHostHandler {
        async fn invoke(
            &self,
            invocation: InboundInvoke,
        ) -> Result<InvocationResponse, ErrorPayload> {
            if invocation.request.operation != "host.block" {
                return HostHandler.invoke(invocation).await;
            }
            let _stopped = DropNotify(Arc::clone(&self.cancelled));
            self.started.notify_one();
            invocation.cancellation.cancelled().await;
            *self
                .reason
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = invocation.cancellation.reason();
            Err(ErrorPayload::new(
                WireErrorCode::Cancelled,
                "nested host invocation cancelled",
            ))
        }
    }

    struct WorkerHandler {
        cancellation_observed: Arc<AtomicBool>,
        cancellation_notify: Arc<Notify>,
        invocation_started: Arc<Notify>,
    }

    struct DelayedEchoWorkerHandler {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[test]
    fn cancellation_preserves_the_first_reason() {
        let cancellation = InvocationCancellation::default();
        cancellation.cancel("caller_dropped");
        cancellation.cancel("peer_driver_stopped");

        assert!(cancellation.is_cancelled());
        assert_eq!(cancellation.reason().as_deref(), Some("caller_dropped"));
    }

    #[async_trait::async_trait]
    impl PeerInvokeHandler for DelayedEchoWorkerHandler {
        async fn invoke(
            &self,
            invocation: InboundInvoke,
        ) -> Result<InvocationResponse, ErrorPayload> {
            if invocation.request.operation != "worker.delayed_echo" {
                return Err(ErrorPayload::new(
                    WireErrorCode::UnknownHandler,
                    "unknown worker operation",
                ));
            }
            self.started.notify_one();
            self.release.notified().await;
            Ok(InvocationResponse::Unary(invocation.request.input))
        }
    }

    #[async_trait::async_trait]
    impl PeerInvokeHandler for WorkerHandler {
        async fn invoke(
            &self,
            invocation: InboundInvoke,
        ) -> Result<InvocationResponse, ErrorPayload> {
            match invocation.request.operation.as_str() {
                "worker.echo" => Ok(InvocationResponse::Unary(invocation.request.input)),
                "worker.nested" => invocation
                    .nested
                    .invoke("host.echo", invocation.request.input)
                    .await
                    .map(InvocationResponse::Unary)
                    .map_err(|error| {
                        ErrorPayload::new(WireErrorCode::NestedFailed, error.to_string())
                    }),
                "worker.nested_wait" => invocation
                    .nested
                    .invoke("host.block", invocation.request.input)
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
                    let _stopped = DropNotify(Arc::clone(&self.cancellation_notify));
                    self.invocation_started.notify_one();
                    invocation.cancellation.cancelled().await;
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

    fn peer_info(name: &str) -> PeerInfo {
        PeerInfo {
            name: name.into(),
            version: None,
        }
    }

    struct ReadyPeerPair {
        host_handle: PeerHandle,
        host_driver: PeerDriver<ChannelTransport>,
        worker_handle: PeerHandle,
        worker_driver: PeerDriver<ChannelTransport>,
        host_write_gate: Arc<WriteGate>,
        host_inbound_tx: mpsc::Sender<Vec<u8>>,
    }

    async fn ready_peer_pair(features: BTreeSet<FeatureName>) -> ReadyPeerPair {
        let (host_transport, worker_transport) = ChannelTransport::pair();
        let host_write_gate = Arc::clone(&host_transport.write_gate);
        let host_inbound_tx = worker_transport.outbound.clone();
        let host = Peer::new(host_transport, peer_info("host"));
        let worker = Peer::new(worker_transport, peer_info("worker"));
        let mut host_initialization = HostInitialization::new("initialize-1", "worker");
        host_initialization.supported_features = features.clone();
        host_initialization.required_features = features.clone();
        let mut worker_initialization = WorkerInitialization::new(InitializeManifest::default());
        worker_initialization.supported_features = features;
        let (host, worker) = tokio::join!(
            host.initialize(host_initialization),
            worker.accept(worker_initialization)
        );
        let (host, worker) = tokio::join!(
            host.unwrap().0.activate("activate-1"),
            worker.unwrap().accept_activation()
        );
        let (host_handle, host_driver) = host.unwrap().into_runtime();
        let (worker_handle, worker_driver) = worker.unwrap().into_runtime();
        ReadyPeerPair {
            host_handle,
            host_driver,
            worker_handle,
            worker_driver,
            host_write_gate,
            host_inbound_tx,
        }
    }

    #[tokio::test]
    async fn transport_eof_releases_pending_unary_and_stream_calls() {
        let ReadyPeerPair {
            host_handle,
            host_driver,
            worker_driver,
            ..
        } = ready_peer_pair(BTreeSet::from([FeatureName::model_stream_v1()])).await;
        let cancellation_observed = Arc::new(AtomicBool::new(false));
        let cancellation_notify = Arc::new(Notify::new());
        let invocation_started = Arc::new(Notify::new());
        let host_task =
            tokio::spawn(host_driver.run_until(Arc::new(HostHandler), CancellationToken::new()));
        let worker_shutdown = CancellationToken::new();
        let worker_task = tokio::spawn(worker_driver.run_until(
            Arc::new(WorkerHandler {
                cancellation_observed,
                cancellation_notify,
                invocation_started: Arc::clone(&invocation_started),
            }),
            worker_shutdown.clone(),
        ));

        let unary_handle = host_handle.clone();
        let unary = tokio::spawn(async move {
            unary_handle
                .invoke("worker.cancel_unary", Value::Null)
                .await
        });
        timeout(Duration::from_secs(1), invocation_started.notified())
            .await
            .unwrap();
        let mut stream = host_handle
            .invoke_stream("worker.wait", Value::Null)
            .await
            .unwrap();
        assert!(matches!(
            stream.next().await,
            Some(ModelStreamEvent::Started)
        ));

        worker_shutdown.cancel();
        timeout(Duration::from_secs(1), worker_task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            timeout(Duration::from_secs(1), host_task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(matches!(
            timeout(Duration::from_secs(1), unary)
                .await
                .unwrap()
                .unwrap(),
            Err(InvokeError::PeerClosed)
        ));
        assert!(matches!(
            timeout(Duration::from_secs(1), stream.next()).await.unwrap(),
            Some(ModelStreamEvent::Failed { error })
                if error.code_enum() == Some(WireErrorCode::PeerClosed)
        ));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn nested_invoke_inherits_parent_cancellation() {
        let ReadyPeerPair {
            host_handle,
            host_driver,
            worker_driver,
            ..
        } = ready_peer_pair(BTreeSet::from([FeatureName::nested_invoke_v1()])).await;
        let nested_started = Arc::new(Notify::new());
        let nested_cancelled = Arc::new(Notify::new());
        let nested_cancel_reason = Arc::new(Mutex::new(None));
        let host_shutdown = CancellationToken::new();
        let worker_shutdown = CancellationToken::new();
        let host_task = tokio::spawn(host_driver.run_until(
            Arc::new(CancellationObservingHostHandler {
                started: Arc::clone(&nested_started),
                cancelled: Arc::clone(&nested_cancelled),
                reason: Arc::clone(&nested_cancel_reason),
            }),
            host_shutdown.clone(),
        ));
        let worker_task = tokio::spawn(worker_driver.run_until(
            Arc::new(WorkerHandler {
                cancellation_observed: Arc::new(AtomicBool::new(false)),
                cancellation_notify: Arc::new(Notify::new()),
                invocation_started: Arc::new(Notify::new()),
            }),
            worker_shutdown.clone(),
        ));

        let invoke_handle = host_handle.clone();
        let outer = tokio::spawn(async move {
            invoke_handle
                .invoke("worker.nested_wait", Value::Null)
                .await
        });
        timeout(Duration::from_secs(1), nested_started.notified())
            .await
            .unwrap();
        outer.abort();
        let _ = outer.await;
        timeout(Duration::from_secs(1), nested_cancelled.notified())
            .await
            .expect("cancelling the parent must cancel its nested invoke");
        assert_eq!(
            nested_cancel_reason
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_deref(),
            Some("caller_dropped")
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

    #[tokio::test]
    async fn saturated_inbound_handlers_do_not_block_result_dispatch() {
        let ReadyPeerPair {
            host_handle,
            host_driver,
            worker_handle,
            worker_driver,
            ..
        } = ready_peer_pair(BTreeSet::new()).await;
        let started = Arc::new(AtomicUsize::new(0));
        let started_notify = Arc::new(Notify::new());
        let host_shutdown = CancellationToken::new();
        let worker_shutdown = CancellationToken::new();
        let host_task = tokio::spawn(host_driver.run_until(
            Arc::new(SaturatingHostHandler {
                started: Arc::clone(&started),
                started_notify: Arc::clone(&started_notify),
            }),
            host_shutdown.clone(),
        ));
        let worker_task = tokio::spawn(worker_driver.run_until(
            Arc::new(WorkerHandler {
                cancellation_observed: Arc::new(AtomicBool::new(false)),
                cancellation_notify: Arc::new(Notify::new()),
                invocation_started: Arc::new(Notify::new()),
            }),
            worker_shutdown.clone(),
        ));

        let mut blocked = Vec::with_capacity(MAX_IN_FLIGHT_REQUESTS);
        for _ in 0..MAX_IN_FLIGHT_REQUESTS {
            let worker_handle = worker_handle.clone();
            blocked.push(tokio::spawn(async move {
                worker_handle.invoke("host.block", Value::Null).await
            }));
        }
        timeout(Duration::from_secs(2), async {
            loop {
                let notified = started_notify.notified();
                if started.load(Ordering::SeqCst) == MAX_IN_FLIGHT_REQUESTS {
                    break;
                }
                notified.await;
            }
        })
        .await
        .unwrap();

        let echoed = timeout(
            Duration::from_secs(1),
            host_handle.invoke("worker.echo", serde_json::json!({ "routed": true })),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(echoed, serde_json::json!({ "routed": true }));

        host_shutdown.cancel();
        worker_shutdown.cancel();
        for task in blocked {
            task.abort();
        }
        let _ = timeout(Duration::from_secs(1), host_task)
            .await
            .unwrap()
            .unwrap();
        let _ = timeout(Duration::from_secs(1), worker_task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn rejection_write_backpressure_does_not_block_result_dispatch() {
        let ReadyPeerPair {
            host_handle,
            host_driver,
            worker_handle,
            worker_driver,
            host_write_gate,
            host_inbound_tx,
        } = ready_peer_pair(BTreeSet::new()).await;
        let started = Arc::new(AtomicUsize::new(0));
        let started_notify = Arc::new(Notify::new());
        let delayed_started = Arc::new(Notify::new());
        let delayed_release = Arc::new(Notify::new());
        let host_shutdown = CancellationToken::new();
        let worker_shutdown = CancellationToken::new();
        let host_task = tokio::spawn(host_driver.run_until(
            Arc::new(SaturatingHostHandler {
                started: Arc::clone(&started),
                started_notify: Arc::clone(&started_notify),
            }),
            host_shutdown.clone(),
        ));
        let worker_task = tokio::spawn(worker_driver.run_until(
            Arc::new(DelayedEchoWorkerHandler {
                started: Arc::clone(&delayed_started),
                release: Arc::clone(&delayed_release),
            }),
            worker_shutdown.clone(),
        ));

        let mut blocked = Vec::with_capacity(MAX_IN_FLIGHT_REQUESTS);
        for _ in 0..MAX_IN_FLIGHT_REQUESTS {
            let worker_handle = worker_handle.clone();
            blocked.push(tokio::spawn(async move {
                worker_handle.invoke("host.block", Value::Null).await
            }));
        }
        timeout(Duration::from_secs(2), async {
            loop {
                let notified = started_notify.notified();
                if started.load(Ordering::SeqCst) == MAX_IN_FLIGHT_REQUESTS {
                    break;
                }
                notified.await;
            }
        })
        .await
        .unwrap();

        let delayed_handle = host_handle.clone();
        let delayed = tokio::spawn(async move {
            delayed_handle
                .invoke("worker.delayed_echo", serde_json::json!({ "routed": true }))
                .await
        });
        timeout(Duration::from_secs(1), delayed_started.notified())
            .await
            .unwrap();

        host_write_gate.arm();
        let write_started = host_write_gate.started.notified();
        host_inbound_tx
            .send(
                encode_wire_message(&WireMessage::Invoke(InvokeMsg {
                    id: "overflow".into(),
                    operation: "host.block".into(),
                    input: Value::Null,
                    stream: false,
                    parent_invoke_id: None,
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        timeout(Duration::from_secs(1), write_started)
            .await
            .unwrap();

        delayed_release.notify_one();
        assert_eq!(
            timeout(Duration::from_secs(1), delayed)
                .await
                .expect("result dispatch must not wait for the rejection writer")
                .unwrap()
                .unwrap(),
            serde_json::json!({ "routed": true })
        );

        host_write_gate.release.add_permits(1);
        host_shutdown.cancel();
        worker_shutdown.cancel();
        for task in blocked {
            task.abort();
        }
        let _ = timeout(Duration::from_secs(1), host_task)
            .await
            .unwrap()
            .unwrap();
        let _ = timeout(Duration::from_secs(1), worker_task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn ready_peer_routes_nested_streaming_and_drop_cancellation() {
        let features = BTreeSet::from([
            FeatureName::nested_invoke_v1(),
            FeatureName::model_stream_v1(),
        ]);
        let ReadyPeerPair {
            host_handle,
            host_driver,
            worker_driver,
            host_write_gate,
            ..
        } = ready_peer_pair(features).await;
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

        host_write_gate.arm();
        let write_started = host_write_gate.started.notified();
        let stream_handle = host_handle.clone();
        let stream_call = tokio::spawn(async move {
            stream_handle
                .invoke_stream("worker.stream", Value::Null)
                .await
        });
        timeout(Duration::from_secs(1), write_started)
            .await
            .unwrap();
        assert!(!stream_call.is_finished());
        host_write_gate.release.add_permits(1);
        let mut stream = stream_call.await.unwrap().unwrap();
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

        host_write_gate.arm();
        let write_started = host_write_gate.started.notified();
        let cancel_handle = host_handle.clone();
        let cancelled_before_written = tokio::spawn(async move {
            cancel_handle
                .invoke("worker.cancel_unary", Value::Null)
                .await
        });
        timeout(Duration::from_secs(1), write_started)
            .await
            .unwrap();
        cancelled_before_written.abort();
        let _ = cancelled_before_written.await;
        timeout(Duration::from_secs(1), async {
            while host_handle.outbound_permits.available_permits() != MAX_IN_FLIGHT_REQUESTS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        host_write_gate.release.add_permits(1);
        timeout(Duration::from_secs(1), async {
            while host_handle.outbound_permits.available_permits() != MAX_IN_FLIGHT_REQUESTS {
                tokio::task::yield_now().await;
            }
        })
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
