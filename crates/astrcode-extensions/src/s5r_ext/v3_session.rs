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

use astrcode_extension_sdk::{
    extension::ExtensionError,
    host::HostError,
    s5r::{
        CAP_HANDLER_INVOKE, CAP_RUNTIME_PING, CallContinuation, ErrorPayload, HandlerId,
        HandlerInvokeRequest, HandlerKind, HandlerResult,
    },
    tool::ExecutionMode,
    wire::{
        FeatureName, HostInitialization, HostInitialized, InboundInvoke, InvocationResponse,
        InvokeError, Peer, PeerHandle, PeerInvokeHandler, StdioFrameTransport, WireErrorCode,
        protocol::{ModelStreamEvent, PeerInfo, S5R_STACK},
    },
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
    extension_manifest::{registration_from_s5r_manifest, validate_registration_features},
    host_router::{HostRouter, InvokeContext, supported_operation_catalog},
    process_supervision::{SupervisedChild, SupervisedCommand},
};

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);
const ACTIVATE_TIMEOUT: Duration = Duration::from_secs(30);
const INVOKE_TIMEOUT_MS: u64 = 120_000;
const INVOKE_TIMEOUT: Duration = Duration::from_millis(INVOKE_TIMEOUT_MS);
const PROCESS_TERMINATION_GRACE: Duration = Duration::from_secs(2);
const MAX_PARALLEL_INVOKES: u32 = 8;
const MAX_CONTINUATION_DEPTH: u32 = 16;

struct InvokeContextGuard<'a> {
    id: String,
    contexts: &'a RwLock<HashMap<String, InvokeContext>>,
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
        let input = event
            .get("scope")
            .and_then(Value::as_object)
            .or_else(|| event.get("input").and_then(Value::as_object));
        Self {
            session_id: input.and_then(|input| input.get("session_id")).cloned(),
            turn_id: input.and_then(|input| input.get("turn_id")).cloned(),
            tool_call_id: input.and_then(|input| input.get("tool_call_id")).cloned(),
            working_dir: input.and_then(|input| input.get("working_dir")).cloned(),
        }
    }

    fn apply_to(&self, event: &mut Value) {
        let attribution_field = if event.get("scope").is_some() {
            "scope"
        } else {
            "input"
        };
        let Some(input) = event
            .get_mut(attribution_field)
            .and_then(Value::as_object_mut)
        else {
            return;
        };
        insert_attribution(input, "session_id", &self.session_id);
        insert_attribution(input, "turn_id", &self.turn_id);
        insert_attribution(input, "tool_call_id", &self.tool_call_id);
        insert_attribution(input, "working_dir", &self.working_dir);
    }
}

// 缺失的归因字段必须显式移除而非透传：宿主侧归因走 parent_invoke_id→invoke_contexts，
// 不依赖这些线缆字段，continuation 自带的同名字段不可信。
fn insert_attribution(input: &mut Map<String, Value>, field: &str, value: &Option<Value>) {
    match value {
        Some(value) => {
            input.insert(field.into(), value.clone());
        },
        None => {
            input.remove(field);
        },
    }
}

impl<'a> InvokeContextGuard<'a> {
    fn insert(
        id: String,
        context: InvokeContext,
        contexts: &'a RwLock<HashMap<String, InvokeContext>>,
    ) -> Self {
        contexts.write().insert(id.clone(), context);
        Self { id, contexts }
    }
}

impl Drop for InvokeContextGuard<'_> {
    fn drop(&mut self) {
        self.contexts.write().remove(&self.id);
    }
}

struct V3HostInvokeHandler {
    state: Arc<HostInvokeState>,
}

struct GuardedModelStream {
    stream: astrcode_extension_sdk::wire::ModelEventStream,
    _reentrancy: ReentrancyGuard,
}

impl Stream for GuardedModelStream {
    type Item = ModelStreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_next(context)
    }
}

#[async_trait::async_trait]
impl PeerInvokeHandler for V3HostInvokeHandler {
    async fn invoke(&self, invocation: InboundInvoke) -> Result<InvocationResponse, ErrorPayload> {
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
    driver_task: Mutex<Option<JoinHandle<Result<(), astrcode_extension_sdk::wire::PeerError>>>>,
    driver_shutdown: CancellationToken,
    initialized_peer: Mutex<Option<Peer<StdioFrameTransport, HostInitialized>>>,
    handle: RwLock<Option<PeerHandle>>,
    host_invoke: Arc<HostInvokeState>,
    admission: Arc<tokio::sync::Semaphore>,
}

impl S5rV3Session {
    pub(crate) async fn spawn(
        program: &str,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
        extension_id: &str,
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
                version: Some(S5R_STACK.into()),
            },
        );
        let features = BTreeSet::from([
            FeatureName::nested_invoke_v1(),
            FeatureName::model_stream_v1(),
            FeatureName::custom_event_v1(),
        ]);
        let mut initialization = HostInitialization::new("initialize-1", extension_id);
        initialization.supported_features = features;
        // 仅 nested_invoke_v1 是所有 worker 的硬依赖（invoke context 经
        // parent_invoke_id 解析）。model stream 在调用时校验；custom-event
        // manifest 在握手完成后、发布注册前校验。
        initialization.required_features = BTreeSet::from([FeatureName::nested_invoke_v1()]);
        initialization.host_operations = supported_operation_catalog();
        let (peer, worker_peer, worker_manifest) =
            match tokio::time::timeout(INITIALIZE_TIMEOUT, peer.initialize(initialization)).await {
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

        let registration = match registration_from_s5r_manifest(&worker_peer, worker_manifest)
            .and_then(|registration| {
                validate_registration_features(&registration, peer.negotiated_features())?;
                Ok(registration)
            }) {
            Ok(registration) => registration,
            Err(error) => {
                terminate_failed_start(&mut child, stderr_task).await;
                return Err(error);
            },
        };
        let host_invoke = Arc::new(HostInvokeState {
            router,
            registration,
            reentrancy: Arc::new(AtomicU32::new(0)),
            invoke_contexts: RwLock::new(HashMap::new()),
            detached_invoke_context: RwLock::new(None),
        });
        let driver_shutdown = CancellationToken::new();

        Ok(Arc::new(Self {
            child: Mutex::new(Some(child)),
            stderr_task: Mutex::new(Some(stderr_task)),
            driver_task: Mutex::new(None),
            driver_shutdown,
            initialized_peer: Mutex::new(Some(peer)),
            handle: RwLock::new(None),
            host_invoke,
            admission: Arc::new(tokio::sync::Semaphore::new(MAX_PARALLEL_INVOKES as usize)),
        }))
    }

    pub(crate) fn registration(&self) -> &crate::extension_manifest::ExtensionRegistration {
        &self.host_invoke.registration
    }

    pub(crate) fn set_detached_invoke_context(&self, context: InvokeContext) {
        *self.host_invoke.detached_invoke_context.write() = Some(context);
    }

    pub(crate) async fn activate(&self) -> Result<(), ExtensionError> {
        let peer = self.initialized_peer.lock().take().ok_or_else(|| {
            ExtensionError::Internal("S5R 3.0 session is not awaiting activation".into())
        })?;
        let peer = match tokio::time::timeout(ACTIVATE_TIMEOUT, peer.activate("activate-1")).await {
            Ok(Ok(peer)) => peer,
            Ok(Err(error)) => {
                self.shutdown().await;
                return Err(ExtensionError::Internal(format!(
                    "S5R 3.0 activate: {error}"
                )));
            },
            Err(_) => {
                self.shutdown().await;
                return Err(ExtensionError::Timeout(ACTIVATE_TIMEOUT.as_millis() as u64));
            },
        };
        let (handle, driver) = peer.into_runtime();
        *self.handle.write() = Some(handle);
        let driver_task = tokio::spawn(driver.run_until(
            Arc::new(V3HostInvokeHandler {
                state: Arc::clone(&self.host_invoke),
            }),
            self.driver_shutdown.clone(),
        ));
        *self.driver_task.lock() = Some(driver_task);
        Ok(())
    }

    pub(crate) async fn ping(&self) -> Result<(), ExtensionError> {
        let handle = self
            .handle
            .read()
            .clone()
            .ok_or_else(|| ExtensionError::Internal("S5R 3.0 session is not active".into()))?;
        let output = handle
            .invoke(CAP_RUNTIME_PING, Value::Null)
            .await
            .map_err(|error| ExtensionError::Internal(error.to_string()))?;
        if output == json!({ "ok": true }) {
            Ok(())
        } else {
            Err(ExtensionError::Internal(
                "S5R 3.0 worker returned an invalid ping response".into(),
            ))
        }
    }

    pub(crate) async fn invoke_handler(
        &self,
        handler_id: &astrcode_extension_sdk::wire::HandlerId,
        event: Value,
        invoke_context: &InvokeContext,
    ) -> Result<HandlerResult, ExtensionError> {
        self.invoke_handler_in_lane(handler_id, event, invoke_context, ExecutionMode::Sequential)
            .await
    }

    async fn invoke_handler_in_lane(
        &self,
        handler_id: &astrcode_extension_sdk::wire::HandlerId,
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
        handler_id: &astrcode_extension_sdk::wire::HandlerId,
        event: Value,
        invoke_context: &InvokeContext,
    ) -> Result<HandlerResult, ExtensionError> {
        let handle = self
            .handle
            .read()
            .clone()
            .ok_or_else(|| ExtensionError::Internal("S5R 3.0 session is not active".into()))?;
        let request_id = handle.allocate_request_id();
        let _context = InvokeContextGuard::insert(
            request_id.clone(),
            invoke_context.clone(),
            &self.host_invoke.invoke_contexts,
        );
        let invoke = handle.invoke_with_id(
            request_id,
            CAP_HANDLER_INVOKE,
            serde_json::to_value(HandlerInvokeRequest {
                handler_id: handler_id.clone(),
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
        handler_id: &astrcode_extension_sdk::wire::HandlerId,
        event: Value,
        invoke_context: &InvokeContext,
        execution_mode: ExecutionMode,
    ) -> Result<HandlerResult, ExtensionError> {
        let attribution = HandlerAttribution::from_event(&event);
        let mut stack = vec![(handler_id.clone(), event, 0u32)];
        let mut first = None;
        let extension_id = self.host_invoke.registration.extension_id.clone();
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
                let (next_handler, mut next_event) = continuation_call(continuation, &extension_id)
                    .map_err(ExtensionError::Internal)?;
                attribution.apply_to(&mut next_event);
                stack.push((next_handler, next_event, depth + 1));
            }
        }
        first.ok_or_else(|| ExtensionError::Internal("empty handler chain".into()))
    }

    pub(crate) async fn invoke_handler_once(
        &self,
        handler_id: &astrcode_extension_sdk::wire::HandlerId,
        event: Value,
        invoke_context: &InvokeContext,
        execution_mode: ExecutionMode,
    ) -> Result<HandlerResult, ExtensionError> {
        let permits = invocation_permit_count(execution_mode);
        let _permit = Arc::clone(&self.admission)
            .acquire_many_owned(permits)
            .await
            .map_err(|_| self.draining_error())?;
        let result = self
            .invoke_handler_unadmitted(handler_id, event, invoke_context)
            .await?;
        if !result.continuations.is_empty() {
            return Err(ExtensionError::Internal(
                "tool planning cannot return continuations".into(),
            ));
        }
        Ok(result)
    }

    fn draining_error(&self) -> ExtensionError {
        let extension_id = self.host_invoke.registration.extension_id.clone();
        ExtensionError::Draining { extension_id }
    }

    pub(crate) async fn shutdown(&self) {
        *self.host_invoke.detached_invoke_context.write() = None;
        self.admission.close();
        self.driver_shutdown.cancel();
        self.initialized_peer.lock().take();
        self.handle.write().take();
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

fn continuation_call(
    continuation: &CallContinuation,
    extension_id: &str,
) -> Result<(HandlerId, Value), String> {
    match continuation {
        CallContinuation::Hook { on, input } => Ok((
            HandlerId::new(extension_id, HandlerKind::Hook, on)?,
            json!({ "on": on, "input": input }),
        )),
        CallContinuation::Tool { name, input } => Ok((
            HandlerId::new(extension_id, HandlerKind::Tool, name)?,
            json!({
                "phase": astrcode_extension_sdk::s5r::ToolInvocationPhase::Execute,
                "arguments": input,
                "scope": {}
            }),
        )),
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
        self.initialized_peer.lock().take();
        self.handle.write().take();
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
    invoke: impl std::future::Future<Output = Result<Value, InvokeError>>,
    cancellation: Option<&CancellationToken>,
) -> Result<Value, ExtensionError> {
    let result = if let Some(cancellation) = cancellation {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => {
                return Err(ExtensionError::Cancelled);
            },
            result = tokio::time::timeout(INVOKE_TIMEOUT, invoke) => result,
        }
    } else {
        tokio::time::timeout(INVOKE_TIMEOUT, invoke).await
    };
    result
        .map_err(|_| ExtensionError::Timeout(INVOKE_TIMEOUT_MS))?
        .map_err(invoke_error_to_extension_error)
}

fn invoke_error_to_extension_error(error: InvokeError) -> ExtensionError {
    let payload = match error {
        InvokeError::Local(payload) | InvokeError::Remote(payload) => payload,
        InvokeError::DriverUnavailable => ErrorPayload::new(
            WireErrorCode::HostNotReady,
            "S5R 3.0 extension peer driver is not running",
        ),
        InvokeError::PeerClosed => ErrorPayload::new(
            WireErrorCode::PeerClosed,
            "S5R 3.0 extension peer closed before the invoke completed",
        ),
    };
    ExtensionError::Host(HostError::from(payload))
}

async fn terminate_failed_start(child: &mut SupervisedChild, stderr_task: JoinHandle<()>) {
    let _ = child.terminate(PROCESS_TERMINATION_GRACE).await;
    let mut stderr_task = StderrTaskGuard::new(Some(stderr_task));
    stderr_task.wait().await;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use astrcode_extension_sdk::wire::WireErrorCode;
    use serde_json::json;
    use tokio::sync::Semaphore;

    use super::*;

    #[test]
    fn continuation_attribution_is_inherited_and_cannot_be_spoofed() {
        let attribution = HandlerAttribution::from_event(&json!({
            "scope": {
                "session_id": "session-1",
                "turn_id": "turn-1",
                "tool_call_id": "call-1",
                "working_dir": "/trusted"
            }
        }));
        let mut continuation = json!({
            "scope": {
                "session_id": "spoofed",
                "working_dir": "/untrusted"
            }
        });

        attribution.apply_to(&mut continuation);

        assert_eq!(continuation["scope"]["session_id"], "session-1");
        assert_eq!(continuation["scope"]["turn_id"], "turn-1");
        assert_eq!(continuation["scope"]["tool_call_id"], "call-1");
        assert_eq!(continuation["scope"]["working_dir"], "/trusted");
    }

    #[test]
    fn continuation_attribution_strips_fields_absent_from_the_trusted_event() {
        let attribution = HandlerAttribution::from_event(&json!({
            "input": { "arguments": {} }
        }));
        let mut continuation = json!({
            "input": {
                "arguments": {},
                "session_id": "spoofed",
                "turn_id": "spoofed",
                "tool_call_id": "spoofed",
                "working_dir": "/untrusted"
            }
        });

        attribution.apply_to(&mut continuation);

        let input = continuation["input"].as_object().unwrap();
        assert_eq!(input.get("arguments"), Some(&json!({})));
        for field in ["session_id", "turn_id", "tool_call_id", "working_dir"] {
            assert!(
                !input.contains_key(field),
                "continuation must not carry spoofed {field}"
            );
        }
    }

    #[tokio::test]
    async fn invoke_errors_preserve_structured_protocol_semantics() {
        for (invoke_error, code, retryable) in [
            (
                InvokeError::Remote(ErrorPayload::new(
                    WireErrorCode::Cancelled,
                    "structured worker error",
                )),
                WireErrorCode::Cancelled,
                false,
            ),
            (
                InvokeError::Local(
                    ErrorPayload::new(WireErrorCode::InvalidInput, "local admission error")
                        .retryable(true),
                ),
                WireErrorCode::InvalidInput,
                true,
            ),
            (
                InvokeError::DriverUnavailable,
                WireErrorCode::HostNotReady,
                false,
            ),
            (InvokeError::PeerClosed, WireErrorCode::PeerClosed, false),
        ] {
            let error = run_with_cancellation(async { Err(invoke_error) }, None)
                .await
                .unwrap_err();

            match error {
                ExtensionError::Host(host) => {
                    assert_eq!(host.code_enum(), Some(code));
                    assert_eq!(host.retryable, retryable);
                },
                other => panic!("invoke errors must surface as ExtensionError::Host, got {other}"),
            }
        }
    }

    #[tokio::test]
    async fn cancellation_is_distinguishable_from_internal_errors() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = run_with_cancellation(
            std::future::pending::<Result<Value, InvokeError>>(),
            Some(&cancellation),
        )
        .await
        .unwrap_err();

        assert!(
            matches!(error, ExtensionError::Cancelled),
            "cancellation must not collapse into ExtensionError::Internal, got {error}"
        );
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
