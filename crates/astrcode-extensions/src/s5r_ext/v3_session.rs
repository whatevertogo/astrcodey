//! Host-side S5R 3.0 process session.

use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
    pin::Pin,
    process::Stdio,
    sync::{Arc, atomic::AtomicU32},
    task::{Context, Poll},
    time::Duration,
};

use astrcode_extension_contract::{
    FeatureName, FrameTransport, InboundInvoke, InvocationResponse, Peer, PeerHandle,
    PeerHandshake, PeerInvokeHandler, StdioFrameTransport,
    protocol::{ModelStreamEvent, PeerInfo, S5R_STACK},
};
use astrcode_extension_sdk::{
    extension::ExtensionError,
    s5r::{
        CAP_HANDLER_INVOKE, CAP_RUNTIME_PING, ErrorPayload, HandlerInvokeRequest,
        effects::HandlerResult,
    },
    tool::ExecutionMode,
};
use futures_util::Stream;
use parking_lot::{Mutex, RwLock};
use serde_json::{Map, Value, json};
use tokio::{process::Command, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use super::session_support::{
    HostInvokeState, ReentrancyGuard, StderrTaskGuard, drain_stderr, prepare_host_invoke,
};
use crate::{
    extension_manifest::{ExtensionRegistration, registration_from_s5r_metadata},
    host_router::{HostRouter, InvokeContext},
    process_supervision::{SupervisedChild, SupervisedCommand},
};

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);
const INVOKE_TIMEOUT: Duration = Duration::from_secs(120);
const PROCESS_TERMINATION_GRACE: Duration = Duration::from_secs(2);
const MAX_PARALLEL_INVOKES: u32 = 8;
const MAX_CONTINUATION_DEPTH: u32 = 16;

#[derive(Debug, thiserror::Error)]
pub(crate) enum S5rV3SessionError {
    #[error("{0}")]
    Message(String),
}

struct InvokeContextGuard {
    id: String,
    contexts: Arc<RwLock<HashMap<String, InvokeContext>>>,
}

#[derive(Clone, Default)]
struct HandlerAttribution {
    session_id: Option<Value>,
    turn_id: Option<Value>,
    tool_call_id: Option<Value>,
    working_dir: Option<Value>,
}

impl HandlerAttribution {
    fn from_event(event: &Value) -> Self {
        let input = event.get("input").and_then(Value::as_object);
        Self {
            session_id: input.and_then(|input| input.get("session_id")).cloned(),
            turn_id: input.and_then(|input| input.get("turn_id")).cloned(),
            tool_call_id: input
                .and_then(|input| input.get("tool_call_id").or_else(|| input.get("call_id")))
                .cloned(),
            working_dir: input.and_then(|input| input.get("working_dir")).cloned(),
        }
    }

    fn apply_to(&self, event: &mut Value) {
        let Some(input) = event.get_mut("input").and_then(Value::as_object_mut) else {
            return;
        };
        insert_attribution(input, "session_id", &self.session_id);
        insert_attribution(input, "turn_id", &self.turn_id);
        insert_attribution(input, "tool_call_id", &self.tool_call_id);
        insert_attribution(input, "working_dir", &self.working_dir);
    }
}

fn insert_attribution(input: &mut Map<String, Value>, field: &str, value: &Option<Value>) {
    if let Some(value) = value {
        input.insert(field.into(), value.clone());
    }
}

impl InvokeContextGuard {
    fn insert(
        id: String,
        context: InvokeContext,
        contexts: Arc<RwLock<HashMap<String, InvokeContext>>>,
    ) -> Self {
        contexts.write().insert(id.clone(), context);
        Self { id, contexts }
    }
}

impl Drop for InvokeContextGuard {
    fn drop(&mut self) {
        self.contexts.write().remove(&self.id);
    }
}

struct V3HostInvokeHandler<T> {
    state: Arc<HostInvokeState>,
    transport: std::marker::PhantomData<fn() -> T>,
}

struct GuardedModelStream {
    stream: astrcode_extension_contract::ModelEventStream,
    _reentrancy: ReentrancyGuard,
}

impl Stream for GuardedModelStream {
    type Item = ModelStreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_next(context)
    }
}

#[async_trait::async_trait]
impl<T> PeerInvokeHandler<T> for V3HostInvokeHandler<T>
where
    T: FrameTransport + 'static,
{
    async fn invoke(
        &self,
        invocation: InboundInvoke<T>,
    ) -> Result<InvocationResponse, ErrorPayload> {
        let request = invocation.request;
        let (reentrancy, context) = prepare_host_invoke(
            &self.state,
            &request.operation,
            request.parent_invoke_id.as_deref(),
            &invocation.cancellation,
        )?;
        if request.stream {
            let stream = self
                .state
                .router
                .invoke_event_stream(&request.operation, request.input, &context)
                .await?;
            return Ok(InvocationResponse::Stream(Box::pin(GuardedModelStream {
                stream,
                _reentrancy: reentrancy,
            })));
        }
        let output = self
            .state
            .router
            .invoke(&request.operation, request.input, &context)
            .await?;
        drop(reentrancy);
        Ok(InvocationResponse::Unary(output))
    }
}

/// One S5R 3.0 process generation.
pub(crate) struct S5rV3Session {
    child: Mutex<Option<SupervisedChild>>,
    stderr_task: Mutex<Option<JoinHandle<()>>>,
    driver_task: Mutex<Option<JoinHandle<Result<(), astrcode_extension_contract::PeerError>>>>,
    driver_shutdown: CancellationToken,
    handle: PeerHandle<StdioFrameTransport>,
    registration: Arc<RwLock<Option<ExtensionRegistration>>>,
    invoke_contexts: Arc<RwLock<HashMap<String, InvokeContext>>>,
    detached_invoke_context: Arc<RwLock<Option<InvokeContext>>>,
    admission: Arc<tokio::sync::Semaphore>,
}

impl S5rV3Session {
    pub(crate) async fn spawn(
        program: &str,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
        router: Arc<HostRouter>,
    ) -> Result<Arc<Self>, String> {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = SupervisedCommand::new(command)
            .spawn()
            .map_err(|error| format!("spawn S5R 3.0 extension {program}: {error}"))?;
        let stdin = child.take_stdin().ok_or("S5R 3.0 child missing stdin")?;
        let stdout = child.take_stdout().ok_or("S5R 3.0 child missing stdout")?;
        let stderr = child.take_stderr().ok_or("S5R 3.0 child missing stderr")?;
        let stderr_task = tokio::spawn(drain_stderr(stderr));

        let peer = Peer::new(
            StdioFrameTransport::new(stdin, stdout),
            PeerInfo {
                name: "astrcode-host".into(),
                role: "host".into(),
                version: Some(S5R_STACK.into()),
            },
        );
        let features = BTreeSet::from([
            FeatureName::nested_invoke_v1(),
            FeatureName::model_stream_v1(),
            FeatureName::custom_event_v1(),
        ]);
        let mut handshake = PeerHandshake::new("initialize-1");
        handshake.supported_features = features.clone();
        handshake.required_features = features;
        let peer = match tokio::time::timeout(INITIALIZE_TIMEOUT, peer.initialize(handshake)).await
        {
            Ok(Ok(peer)) => peer,
            Ok(Err(error)) => {
                terminate_failed_start(&mut child, stderr_task).await;
                return Err(format!("S5R 3.0 initialize: {error}"));
            },
            Err(_) => {
                terminate_failed_start(&mut child, stderr_task).await;
                return Err("S5R 3.0 initialize timed out".into());
            },
        };

        let (handle, driver) = peer.into_runtime();

        let registration =
            registration_from_s5r_metadata(handle.remote_peer(), handle.remote_metadata())?;

        let registration = Arc::new(RwLock::new(Some(registration)));
        let invoke_contexts = Arc::new(RwLock::new(HashMap::new()));
        let detached_invoke_context = Arc::new(RwLock::new(None));
        let state = Arc::new(HostInvokeState {
            router,
            registration: Arc::clone(&registration),
            reentrancy: Arc::new(AtomicU32::new(0)),
            invoke_contexts: Arc::clone(&invoke_contexts),
            detached_invoke_context: Arc::clone(&detached_invoke_context),
        });
        let driver_shutdown = CancellationToken::new();
        let driver_task = tokio::spawn(driver.run_until(
            Arc::new(V3HostInvokeHandler::<StdioFrameTransport> {
                state,
                transport: std::marker::PhantomData,
            }),
            driver_shutdown.clone(),
        ));

        Ok(Arc::new(Self {
            child: Mutex::new(Some(child)),
            stderr_task: Mutex::new(Some(stderr_task)),
            driver_task: Mutex::new(Some(driver_task)),
            driver_shutdown,
            handle,
            registration,
            invoke_contexts,
            detached_invoke_context,
            admission: Arc::new(tokio::sync::Semaphore::new(MAX_PARALLEL_INVOKES as usize)),
        }))
    }

    pub(crate) fn registration(&self) -> Option<ExtensionRegistration> {
        self.registration.read().clone()
    }

    pub(crate) fn set_detached_invoke_context(&self, context: InvokeContext) {
        *self.detached_invoke_context.write() = Some(context);
    }

    pub(crate) async fn ping(&self) -> Result<(), S5rV3SessionError> {
        let output = self
            .handle
            .invoke(CAP_RUNTIME_PING, Value::Null)
            .await
            .map_err(|error| S5rV3SessionError::Message(error.to_string()))?;
        if output == json!({ "ok": true }) {
            Ok(())
        } else {
            Err(S5rV3SessionError::Message(
                "S5R 3.0 worker returned an invalid ping response".into(),
            ))
        }
    }

    pub(crate) async fn invoke_handler(
        &self,
        handler_id: &str,
        event: Value,
        invoke_context: &InvokeContext,
    ) -> Result<HandlerResult, ExtensionError> {
        self.invoke_handler_in_lane(handler_id, event, invoke_context, ExecutionMode::Sequential)
            .await
    }

    async fn invoke_handler_in_lane(
        &self,
        handler_id: &str,
        event: Value,
        invoke_context: &InvokeContext,
        execution_mode: ExecutionMode,
    ) -> Result<HandlerResult, ExtensionError> {
        let permits = invocation_permit_count(execution_mode);
        let _permit = Arc::clone(&self.admission)
            .acquire_many_owned(permits)
            .await
            .map_err(|_| self.draining_error())?;
        self.invoke_handler_unadmitted(handler_id, event, invoke_context)
            .await
    }

    async fn invoke_handler_unadmitted(
        &self,
        handler_id: &str,
        event: Value,
        invoke_context: &InvokeContext,
    ) -> Result<HandlerResult, ExtensionError> {
        let request_id = self.handle.allocate_request_id();
        let _context = InvokeContextGuard::insert(
            request_id.clone(),
            invoke_context.clone(),
            Arc::clone(&self.invoke_contexts),
        );
        let invoke = self.handle.invoke_with_id(
            request_id,
            CAP_HANDLER_INVOKE,
            serde_json::to_value(HandlerInvokeRequest {
                handler_id: handler_id.to_owned(),
                event,
            })
            .map_err(|error| {
                ExtensionError::Internal(format!("serialize handler request: {error}"))
            })?,
        );
        let output = run_with_cancellation(invoke, invoke_context.cancel_token.as_ref()).await?;
        serde_json::from_value(output)
            .map_err(|error| ExtensionError::Internal(format!("parse HandlerResult: {error}")))
    }

    pub(crate) async fn invoke_handler_with_continuations(
        &self,
        handler_id: &str,
        event: Value,
        invoke_context: &InvokeContext,
        execution_mode: ExecutionMode,
    ) -> Result<HandlerResult, ExtensionError> {
        let attribution = HandlerAttribution::from_event(&event);
        let mut stack = vec![(handler_id.to_owned(), event, 0u32)];
        let mut first = None;
        let extension_id = self
            .registration
            .read()
            .as_ref()
            .map(|registration| registration.extension_id().to_owned())
            .ok_or_else(|| ExtensionError::Internal("extension is not initialized".into()))?;
        let permits = invocation_permit_count(execution_mode);
        let _permit = Arc::clone(&self.admission)
            .acquire_many_owned(permits)
            .await
            .map_err(|_| self.draining_error())?;
        while let Some((handler_id, event, depth)) = stack.pop() {
            if depth > MAX_CONTINUATION_DEPTH {
                return Err(ExtensionError::Internal(format!(
                    "continuation depth exceeded (max {MAX_CONTINUATION_DEPTH})"
                )));
            }
            let mut result = self
                .invoke_handler_unadmitted(&handler_id, event, invoke_context)
                .await?;
            let continuations = std::mem::take(&mut result.continuations);
            if first.is_none() {
                first = Some(result);
            }
            for continuation in continuations.iter().rev() {
                let (next_handler, mut next_event) =
                    continuation.handler_id_for_extension(&extension_id);
                attribution.apply_to(&mut next_event);
                stack.push((next_handler, next_event, depth + 1));
            }
        }
        first.ok_or_else(|| ExtensionError::Internal("empty handler chain".into()))
    }

    fn draining_error(&self) -> ExtensionError {
        let extension_id = self
            .registration
            .read()
            .as_ref()
            .map(|registration| registration.extension_id().to_owned())
            .unwrap_or_else(|| "unknown-extension".into());
        ExtensionError::Draining { extension_id }
    }

    pub(crate) async fn shutdown(&self) {
        *self.detached_invoke_context.write() = None;
        self.admission.close();
        self.driver_shutdown.cancel();
        let driver_task = self.driver_task.lock().take();
        if let Some(driver_task) = driver_task {
            match tokio::time::timeout(Duration::from_secs(2), driver_task).await {
                Ok(Ok(Err(error))) => tracing::debug!(%error, "S5R 3.0 peer driver stopped"),
                Ok(Err(error)) if !error.is_cancelled() => {
                    tracing::warn!(%error, "S5R 3.0 peer driver task failed");
                },
                Err(_) => tracing::warn!("S5R 3.0 peer driver did not stop within reap timeout"),
                _ => {},
            }
        }
        let mut child = self.child.lock().take();
        if let Some(child) = &mut child
            && let Err(error) = child.terminate(PROCESS_TERMINATION_GRACE).await
        {
            tracing::warn!(%error, "failed to terminate S5R 3.0 process tree");
        }
        let mut stderr_task = StderrTaskGuard::new(self.stderr_task.lock().take());
        stderr_task.wait().await;
    }
}

fn invocation_permit_count(execution_mode: ExecutionMode) -> u32 {
    match execution_mode {
        ExecutionMode::Parallel => 1,
        ExecutionMode::Sequential => MAX_PARALLEL_INVOKES,
    }
}

impl Drop for S5rV3Session {
    fn drop(&mut self) {
        self.driver_shutdown.cancel();
        if let Some(task) = self.driver_task.lock().take() {
            task.abort();
        }
        drop(self.child.lock().take());
        if let Some(task) = self.stderr_task.lock().take() {
            task.abort();
        }
    }
}

async fn run_with_cancellation(
    invoke: impl std::future::Future<Output = Result<Value, astrcode_extension_contract::InvokeError>>,
    cancellation: Option<&CancellationToken>,
) -> Result<Value, ExtensionError> {
    let result = if let Some(cancellation) = cancellation {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                return Err(ExtensionError::Internal("S5R 3.0 handler invoke cancelled".into()));
            },
            result = tokio::time::timeout(INVOKE_TIMEOUT, invoke) => result,
        }
    } else {
        tokio::time::timeout(INVOKE_TIMEOUT, invoke).await
    };
    result
        .map_err(|_| ExtensionError::Internal("S5R 3.0 handler invoke timed out".into()))?
        .map_err(|error| ExtensionError::Internal(error.to_string()))
}

async fn terminate_failed_start(child: &mut SupervisedChild, stderr_task: JoinHandle<()>) {
    let _ = child.terminate(PROCESS_TERMINATION_GRACE).await;
    let mut stderr_task = StderrTaskGuard::new(Some(stderr_task));
    stderr_task.wait().await;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tokio::sync::Semaphore;

    use super::*;

    #[test]
    fn continuation_attribution_is_inherited_and_cannot_be_spoofed() {
        let attribution = HandlerAttribution::from_event(&json!({
            "input": {
                "session_id": "session-1",
                "turn_id": "turn-1",
                "call_id": "call-1",
                "working_dir": "/trusted"
            }
        }));
        let mut continuation = json!({
            "input": {
                "arguments": {},
                "session_id": "spoofed",
                "working_dir": "/untrusted"
            }
        });

        attribution.apply_to(&mut continuation);

        assert_eq!(continuation["input"]["session_id"], "session-1");
        assert_eq!(continuation["input"]["turn_id"], "turn-1");
        assert_eq!(continuation["input"]["tool_call_id"], "call-1");
        assert_eq!(continuation["input"]["working_dir"], "/trusted");
    }

    #[tokio::test]
    async fn invocation_admission_caps_parallel_calls_and_excludes_sequential_calls() {
        let admission = Arc::new(Semaphore::new(MAX_PARALLEL_INVOKES as usize));
        let mut parallel = Vec::new();
        for _ in 0..MAX_PARALLEL_INVOKES {
            parallel.push(
                Arc::clone(&admission)
                    .acquire_many_owned(invocation_permit_count(ExecutionMode::Parallel))
                    .await
                    .unwrap(),
            );
        }

        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                Arc::clone(&admission)
                    .acquire_many_owned(invocation_permit_count(ExecutionMode::Parallel)),
            )
            .await
            .is_err()
        );

        drop(parallel);
        let _sequential = tokio::time::timeout(
            Duration::from_secs(1),
            Arc::clone(&admission)
                .acquire_many_owned(invocation_permit_count(ExecutionMode::Sequential)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(admission.available_permits(), 0);
    }
}
