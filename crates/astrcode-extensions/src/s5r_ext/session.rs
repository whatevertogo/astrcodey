//! 宿主侧 s5r Peer 会话（子进程 stdio 帧）。

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use astrcode_core::wire::WireErrorCode;
use astrcode_extension_sdk::{
    self,
    extension::ExtensionError,
    runtime::{
        CancelToken, InitializeHandler, InvokeHandler, InvokeReply, OutboundInvokeControl,
        OutboundInvokeTracker, Peer, StdioFrameTransport,
    },
    s5r::{
        CAP_HANDLER_INVOKE, ErrorPayload, HandlerDescriptor, InitializeMsg, InitializeOutput,
        InvokeMsg, PeerInfo, S5R_STACK, S5R_VERSION, WireMessage, effects::HandlerResult,
    },
    tool::ExecutionMode,
};
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::{
    extension_manifest::ExtensionRegistration,
    host_router::{HostRouter, InvokeContext, backend_unavailable, decls_to_map},
    process_supervision::{SupervisedChild, SupervisedCommand},
};

const MAX_REENTRANCY: u32 = 8;
const MAX_PARALLEL_INVOKES: u32 = 8;
const INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const INVOKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const PEER_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
const PROCESS_TERMINATION_GRACE: std::time::Duration = std::time::Duration::from_millis(500);
const STDERR_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

struct StderrTaskGuard {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl StderrTaskGuard {
    fn new(task: Option<tokio::task::JoinHandle<()>>) -> Self {
        Self { task }
    }

    async fn wait(&mut self) {
        let Some(task) = &mut self.task else {
            return;
        };
        match tokio::time::timeout(STDERR_DRAIN_TIMEOUT, &mut *task).await {
            Ok(Ok(())) => {},
            Ok(Err(error)) if !error.is_cancelled() => {
                tracing::warn!(%error, "s5r stderr drain task failed");
            },
            Ok(Err(_)) => {},
            Err(_) => {
                tracing::warn!("s5r stderr drain timed out after process termination");
                task.abort();
                let _ = task.await;
            },
        }
        self.task = None;
    }
}

impl Drop for StderrTaskGuard {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

struct InvokeContextTracker {
    contexts: Arc<RwLock<HashMap<String, InvokeContext>>>,
    context: InvokeContext,
}

struct HostInvokeState {
    router: Arc<HostRouter>,
    registration: Arc<RwLock<Option<ExtensionRegistration>>>,
    reentrancy: Arc<AtomicU32>,
    invoke_contexts: Arc<RwLock<HashMap<String, InvokeContext>>>,
    detached_invoke_context: Arc<RwLock<Option<InvokeContext>>>,
}

impl OutboundInvokeTracker for InvokeContextTracker {
    fn started(&self, request_id: &str) {
        self.contexts
            .write()
            .insert(request_id.to_owned(), self.context.clone());
    }

    fn finished(&self, request_id: &str) {
        self.contexts.write().remove(request_id);
    }
}

struct ReentrancyGuard {
    counter: Arc<AtomicU32>,
}

impl ReentrancyGuard {
    fn enter(counter: &Arc<AtomicU32>) -> Result<Self, ErrorPayload> {
        let depth = counter.fetch_add(1, Ordering::SeqCst);
        if depth >= MAX_REENTRANCY {
            counter.fetch_sub(1, Ordering::SeqCst);
            return Err(ErrorPayload::new(
                WireErrorCode::ReentrancyExceeded,
                "reentrancy depth exceeded",
            ));
        }
        Ok(Self {
            counter: Arc::clone(counter),
        })
    }
}

impl Drop for ReentrancyGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum S5rSessionError {
    #[error("{0}")]
    Msg(String),
}

pub(crate) struct S5rSession {
    child: Mutex<Option<SupervisedChild>>,
    stderr_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    peer: Arc<Peer<StdioFrameTransport>>,
    registration: Arc<RwLock<Option<ExtensionRegistration>>>,
    invoke_contexts: Arc<RwLock<HashMap<String, InvokeContext>>>,
    detached_invoke_context: Arc<RwLock<Option<InvokeContext>>>,
    in_flight: Arc<tokio::sync::Semaphore>,
}

impl S5rSession {
    pub async fn spawn(
        program: &str,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
        router: Arc<HostRouter>,
    ) -> Result<Arc<Self>, String> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = SupervisedCommand::new(cmd)
            .spawn()
            .map_err(|e| format!("spawn s5r extension {program}: {e}"))?;
        let stdin = child.take_stdin().ok_or("s5r child missing stdin")?;
        let stdout = child.take_stdout().ok_or("s5r child missing stdout")?;
        let stderr = child.take_stderr().ok_or("s5r child missing stderr")?;
        let stderr_task = tokio::spawn(drain_stderr(stderr));

        let transport = StdioFrameTransport::new(stdin, stdout);
        let peer = Peer::new(
            transport,
            PeerInfo {
                name: "astrcode-host".into(),
                role: "core".into(),
                version: Some(S5R_STACK.into()),
            },
        );

        let registration = Arc::new(RwLock::new(None::<ExtensionRegistration>));
        let reentrancy = Arc::new(AtomicU32::new(0));
        let detached_invoke_context = Arc::new(RwLock::new(None));
        let invoke_contexts = Arc::new(RwLock::new(HashMap::new()));

        let registration_for_init = Arc::clone(&registration);
        let init_handler: InitializeHandler = Arc::new(move |init| {
            let registration_for_init = Arc::clone(&registration_for_init);
            Box::pin(async move { handle_initialize(init, &registration_for_init) })
        });
        peer.set_initialize_handler(init_handler);

        let host_invoke_state = Arc::new(HostInvokeState {
            router,
            registration: Arc::clone(&registration),
            reentrancy,
            invoke_contexts: Arc::clone(&invoke_contexts),
            detached_invoke_context: Arc::clone(&detached_invoke_context),
        });
        let invoke_handler: InvokeHandler = Arc::new(move |invoke, token| {
            let host_invoke_state = Arc::clone(&host_invoke_state);
            Box::pin(async move { handle_host_invoke(&host_invoke_state, invoke, token).await })
        });
        peer.set_invoke_handler(invoke_handler);

        let initialize_result = async {
            peer.start().await.map_err(|error| error.to_string())?;
            peer.wait_remote_initialized(INIT_TIMEOUT)
                .await
                .map_err(|error| format!("s5r initialize: {error}"))
        }
        .await;
        if let Err(error) = initialize_result {
            if tokio::time::timeout(PEER_SHUTDOWN_TIMEOUT, peer.stop())
                .await
                .is_err()
            {
                tracing::warn!("s5r peer shutdown timed out after initialization failure");
            }
            let _ = child.terminate(PROCESS_TERMINATION_GRACE).await;
            let mut stderr_task = StderrTaskGuard::new(Some(stderr_task));
            stderr_task.wait().await;
            return Err(error);
        }

        Ok(Arc::new(Self {
            child: Mutex::new(Some(child)),
            stderr_task: Mutex::new(Some(stderr_task)),
            peer,
            registration,
            invoke_contexts,
            detached_invoke_context,
            in_flight: Arc::new(tokio::sync::Semaphore::new(MAX_PARALLEL_INVOKES as usize)),
        }))
    }

    pub fn registration(&self) -> Option<ExtensionRegistration> {
        self.registration.read().clone()
    }

    pub(crate) fn set_detached_invoke_context(&self, context: InvokeContext) {
        *self.detached_invoke_context.write() = Some(context);
    }

    pub fn extension_id(&self) -> String {
        self.registration
            .read()
            .as_ref()
            .map(|r| r.extension_id().to_owned())
            .unwrap_or_default()
    }

    pub async fn ping(&self) -> Result<(), S5rSessionError> {
        self.peer
            .ping()
            .await
            .map_err(|error| S5rSessionError::Msg(error.to_string()))
    }

    pub async fn shutdown(&self) {
        *self.detached_invoke_context.write() = None;
        self.in_flight.close();
        let mut child = self.child.lock().take();
        let mut stderr_task = StderrTaskGuard::new(self.stderr_task.lock().take());
        let outbound_ids = self
            .invoke_contexts
            .read()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let peer_shutdown = async {
            for id in outbound_ids {
                self.peer.cancel_outbound(&id, "session_shutdown").await;
            }
            self.peer.stop().await;
        };
        if tokio::time::timeout(PEER_SHUTDOWN_TIMEOUT, peer_shutdown)
            .await
            .is_err()
        {
            tracing::warn!("s5r peer shutdown timed out; terminating process tree");
        }
        if let Some(child) = &mut child {
            if let Err(error) = child.terminate(PROCESS_TERMINATION_GRACE).await {
                tracing::warn!(%error, "failed to terminate s5r process tree");
            }
        }
        stderr_task.wait().await;
    }

    pub async fn invoke_handler(
        &self,
        handler_id: &str,
        event: Value,
        invoke_ctx: &InvokeContext,
    ) -> Result<HandlerResult, ExtensionError> {
        let _permit = self
            .acquire_invoke_permits(ExecutionMode::Sequential)
            .await?;
        self.invoke_handler_in_lane(handler_id, event, invoke_ctx)
            .await
    }

    async fn invoke_handler_in_lane(
        &self,
        handler_id: &str,
        event: Value,
        invoke_ctx: &InvokeContext,
    ) -> Result<HandlerResult, ExtensionError> {
        let tracker: Arc<dyn OutboundInvokeTracker> = Arc::new(InvokeContextTracker {
            contexts: Arc::clone(&self.invoke_contexts),
            context: invoke_ctx.clone(),
        });
        let control = OutboundInvokeControl {
            external_cancel: invoke_ctx.cancel_token.clone(),
            tracker: Some(tracker),
            ..OutboundInvokeControl::default()
        };
        let output = tokio::time::timeout(
            INVOKE_TIMEOUT,
            self.peer.invoke(
                CAP_HANDLER_INVOKE,
                json!({
                    "handler_id": handler_id,
                    "event": event,
                }),
                control,
            ),
        )
        .await
        .map_err(|_| ExtensionError::Internal("s5r handler invoke timed out".into()))?
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        serde_json::from_value(output)
            .map_err(|e| ExtensionError::Internal(format!("parse HandlerResult: {e}")))
    }

    pub async fn invoke_handler_with_continuations(
        &self,
        handler_id: &str,
        event: Value,
        invoke_ctx: &InvokeContext,
        execution_mode: ExecutionMode,
    ) -> Result<HandlerResult, ExtensionError> {
        const MAX_CONTINUATION_DEPTH: u32 = 16;
        let _permit = self.acquire_invoke_permits(execution_mode).await?;
        let extension_id = self.extension_id();
        let mut stack = vec![(handler_id.to_string(), event, 0u32)];
        let mut first: Option<HandlerResult> = None;
        while let Some((hid, ev, depth)) = stack.pop() {
            if depth > MAX_CONTINUATION_DEPTH {
                return Err(ExtensionError::Internal(format!(
                    "continuation depth exceeded (max {MAX_CONTINUATION_DEPTH})"
                )));
            }
            let mut resp = self.invoke_handler_in_lane(&hid, ev, invoke_ctx).await?;
            let continuations = std::mem::take(&mut resp.continuations);
            if first.is_none() {
                first = Some(resp);
            }
            for cont in continuations.iter().rev() {
                let (nh, ne) = cont.handler_id_for_extension(&extension_id);
                stack.push((nh, ne, depth + 1));
            }
        }
        first.ok_or_else(|| ExtensionError::Internal("empty handler chain".into()))
    }

    async fn acquire_invoke_permits(
        &self,
        requested_mode: ExecutionMode,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, ExtensionError> {
        let permits = if requested_mode == ExecutionMode::Parallel {
            1
        } else {
            MAX_PARALLEL_INVOKES
        };
        Arc::clone(&self.in_flight)
            .acquire_many_owned(permits)
            .await
            .map_err(|_| ExtensionError::Internal("s5r session is shutting down".into()))
    }
}

impl Drop for S5rSession {
    fn drop(&mut self) {
        drop(self.child.lock().take());
        if let Some(stderr_task) = self.stderr_task.lock().take() {
            stderr_task.abort();
        }
    }
}

fn handle_initialize(
    init: InitializeMsg,
    registration: &Arc<RwLock<Option<ExtensionRegistration>>>,
) -> Result<InitializeOutput, ErrorPayload> {
    if init.protocol_version != S5R_VERSION {
        return Err(ErrorPayload::new(
            WireErrorCode::UnsupportedProtocolVersion,
            format!(
                "unsupported s5r protocol version {:?}; expected {S5R_VERSION:?}",
                init.protocol_version
            ),
        ));
    }
    let resolved_registration =
        crate::extension_manifest::registration_from_s5r_metadata(&init.metadata, S5R_VERSION)
            .map_err(|e| ErrorPayload::new(WireErrorCode::InvalidManifest, e))?;
    validate_initialize_handlers(&resolved_registration, &init.handlers)
        .map_err(|error| ErrorPayload::new(WireErrorCode::InvalidManifest, error))?;
    let capabilities = HostRouter::catalog_for_grants(resolved_registration.capabilities());
    *registration.write() = Some(resolved_registration);
    Ok(InitializeOutput {
        peer: PeerInfo {
            name: "astrcode-host".into(),
            role: "core".into(),
            version: Some(S5R_STACK.into()),
        },
        protocol_version: S5R_VERSION.into(),
        capabilities,
        metadata: json!({ "wire_codec": "json" }),
    })
}

fn validate_initialize_handlers(
    registration: &ExtensionRegistration,
    handlers: &[HandlerDescriptor],
) -> Result<(), String> {
    let expected = registration.expected_handler_descriptors()?;
    let extension_id = registration.extension_id();
    let expected_ids = expected
        .iter()
        .map(|descriptor| descriptor.handler_id.as_str())
        .collect::<HashSet<_>>();
    let mut actual_by_id = HashMap::new();
    let mut actual_parts = Vec::with_capacity(handlers.len());
    for handler in handlers {
        let (kind, name) = parse_handler_id(extension_id, &handler.handler_id)?;
        if actual_by_id
            .insert(handler.handler_id.as_str(), handler)
            .is_some()
        {
            return Err(format!(
                "initialize declares duplicate handler {}",
                handler.handler_id
            ));
        }
        actual_parts.push((handler, kind, name));
    }

    for expected_handler in &expected {
        let (expected_kind, expected_name) =
            parse_handler_id(extension_id, &expected_handler.handler_id)?;
        let Some(actual) = actual_by_id.get(expected_handler.handler_id.as_str()) else {
            if let Some((actual, actual_kind, _)) =
                actual_parts.iter().find(|(_, actual_kind, actual_name)| {
                    *actual_name == expected_name && *actual_kind != expected_kind
                })
            {
                return Err(format!(
                    "handler {} has kind {actual_kind}, expected {expected_kind}",
                    actual.handler_id
                ));
            }
            return Err(format!(
                "initialize is missing handler {}",
                expected_handler.handler_id
            ));
        };
        if actual.description != expected_handler.description {
            return Err(format!(
                "handler {} description does not match initialize metadata",
                expected_handler.handler_id
            ));
        }
        if actual.input_schema != expected_handler.input_schema {
            return Err(format!(
                "handler {} input schema does not match initialize metadata",
                expected_handler.handler_id
            ));
        }
    }

    if let Some(extra) = handlers
        .iter()
        .find(|handler| !expected_ids.contains(handler.handler_id.as_str()))
    {
        return Err(format!(
            "initialize declares unexpected handler {}",
            extra.handler_id
        ));
    }
    Ok(())
}

fn parse_handler_id<'a>(
    extension_id: &str,
    handler_id: &'a str,
) -> Result<(&'a str, &'a str), String> {
    crate::extension_manifest::parse_handler_id(extension_id, handler_id)
}

async fn handle_host_invoke(
    state: &HostInvokeState,
    invoke: InvokeMsg,
    token: CancelToken,
) -> Result<InvokeReply, ErrorPayload> {
    token
        .raise_if_cancelled()
        .map_err(|e| ErrorPayload::new(WireErrorCode::Cancelled, e))?;
    if !invoke.capability.starts_with("astrcode.") {
        return Err(ErrorPayload::new(
            WireErrorCode::UnknownCapability,
            format!("host does not provide capability {}", invoke.capability),
        ));
    }
    let _reentrancy = ReentrancyGuard::enter(&state.reentrancy)?;
    let registration = state.registration.read().clone();
    let Some(registration) = registration else {
        return Err(ErrorPayload::new(
            WireErrorCode::NotInitialized,
            "extension not initialized",
        ));
    };
    let parent_context = invoke
        .parent_invoke_id
        .as_deref()
        .and_then(|parent_id| state.invoke_contexts.read().get(parent_id).cloned());
    let mut ctx = resolve_host_invoke_context(
        invoke.parent_invoke_id.as_deref(),
        parent_context,
        state.detached_invoke_context.read().clone(),
    )?;
    ctx.extension_id = registration.extension_id().to_owned();
    ctx.declared_capabilities = registration.capabilities().to_vec();
    ctx.event_declarations = decls_to_map(registration.custom_events());
    ctx.on_peer_io_thread = true;
    let linked_cancellation = link_host_invoke_cancellation(&mut ctx);

    if invoke.stream {
        let events = run_host_invoke_with_wire_cancellation(
            &token,
            &linked_cancellation,
            state
                .router
                .invoke_stream(&invoke.capability, invoke.input, &invoke.id, &ctx),
        )
        .await?
        .into_iter()
        .filter_map(|wire| match wire {
            WireMessage::Event(event) => Some(event),
            _ => None,
        })
        .collect();
        Ok(InvokeReply::Events(events))
    } else {
        let output = run_host_invoke_with_wire_cancellation(
            &token,
            &linked_cancellation,
            state.router.invoke(&invoke.capability, invoke.input, &ctx),
        )
        .await?;
        Ok(InvokeReply::Value(output))
    }
}

fn link_host_invoke_cancellation(ctx: &mut InvokeContext) -> CancellationToken {
    let linked = ctx
        .cancel_token
        .as_ref()
        .map(CancellationToken::child_token)
        .unwrap_or_default();
    ctx.cancel_token = Some(linked.clone());
    linked
}

async fn run_host_invoke_with_wire_cancellation<T>(
    wire_token: &CancelToken,
    linked_cancellation: &CancellationToken,
    invoke: impl std::future::Future<Output = Result<T, ErrorPayload>>,
) -> Result<T, ErrorPayload> {
    let wire_cancellation = wire_token.cancellation_token();
    tokio::pin!(invoke);
    tokio::select! {
        biased;
        () = wire_cancellation.cancelled() => {
            linked_cancellation.cancel();
            invoke.await
        },
        output = &mut invoke => output,
    }
}

fn resolve_host_invoke_context(
    parent_invoke_id: Option<&str>,
    parent_context: Option<InvokeContext>,
    detached_context: Option<InvokeContext>,
) -> Result<InvokeContext, ErrorPayload> {
    match parent_invoke_id {
        Some(parent_id) => parent_context.ok_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::UnknownParentInvoke,
                format!("parent invoke {parent_id} is no longer active"),
            )
        }),
        None => detached_context.ok_or_else(|| {
            backend_unavailable("extension host context is not ready until startup completes")
        }),
    }
}

async fn drain_stderr(stderr: tokio::process::ChildStderr) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut reader = BufReader::new(stderr).lines();
    while let Ok(Some(_line)) = reader.next_line().await {}
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
        permission::ApprovalDecision,
        tool::{
            CreateRootSessionRequest, CreateSessionRequest, SessionAccess, SessionApiError,
            SessionDeliveryOutcome, SessionHandle, SessionOperations, SessionStatus,
            SubmitTurnRequest, SubmitTurnResult, ToolDefinition,
        },
    };
    use astrcode_extension_sdk::{
        extension::{ExtensionCapability, ExtensionTasks},
        host::HostLlmChatRequest,
        s5r::{WIRE_FEATURE_PARENT_INVOKE_ID, capability_to_wire},
    };
    use serde_json::json;
    use tokio::sync::Notify;

    use super::*;

    fn initialize_message(mut metadata: Value) -> InitializeMsg {
        metadata
            .as_object_mut()
            .expect("initialize metadata object")
            .insert(
                "wire_features".into(),
                json!([WIRE_FEATURE_PARENT_INVOKE_ID]),
            );
        InitializeMsg {
            id: "initialize-1".into(),
            protocol_version: S5R_VERSION.into(),
            peer: PeerInfo {
                name: "test-extension".into(),
                role: "extension".into(),
                version: None,
            },
            handlers: Vec::new(),
            provided_capabilities: Vec::new(),
            metadata,
        }
    }

    fn handler_descriptor(
        handler_id: &str,
        description: &str,
        input_schema: Value,
    ) -> HandlerDescriptor {
        HandlerDescriptor {
            handler_id: handler_id.into(),
            description: description.into(),
            input_schema,
        }
    }

    #[test]
    fn host_invoke_context_is_either_parent_scoped_or_started_detached() {
        let inherited_without_workspace = InvokeContext::default();
        let detached = InvokeContext {
            working_dir: Some("/default".into()),
            ..InvokeContext::default()
        };
        let cases = [
            (
                "parent",
                Some("parent-1"),
                Some(inherited_without_workspace.clone()),
                None,
            ),
            ("detached", None, None, Some("/default")),
        ];

        for (name, parent_id, parent, expected) in cases {
            let context =
                resolve_host_invoke_context(parent_id, parent, Some(detached.clone())).unwrap();
            assert_eq!(context.working_dir.as_deref(), expected, "{name}");
        }

        let error = match resolve_host_invoke_context(Some("finished-parent"), None, Some(detached))
        {
            Ok(_) => panic!("finished parents must not fall back to a detached context"),
            Err(error) => error,
        };
        assert_eq!(error.code_enum(), Some(WireErrorCode::UnknownParentInvoke));

        let error = match resolve_host_invoke_context(None, None, None) {
            Ok(_) => panic!("detached invokes must wait for the runner-owned startup context"),
            Err(error) => error,
        };
        assert_eq!(error.code_enum(), Some(WireErrorCode::BackendUnavailable));
    }

    #[test]
    fn initialize_requires_matching_protocol_and_handler_catalog() {
        let metadata = json!({
            "extension_id": "handler-contract",
            "version": "test",
            "protocol": {"s5r": S5R_VERSION},
            "tools": [{
                "name": "probe",
                "description": "Probe tool",
                "parameters": {
                    "type": "object",
                    "properties": {"value": {"type": "string"}}
                }
            }]
        });
        let valid = handler_descriptor(
            "handler-contract:tool:probe",
            "Probe tool",
            json!({
                "type": "object",
                "properties": {"value": {"type": "string"}}
            }),
        );
        let cases = [
            (
                "protocol version",
                "1.0",
                vec![valid.clone()],
                Some((
                    WireErrorCode::UnsupportedProtocolVersion.as_str(),
                    "unsupported s5r protocol",
                )),
            ),
            (
                "missing",
                S5R_VERSION,
                Vec::new(),
                Some((WireErrorCode::InvalidManifest.as_str(), "missing handler")),
            ),
            (
                "duplicate",
                S5R_VERSION,
                vec![valid.clone(), valid.clone()],
                Some((WireErrorCode::InvalidManifest.as_str(), "duplicate handler")),
            ),
            (
                "extra",
                S5R_VERSION,
                vec![
                    valid.clone(),
                    handler_descriptor(
                        "handler-contract:hook:extra",
                        "hook extra",
                        json!({"type": "object"}),
                    ),
                ],
                Some((
                    WireErrorCode::InvalidManifest.as_str(),
                    "unexpected handler",
                )),
            ),
            (
                "kind mismatch",
                S5R_VERSION,
                vec![handler_descriptor(
                    "handler-contract:command:probe",
                    "Probe tool",
                    valid.input_schema.clone(),
                )],
                Some((
                    WireErrorCode::InvalidManifest.as_str(),
                    "has kind command, expected tool",
                )),
            ),
            (
                "descriptor mismatch",
                S5R_VERSION,
                vec![handler_descriptor(
                    "handler-contract:tool:probe",
                    "Probe tool",
                    json!({"type": "string"}),
                )],
                Some((
                    WireErrorCode::InvalidManifest.as_str(),
                    "input schema does not match",
                )),
            ),
            ("valid", S5R_VERSION, vec![valid], None),
        ];

        for (name, protocol_version, handlers, expected_error) in cases {
            let registration = Arc::new(RwLock::new(None));
            let mut initialize = initialize_message(metadata.clone());
            initialize.protocol_version = protocol_version.into();
            initialize.handlers = handlers;
            let result = handle_initialize(initialize, &registration);

            match expected_error {
                Some((expected_code, expected_message)) => {
                    let error = result.expect_err(name);
                    assert_eq!(error.code, expected_code, "{name}");
                    assert!(
                        error.message.contains(expected_message),
                        "{name}: {error:?}"
                    );
                    assert!(registration.read().is_none(), "{name}");
                },
                None => {
                    result.expect("matching protocol and handlers should initialize");
                    assert!(registration.read().is_some(), "{name}");
                },
            }
        }
    }

    #[derive(Default)]
    struct RootSessionOps {
        request: Mutex<Option<CreateRootSessionRequest>>,
    }

    fn unused_session_operation<T>() -> Result<T, SessionApiError> {
        Err(SessionApiError::Unsupported(
            "unused test session operation".into(),
        ))
    }

    #[async_trait::async_trait]
    impl SessionOperations for RootSessionOps {
        async fn create_root_session(
            &self,
            request: CreateRootSessionRequest,
        ) -> Result<SessionHandle, SessionApiError> {
            *self.request.lock() = Some(request);
            Ok(SessionHandle {
                session_id: "detached-root".into(),
            })
        }

        async fn create_session(
            &self,
            _parent_session_id: &str,
            _request: CreateSessionRequest,
        ) -> Result<SessionHandle, SessionApiError> {
            unused_session_operation()
        }

        async fn inject_message(
            &self,
            _access: SessionAccess<'_>,
            _content: String,
        ) -> Result<SessionDeliveryOutcome, SessionApiError> {
            unused_session_operation()
        }

        async fn submit_turn(
            &self,
            _request: SubmitTurnRequest,
        ) -> Result<SubmitTurnResult, SessionApiError> {
            unused_session_operation()
        }

        async fn query_session(
            &self,
            _access: SessionAccess<'_>,
        ) -> Result<SessionStatus, SessionApiError> {
            unused_session_operation()
        }

        async fn recycle_session(&self, _access: SessionAccess<'_>) -> Result<(), SessionApiError> {
            unused_session_operation()
        }

        async fn delete_session(&self, _access: SessionAccess<'_>) -> Result<(), SessionApiError> {
            unused_session_operation()
        }

        async fn restore_session(&self, _access: SessionAccess<'_>) -> Result<(), SessionApiError> {
            unused_session_operation()
        }

        async fn resolve_tool_approval(
            &self,
            _target_session_id: &str,
            _call_id: &str,
            _decision: ApprovalDecision,
        ) -> Result<(), SessionApiError> {
            unused_session_operation()
        }
    }

    #[tokio::test]
    async fn detached_root_input_delivery_uses_the_runner_owned_start_context() {
        let registration = Arc::new(RwLock::new(None));
        handle_initialize(
            initialize_message(json!({
                "extension_id": "detached-channel",
                "version": "test",
                "protocol": {"s5r": S5R_VERSION},
                "capabilities": [capability_to_wire(ExtensionCapability::InputDelivery)]
            })),
            &registration,
        )
        .unwrap();

        let detached_invoke_context = Arc::new(RwLock::new(None));
        let state = HostInvokeState {
            router: Arc::new(HostRouter::from_backends(Default::default())),
            registration,
            reentrancy: Arc::new(AtomicU32::new(0)),
            invoke_contexts: Arc::new(RwLock::new(HashMap::new())),
            detached_invoke_context: Arc::clone(&detached_invoke_context),
        };
        let invoke = InvokeMsg {
            id: "root-create-1".into(),
            capability: "astrcode.session.root.create".into(),
            input: json!({}),
            stream: false,
            parent_invoke_id: None,
        };

        let error = match handle_host_invoke(&state, invoke.clone(), CancelToken::default()).await {
            Ok(_) => panic!("detached host calls must wait until Extension::start"),
            Err(error) => error,
        };
        assert_eq!(error.code_enum(), Some(WireErrorCode::BackendUnavailable));

        let ops = Arc::new(RootSessionOps::default());
        *detached_invoke_context.write() = Some(InvokeContext {
            extension_id: "stale-start-context-id".into(),
            working_dir: Some("/workspace".into()),
            session_ops: Some(ops.clone()),
            tasks: Some(ExtensionTasks::new("detached-channel")),
            ..InvokeContext::default()
        });
        let reply = handle_host_invoke(&state, invoke, CancelToken::default())
            .await
            .unwrap();
        let InvokeReply::Value(output) = reply else {
            panic!("non-streaming root create must return a value");
        };
        assert_eq!(output["session_id"], "detached-root");

        let request = ops.request.lock().clone().expect("captured root request");
        assert_eq!(request.working_dir, "/workspace");
        assert_eq!(
            request.source_extension.as_deref(),
            Some("detached-channel")
        );
    }

    struct StalledLlm {
        started: Arc<Notify>,
        dropped: Arc<AtomicU32>,
    }

    struct DropCounter(Arc<AtomicU32>);

    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for StalledLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<tokio::sync::mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            let _drop_counter = DropCounter(Arc::clone(&self.dropped));
            self.started.notify_one();
            std::future::pending().await
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 8_192,
                max_output_tokens: 1_024,
            }
        }
    }

    #[tokio::test]
    async fn detached_host_invoke_links_generation_and_wire_cancellation() {
        let registration = Arc::new(RwLock::new(None));
        handle_initialize(
            initialize_message(json!({
                "extension_id": "detached-model-client",
                "version": "test",
                "protocol": {"s5r": S5R_VERSION},
                "capabilities": [capability_to_wire(ExtensionCapability::MainModel)]
            })),
            &registration,
        )
        .unwrap();

        let started = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicU32::new(0));
        let detached_invoke_context = Arc::new(RwLock::new(None));
        let state = HostInvokeState {
            router: Arc::new(HostRouter::from_backends(
                crate::host_router::HostBackends {
                    main_llm: Some(Arc::new(StalledLlm {
                        started: Arc::clone(&started),
                        dropped: Arc::clone(&dropped),
                    })),
                    ..Default::default()
                },
            )),
            registration,
            reentrancy: Arc::new(AtomicU32::new(0)),
            invoke_contexts: Arc::new(RwLock::new(HashMap::new())),
            detached_invoke_context: Arc::clone(&detached_invoke_context),
        };
        let invoke = InvokeMsg {
            id: "main-chat-1".into(),
            capability: "astrcode.llm.main_chat".into(),
            input: serde_json::to_value(HostLlmChatRequest::new(vec![LlmMessage::user("hello")]))
                .unwrap(),
            stream: false,
            parent_invoke_id: None,
        };

        for (case_index, cancel_generation) in [true, false].into_iter().enumerate() {
            let generation_cancellation = CancellationToken::new();
            *detached_invoke_context.write() = Some(InvokeContext {
                cancel_token: Some(generation_cancellation.clone()),
                tasks: Some(ExtensionTasks::new("detached-model-client")),
                ..InvokeContext::default()
            });
            let wire_token = CancelToken::default();
            let host_call = handle_host_invoke(&state, invoke.clone(), wire_token.clone());
            tokio::pin!(host_call);
            tokio::select! {
                () = started.notified() => {},
                _ = &mut host_call => panic!("stalled host call returned early"),
            }

            if cancel_generation {
                generation_cancellation.cancel();
            } else {
                wire_token.cancel("wire_cancelled");
            }

            let result = tokio::time::timeout(std::time::Duration::from_secs(1), &mut host_call)
                .await
                .expect("linked cancellation must finish the host call");
            let error = match result {
                Ok(_) => panic!("cancelled host call must not succeed"),
                Err(error) => error,
            };
            assert_eq!(error.code_enum(), Some(WireErrorCode::Cancelled));
            assert_eq!(
                dropped.load(Ordering::SeqCst),
                u32::try_from(case_index + 1).unwrap()
            );
        }
    }

    #[test]
    fn initialize_only_publishes_valid_normalized_registration_and_grants() {
        let registration = Arc::new(RwLock::new(None));
        let mut missing_feature = initialize_message(json!({
            "extension_id": "missing-feature",
            "version": "test",
            "protocol": {"s5r": S5R_VERSION}
        }));
        missing_feature
            .metadata
            .as_object_mut()
            .unwrap()
            .remove("wire_features");
        let error = handle_initialize(missing_feature, &registration).unwrap_err();
        assert_eq!(error.code_enum(), Some(WireErrorCode::InvalidManifest));
        assert!(registration.read().is_none());

        let invalid_metadata = [
            json!({
                "extension_id": "invalid-tool",
                "version": "test",
                "protocol": {"s5r": S5R_VERSION},
                "capabilities": [
                    capability_to_wire(ExtensionCapability::SessionControl)
                ],
                "tools": [{
                    "name": "tool",
                    "description": "",
                    "parameters": {"type": "object"},
                    "mode": "concurrent-ish"
                }]
            }),
            json!({
                "extension_id": "invalid-hook",
                "version": "test",
                "protocol": {"s5r": S5R_VERSION},
                "capabilities": [
                    capability_to_wire(ExtensionCapability::SessionControl)
                ],
                "hooks": [{"on": "continue_after_stop", "mode": "non_blocking"}]
            }),
            json!({
                "extension_id": "invalid-hook-mode",
                "version": "test",
                "protocol": {"s5r": S5R_VERSION},
                "hooks": [{"on": "turn_end", "mode": "sometimes"}]
            }),
            json!({
                "extension_id": "unknown-hook",
                "version": "test",
                "protocol": {"s5r": S5R_VERSION},
                "hooks": [{"on": "typo_hook", "mode": "blocking"}]
            }),
            json!({
                "extension_id": "unsupported-hook",
                "version": "test",
                "protocol": {"s5r": S5R_VERSION},
                "hooks": [{"on": "user_message_envelope", "mode": "blocking"}]
            }),
            json!({
                "extension_id": "invalid-route",
                "version": "test",
                "protocol": {"s5r": S5R_VERSION},
                "capabilities": [
                    capability_to_wire(ExtensionCapability::SessionControl)
                ],
                "http_routes": [{
                    "route": {"method": "GET", "path": "../escape"},
                    "handler_id": "route-handler"
                }]
            }),
        ];

        for metadata in invalid_metadata {
            let registration = Arc::new(RwLock::new(None));
            let error = handle_initialize(initialize_message(metadata), &registration).unwrap_err();

            assert_eq!(error.code_enum(), Some(WireErrorCode::InvalidManifest));
            assert!(registration.read().is_none());
        }

        let registration = Arc::new(RwLock::new(None));
        let mut initialize = initialize_message(json!({
            "extension_id": "valid-extension",
            "version": "test",
            "protocol": {"s5r": S5R_VERSION},
            "capabilities": [
                capability_to_wire(ExtensionCapability::SessionControl)
            ],
            "tools": [{
                "name": "tool",
                "description": "",
                "parameters": {"type": "object"}
            }],
            "hooks": [{"on": "turn_end", "mode": "non_blocking"}],
            "http_routes": [{
                "route": {"method": "GET", "path": "/status"},
                "handler_id": "valid-extension:http:status"
            }],
            "future_manifest_field": true
        }));
        initialize.handlers = vec![
            handler_descriptor("valid-extension:tool:tool", "", json!({"type": "object"})),
            handler_descriptor(
                "valid-extension:hook:turn_end",
                "hook turn_end",
                json!({"type": "object"}),
            ),
            handler_descriptor("valid-extension:http:status", "", json!({"type": "object"})),
        ];
        let output =
            handle_initialize(initialize, &registration).expect("valid metadata should initialize");

        assert!(
            output
                .capabilities
                .iter()
                .any(|capability| capability.name == "astrcode.session.control.create")
        );
        let stored = registration.read();
        let stored = stored.as_ref().expect("registration should be published");
        assert_eq!(stored.extension_id(), "valid-extension");
        assert_eq!(
            stored.tools()[0].execution_mode,
            astrcode_extension_sdk::tool::ExecutionMode::Sequential
        );
    }
}
