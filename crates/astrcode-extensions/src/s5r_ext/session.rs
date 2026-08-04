//! 宿主侧 s5r Peer 会话（子进程 stdio 帧）。

use std::{
    collections::HashMap,
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

use astrcode_extension_sdk::{
    extension::{ExtensionCapability, ExtensionError, ExtensionEventDecl},
    runtime::{
        CancelToken, InitializeHandler, InvokeHandler, InvokeReply, OutboundInvokeControl,
        OutboundInvokeTracker, Peer, PeerError, StdioFrameTransport,
    },
    s5r::{
        CAP_HANDLER_INVOKE, ErrorPayload, InitializeMsg, InitializeOutput, InvokeMsg, PeerInfo,
        S5R_STACK, S5R_VERSION, WIRE_FEATURE_PARENT_INVOKE_ID, WireMessage, effects::HandlerResult,
    },
    tool::ExecutionMode,
};
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::{
    extension_manifest::ExtensionRegistration,
    host_router::{HostRouter, InvokeContext, decls_to_map},
    process_supervision::{SupervisedChild, SupervisedCommand},
    s5r_ext::protocol::S5R_PROTOCOL_VERSION,
};

const MAX_REENTRANCY: u32 = 8;
const MAX_PARALLEL_INVOKES: u32 = 8;
const INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const INVOKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const PEER_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
const PROCESS_TERMINATION_GRACE: std::time::Duration = std::time::Duration::from_millis(500);
const STDERR_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

struct LegacyInvokeGuard {
    active_invoke: Arc<RwLock<Option<InvokeContext>>>,
    poisoned: Arc<AtomicBool>,
    completed: bool,
}

impl LegacyInvokeGuard {
    fn set(
        active_invoke: &Arc<RwLock<Option<InvokeContext>>>,
        poisoned: &Arc<AtomicBool>,
        ctx: InvokeContext,
    ) -> Self {
        *active_invoke.write() = Some(ctx);
        Self {
            active_invoke: Arc::clone(active_invoke),
            poisoned: Arc::clone(poisoned),
            completed: false,
        }
    }

    fn complete(&mut self) {
        self.completed = true;
    }
}

impl Drop for LegacyInvokeGuard {
    fn drop(&mut self) {
        *self.active_invoke.write() = None;
        if !self.completed {
            self.poisoned.store(true, Ordering::Release);
        }
    }
}

fn peer_result_confirms_remote_completion(result: &Result<Value, PeerError>) -> bool {
    matches!(result, Ok(_) | Err(PeerError::Payload(_)))
}

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
    legacy_active_invoke: Arc<RwLock<Option<InvokeContext>>>,
    legacy_context_poisoned: Arc<AtomicBool>,
    default_working_dir: Arc<RwLock<Option<String>>>,
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
                "reentrancy_exceeded",
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
    legacy_active_invoke: Arc<RwLock<Option<InvokeContext>>>,
    legacy_context_poisoned: Arc<AtomicBool>,
    in_flight: Arc<tokio::sync::Semaphore>,
}

impl S5rSession {
    pub async fn spawn(
        program: &str,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
        router: Arc<HostRouter>,
        working_dir: Option<&str>,
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
        let default_working_dir = Arc::new(RwLock::new(working_dir.map(str::to_string)));
        let invoke_contexts = Arc::new(RwLock::new(HashMap::new()));
        let legacy_active_invoke = Arc::new(RwLock::new(None));
        let legacy_context_poisoned = Arc::new(AtomicBool::new(false));

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
            legacy_active_invoke: Arc::clone(&legacy_active_invoke),
            legacy_context_poisoned: Arc::clone(&legacy_context_poisoned),
            default_working_dir,
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
            legacy_active_invoke,
            legacy_context_poisoned,
            in_flight: Arc::new(tokio::sync::Semaphore::new(MAX_PARALLEL_INVOKES as usize)),
        }))
    }

    pub fn registration(&self) -> Option<ExtensionRegistration> {
        self.registration.read().clone()
    }

    pub fn extension_id(&self) -> String {
        self.registration
            .read()
            .as_ref()
            .map(|r| r.extension_id().to_owned())
            .unwrap_or_default()
    }

    pub fn declared_capabilities(&self) -> Vec<ExtensionCapability> {
        self.registration
            .read()
            .as_ref()
            .map(|r| r.capabilities().to_vec())
            .unwrap_or_default()
    }

    pub fn event_decls(&self) -> std::collections::HashMap<String, ExtensionEventDecl> {
        self.registration
            .read()
            .as_ref()
            .map(|r| decls_to_map(r.extension_events()))
            .unwrap_or_default()
    }

    pub async fn ping(&self) -> Result<(), S5rSessionError> {
        let _ = self
            .invoke_handler(
                &format!("{}:tool:ping", self.extension_id()),
                json!({ "on": "tool", "name": "ping", "input": {} }),
                &InvokeContext::default(),
            )
            .await
            .map_err(|e| S5rSessionError::Msg(e.to_string()))?;
        Ok(())
    }

    pub async fn shutdown(&self) {
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
        let supports_parent_context = self.supports_parent_invoke_id();
        if !supports_parent_context && self.legacy_context_poisoned.load(Ordering::Acquire) {
            return Err(ExtensionError::Internal(
                "legacy s5r context was cancelled; reload the extension before invoking it again"
                    .into(),
            ));
        }

        let mut legacy_context = (!supports_parent_context).then(|| {
            LegacyInvokeGuard::set(
                &self.legacy_active_invoke,
                &self.legacy_context_poisoned,
                invoke_ctx.clone(),
            )
        });
        let tracker: Arc<dyn OutboundInvokeTracker> = Arc::new(InvokeContextTracker {
            contexts: Arc::clone(&self.invoke_contexts),
            context: invoke_ctx.clone(),
        });
        let control = OutboundInvokeControl {
            external_cancel: invoke_ctx.cancel_token.clone(),
            tracker: Some(tracker),
            ..OutboundInvokeControl::default()
        };
        let extension_id = self.extension_id();
        let invoked = tokio::time::timeout(
            INVOKE_TIMEOUT,
            self.peer.invoke(
                CAP_HANDLER_INVOKE,
                json!({
                    "handler_id": handler_id,
                    "event": event,
                    "caller_extension_id": extension_id,
                }),
                Some(&extension_id),
                control,
            ),
        )
        .await;
        if let Ok(peer_result) = &invoked {
            if peer_result_confirms_remote_completion(peer_result) {
                if let Some(legacy_context) = &mut legacy_context {
                    legacy_context.complete();
                }
            }
        }
        let output = invoked
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
        let permits =
            if requested_mode == ExecutionMode::Parallel && self.supports_parent_invoke_id() {
                1
            } else {
                MAX_PARALLEL_INVOKES
            };
        Arc::clone(&self.in_flight)
            .acquire_many_owned(permits)
            .await
            .map_err(|_| ExtensionError::Internal("s5r session is shutting down".into()))
    }

    fn supports_parent_invoke_id(&self) -> bool {
        self.registration
            .read()
            .as_ref()
            .is_some_and(|registration| {
                registration.supports_wire_feature(WIRE_FEATURE_PARENT_INVOKE_ID)
            })
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
    let reg = crate::extension_manifest::registration_from_s5r_metadata(
        &init.metadata,
        S5R_PROTOCOL_VERSION,
    )
    .map_err(|e| ErrorPayload::new("invalid_manifest", e))?;
    let caps = HostRouter::catalog_for_grants(reg.capabilities());
    *registration.write() = Some(reg);
    Ok(InitializeOutput {
        peer: PeerInfo {
            name: "astrcode-host".into(),
            role: "core".into(),
            version: Some(S5R_STACK.into()),
        },
        protocol_version: Some(S5R_VERSION.into()),
        capabilities: caps,
        metadata: json!({ "wire_codec": "json" }),
    })
}

async fn handle_host_invoke(
    state: &HostInvokeState,
    invoke: InvokeMsg,
    token: CancelToken,
) -> Result<InvokeReply, ErrorPayload> {
    token
        .raise_if_cancelled()
        .map_err(|e| ErrorPayload::new("cancelled", e))?;
    if !invoke.capability.starts_with("astrcode.") {
        return Err(ErrorPayload::new(
            "unknown_capability",
            format!("host does not provide capability {}", invoke.capability),
        ));
    }
    let _reentrancy = ReentrancyGuard::enter(&state.reentrancy)?;
    let reg = state.registration.read().clone();
    let Some(reg) = reg else {
        return Err(ErrorPayload::new(
            "not_initialized",
            "extension not initialized",
        ));
    };
    let mut ctx = match invoke.parent_invoke_id.as_deref() {
        Some(parent_id) => state
            .invoke_contexts
            .read()
            .get(parent_id)
            .cloned()
            .ok_or_else(|| {
                ErrorPayload::new(
                    "unknown_parent_invoke",
                    format!("parent invoke {parent_id} is no longer active"),
                )
            })?,
        None if reg.supports_wire_feature(WIRE_FEATURE_PARENT_INVOKE_ID) => {
            InvokeContext::default()
        },
        None if state.legacy_context_poisoned.load(Ordering::Acquire) => {
            return Err(ErrorPayload::new(
                "legacy_context_lost",
                "legacy worker context was cancelled; reload the extension",
            ));
        },
        None => state
            .legacy_active_invoke
            .read()
            .clone()
            .unwrap_or_default(),
    };
    if ctx.working_dir.is_none() {
        ctx.working_dir = state.default_working_dir.read().clone();
    }
    ctx.extension_id = reg.extension_id().to_owned();
    ctx.declared_capabilities = reg.capabilities().to_vec();
    ctx.event_declarations = decls_to_map(reg.extension_events());
    ctx.on_peer_io_thread = true;
    ctx.cancel_token = Some(token.cancellation_token());

    if invoke.stream {
        let events = state
            .router
            .invoke_stream_sync(
                &invoke.capability,
                &invoke.input.to_string(),
                &invoke.id,
                &ctx,
            )
            .map(|events| {
                events
                    .into_iter()
                    .filter_map(|wire| match wire {
                        WireMessage::Event(ev) => Some(ev),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })?;
        Ok(InvokeReply::Events(events))
    } else {
        let output =
            state
                .router
                .invoke_sync(&invoke.capability, &invoke.input.to_string(), &ctx)?;
        Ok(InvokeReply::Value(output))
    }
}

async fn drain_stderr(stderr: tokio::process::ChildStderr) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut reader = BufReader::new(stderr).lines();
    while let Ok(Some(_line)) = reader.next_line().await {}
}

#[cfg(test)]
mod tests {
    use astrcode_extension_sdk::{runtime::PeerError, s5r::capability_to_wire};
    use serde_json::json;

    use super::*;

    fn initialize_message(metadata: Value) -> InitializeMsg {
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

    #[test]
    fn only_remote_terminal_results_complete_legacy_invoke() {
        let cases = [
            (Ok(json!({"ok": true})), true),
            (Err(PeerError::Payload("remote error".into())), true),
            (Err(PeerError::Timeout), false),
            (Err(PeerError::Closed), false),
            (Err(PeerError::Msg("transport error".into())), false),
            (Err(PeerError::Busy), false),
        ];

        for (result, expected) in cases {
            assert_eq!(
                peer_result_confirms_remote_completion(&result),
                expected,
                "unexpected legacy completion classification for {result:?}"
            );
        }
    }

    #[test]
    fn initialize_only_publishes_valid_normalized_registration_and_grants() {
        let invalid_metadata = [
            json!({
                "extension_id": "invalid-tool",
                "protocol": {"s5r": S5R_PROTOCOL_VERSION},
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
                "protocol": {"s5r": S5R_PROTOCOL_VERSION},
                "capabilities": [
                    capability_to_wire(ExtensionCapability::SessionControl)
                ],
                "hooks": [{"on": "continue_after_stop", "mode": "non_blocking"}]
            }),
            json!({
                "extension_id": "invalid-hook-mode",
                "protocol": {"s5r": S5R_PROTOCOL_VERSION},
                "hooks": [{"on": "turn_end", "mode": "sometimes"}]
            }),
            json!({
                "extension_id": "unknown-hook",
                "protocol": {"s5r": S5R_PROTOCOL_VERSION},
                "hooks": [{"on": "typo_hook", "mode": "blocking"}]
            }),
            json!({
                "extension_id": "unsupported-hook",
                "protocol": {"s5r": S5R_PROTOCOL_VERSION},
                "hooks": [{"on": "user_message_envelope", "mode": "blocking"}]
            }),
            json!({
                "extension_id": "invalid-route",
                "protocol": {"s5r": S5R_PROTOCOL_VERSION},
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

            assert_eq!(error.code, "invalid_manifest");
            assert!(registration.read().is_none());
        }

        let registration = Arc::new(RwLock::new(None));
        let output = handle_initialize(
            initialize_message(json!({
                "extension_id": "valid-extension",
                "protocol": {"s5r": S5R_PROTOCOL_VERSION},
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
                    "handler_id": "status"
                }],
                "future_manifest_field": true
            })),
            &registration,
        )
        .expect("valid metadata should initialize");

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
