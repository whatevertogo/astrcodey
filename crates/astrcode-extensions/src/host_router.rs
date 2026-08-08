//! 宿主能力路由 — 唯一实现 `astrcode.*` RPC 与扩展事件发射。

mod capability;
mod context;
mod extension_http;
mod llm;
mod network;
mod path;
mod process;
mod session;
mod session_inspect;
mod workspace;

use std::{collections::HashMap, future::Future, path::PathBuf, sync::Arc, time::Duration};

use astrcode_core::{
    event::{
        DurableEventPayload, EventPayload, EventPublishReceipt, EventSender, ExtensionEventData,
        LiveEventPayload,
    },
    llm::LlmProvider,
    tool::SessionOperations,
    wire::{WireError, WireErrorCode},
};
use astrcode_extension_sdk::{
    extension::{
        ExtensionCapability, ExtensionError, ExtensionEventDecl, ExtensionHttpRequest,
        ExtensionHttpResponse, ExtensionTaskError, ExtensionTasks,
    },
    host::internal::OutboundNetworkService,
    s5r::{CapabilityDescriptor, ErrorPayload, EventMsg, EventPhase, WireMessage},
};
use astrcode_storage::{EventReader, SessionReader};
use serde_json::Value;
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

use self::{
    capability::{HostCapability, ProcessCapability},
    context::ContextGroup,
    extension_http::ExtensionHttpGroup,
    llm::LlmGroup,
    network::NetworkGroup,
    process::ProcessGroup,
    session::SessionGroup,
    workspace::WorkspaceGroup,
};

pub(super) const HOST_INVOKE_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn parse_wire_request<'de, T>(
    input: &'de Value,
    capability: &str,
) -> Result<T, ErrorPayload>
where
    T: serde::Deserialize<'de>,
{
    T::deserialize(input).map_err(|error| {
        ErrorPayload::new(
            WireErrorCode::InvalidInput,
            format!("invalid {capability} request: {error}"),
        )
    })
}

pub(super) fn serialize_wire_response<T>(output: T, capability: &str) -> Result<Value, ErrorPayload>
where
    T: serde::Serialize,
{
    serde_json::to_value(output).map_err(|error| {
        ErrorPayload::new(
            WireErrorCode::SerializationFailed,
            format!("failed to serialize {capability} response: {error}"),
        )
    })
}

pub(super) fn wire_payload<E: WireError>(error: E) -> ErrorPayload {
    ErrorPayload::new(error.wire_code(), error.to_string()).retryable(error.is_retryable())
}

pub(super) fn io_error(error: impl std::fmt::Display) -> ErrorPayload {
    ErrorPayload::new(WireErrorCode::IoError, error.to_string())
}

pub(super) fn backend_unavailable(message: impl Into<String>) -> ErrorPayload {
    ErrorPayload::new(WireErrorCode::BackendUnavailable, message)
}

/// deadline + 取消的 biased select 包装：取消优先于超时。并发语义（`biased` 顺序、
/// 超时/取消的先后）必须所有调用点一致，故收敛为共享实现。
pub(super) async fn run_until_deadline<F, T, E>(
    operation: F,
    deadline: Instant,
    cancel_token: Option<&CancellationToken>,
    timeout_err: impl FnOnce() -> E,
    cancel_err: impl FnOnce() -> E,
) -> Result<T, E>
where
    F: Future<Output = Result<T, E>>,
{
    let timed = async {
        timeout_at(deadline, operation)
            .await
            .map_err(|_| timeout_err())?
    };
    match cancel_token {
        Some(token) => {
            tokio::select! {
                biased;
                () = token.cancelled() => Err(cancel_err()),
                result = timed => result,
            }
        },
        None => timed.await,
    }
}

pub(super) async fn run_blocking_io<T>(
    operation: impl FnOnce() -> Result<T, ErrorPayload> + Send + 'static,
) -> Result<T, ErrorPayload>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| {
            ErrorPayload::new(
                WireErrorCode::HostRuntimeFailed,
                format!("blocking host I/O task failed: {error}"),
            )
        })?
}

pub(super) async fn run_blocking_io_to_completion<T>(
    tasks: Option<&ExtensionTasks>,
    name: &'static str,
    operation: impl FnOnce() -> Result<T, ErrorPayload> + Send + 'static,
) -> Result<T, ErrorPayload>
where
    T: Send + 'static,
{
    let tasks = tasks.ok_or_else(|| {
        ErrorPayload::new(
            WireErrorCode::BackendUnavailable,
            "extension task owner is unavailable for persistent host I/O",
        )
    })?;
    tasks
        .run_to_completion(name, run_blocking_io(operation))
        .await
        .map_err(|error| match error {
            ExtensionTaskError::ShuttingDown { .. } => {
                ErrorPayload::new(WireErrorCode::Cancelled, error.to_string())
            },
            ExtensionTaskError::Panicked { .. } | ExtensionTaskError::RuntimeStopped { .. } => {
                ErrorPayload::new(WireErrorCode::HostRuntimeFailed, error.to_string())
            },
        })?
}

fn ensure_invoke_active(ctx: &InvokeContext) -> Result<(), ErrorPayload> {
    if ctx
        .cancel_token
        .as_ref()
        .is_some_and(CancellationToken::is_cancelled)
    {
        return Err(ErrorPayload::new(
            WireErrorCode::Cancelled,
            "invoke cancelled",
        ));
    }
    Ok(())
}

/// 单次 guest→host invoke 的运行时上下文。
#[derive(Clone, Default)]
pub struct InvokeContext {
    pub extension_id: String,
    pub session_id: Option<String>,
    /// 当前宿主工具调用 ID；非工具入口不存在该归属。
    pub tool_call_id: Option<String>,
    pub session_store_dir: Option<PathBuf>,
    pub session_ops: Option<Arc<dyn SessionOperations>>,
    pub event_tx: Option<EventSender>,
    pub working_dir: Option<String>,
    pub cancel_token: Option<CancellationToken>,
    pub tasks: Option<ExtensionTasks>,
    pub event_declarations: HashMap<String, ExtensionEventDecl>,
    pub declared_capabilities: Vec<ExtensionCapability>,
    /// 当前调用是否在 peer 专用 I/O 线程上（同步 host import；IPC 子进程共用）。
    pub on_peer_io_thread: bool,
}

/// 宿主后端依赖。
#[derive(Default)]
pub struct HostBackends {
    pub main_llm: Option<Arc<dyn LlmProvider>>,
    pub small_llm: Option<Arc<dyn LlmProvider>>,
    pub event_reader: Option<Arc<dyn EventReader>>,
    pub session_reader: Option<Arc<dyn SessionReader>>,
    pub default_working_dir: Option<String>,
    pub public_http_dispatcher: Option<Arc<dyn PublicHttpDispatcher>>,
    pub outbound_network: Option<Arc<dyn OutboundNetworkService>>,
}

#[async_trait::async_trait]
pub trait PublicHttpDispatcher: Send + Sync {
    async fn dispatch_public_http(
        &self,
        caller_extension_id: &str,
        request: ExtensionHttpRequest,
    ) -> Result<ExtensionHttpResponse, ExtensionError>;
}

/// 唯一 `astrcode.*` 能力实现。
pub struct HostRouter {
    llm: LlmGroup,
    session: SessionGroup,
    context: ContextGroup,
    workspace: WorkspaceGroup,
    process: ProcessGroup,
    network: NetworkGroup,
    extension_http: ExtensionHttpGroup,
}

impl HostRouter {
    pub fn from_backends(backends: HostBackends) -> Self {
        let HostBackends {
            main_llm,
            small_llm,
            event_reader,
            session_reader,
            default_working_dir,
            public_http_dispatcher,
            outbound_network,
        } = backends;
        Self {
            llm: LlmGroup::new(main_llm, small_llm),
            session: SessionGroup::new(event_reader, session_reader),
            context: ContextGroup,
            workspace: WorkspaceGroup::new(default_working_dir.clone()),
            process: ProcessGroup::new(default_working_dir),
            network: NetworkGroup::new(outbound_network),
            extension_http: ExtensionHttpGroup::new(public_http_dispatcher),
        }
    }

    pub fn with_public_http_dispatcher(
        mut self,
        dispatcher: Arc<dyn PublicHttpDispatcher>,
    ) -> Self {
        self.extension_http.set_dispatcher(dispatcher);
        self
    }

    /// 根据已声明能力生成握手 catalog。
    pub fn catalog_for_grants(caps: &[ExtensionCapability]) -> Vec<CapabilityDescriptor> {
        capability::catalog_for_grants(caps)
    }

    /// Reports the operations whose concrete backend is usable for this call context.
    /// Authorization remains a separate check against the canonical SDK catalog.
    pub(crate) fn available_operations(
        &self,
        ctx: &InvokeContext,
    ) -> Vec<astrcode_extension_sdk::host::HostOperation> {
        capability::available_operations(self, ctx)
    }

    pub(crate) fn outbound_network_service(&self) -> Option<Arc<dyn OutboundNetworkService>> {
        self.network.service()
    }

    /// Executes one guest-to-host operation on the caller's async task.
    pub async fn invoke(
        &self,
        cap: &str,
        input: Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        ensure_invoke_active(ctx)?;
        let spec = capability::lookup(cap)?;
        capability::authorize(spec, &ctx.declared_capabilities)?;
        ensure_required_context(spec.operation, ctx)?;

        // Keep the router future bounded: each capability group has a distinct async state machine.
        let invoke = async {
            match spec.capability {
                HostCapability::Llm(capability) => {
                    Box::pin(
                        self.llm
                            .invoke(capability, input, ctx.cancel_token.as_ref()),
                    )
                    .await
                },
                HostCapability::Session(capability) => {
                    Box::pin(self.session.invoke(capability, input, ctx)).await
                },
                HostCapability::Context(capability) => {
                    Box::pin(self.context.invoke(capability, &input, ctx)).await
                },
                HostCapability::Workspace(capability) => {
                    Box::pin(self.workspace.invoke(
                        capability,
                        input,
                        ctx.working_dir.as_deref(),
                        ctx.tasks.as_ref(),
                    ))
                    .await
                },
                HostCapability::Process(capability) => {
                    Box::pin(self.process.invoke(
                        capability,
                        input,
                        ctx.working_dir.as_deref(),
                        ctx.cancel_token.as_ref(),
                    ))
                    .await
                },
                HostCapability::Network(capability) => {
                    Box::pin(
                        self.network
                            .invoke(capability, input, ctx.cancel_token.as_ref()),
                    )
                    .await
                },
                HostCapability::ExtensionHttp(capability) => {
                    Box::pin(
                        self.extension_http
                            .invoke(capability, input, &ctx.extension_id),
                    )
                    .await
                },
            }
        };
        // ProcessRunner must observe cancellation so it can terminate the process group and reap
        // the direct child before returning.
        let backend_owns_cancellation = matches!(
            spec.capability,
            HostCapability::Process(ProcessCapability::Spawn)
        );
        if spec.cancelable && !backend_owns_cancellation {
            if let Some(token) = &ctx.cancel_token {
                return tokio::select! {
                    biased;
                    () = token.cancelled() => {
                        Err(ErrorPayload::new(WireErrorCode::Cancelled, "invoke cancelled"))
                    },
                    output = invoke => output,
                };
            }
        }
        invoke.await
    }

    /// 流式 invoke：返回 Event 序列（`started` + `delta*` + `completed`/`failed`）。
    pub async fn invoke_stream(
        &self,
        cap: &str,
        input: Value,
        request_id: &str,
        ctx: &InvokeContext,
    ) -> Result<Vec<WireMessage>, ErrorPayload> {
        ensure_invoke_active(ctx)?;
        let spec = capability::lookup(cap)?;
        capability::authorize(spec, &ctx.declared_capabilities)?;
        ensure_required_context(spec.operation, ctx)?;
        if !spec.supports_stream {
            return Err(ErrorPayload::new(
                WireErrorCode::StreamNotSupported,
                format!("stream not supported for {cap}"),
            ));
        }
        let request_id = request_id.to_string();

        match spec.capability {
            HostCapability::Llm(capability) => {
                let invoke = self
                    .llm
                    .invoke_stream(capability, input, ctx.cancel_token.as_ref())
                    .await;
                let mut events = vec![WireMessage::Event(EventMsg {
                    id: request_id.clone(),
                    phase: EventPhase::Started,
                    data: Value::Null,
                    output: Value::Null,
                    error: None,
                })];
                match invoke {
                    Ok(output) => {
                        if let Some(chunks) = output.get("chunks").and_then(|c| c.as_array()) {
                            for chunk in chunks {
                                events.push(WireMessage::Event(EventMsg {
                                    id: request_id.clone(),
                                    phase: EventPhase::Delta,
                                    data: chunk.clone(),
                                    output: Value::Null,
                                    error: None,
                                }));
                            }
                        }
                        events.push(WireMessage::Event(EventMsg {
                            id: request_id,
                            phase: EventPhase::Completed,
                            data: output.clone(),
                            output,
                            error: None,
                        }));
                        Ok(events)
                    },
                    Err(error) => {
                        events.push(WireMessage::Event(EventMsg {
                            id: request_id,
                            phase: EventPhase::Failed,
                            data: Value::Null,
                            output: Value::Null,
                            error: Some(error),
                        }));
                        Ok(events)
                    },
                }
            },
            HostCapability::Session(_)
            | HostCapability::Context(_)
            | HostCapability::Workspace(_)
            | HostCapability::Process(_)
            | HostCapability::Network(_)
            | HostCapability::ExtensionHttp(_) => Err(ErrorPayload::new(
                WireErrorCode::InvalidCapabilityRegistry,
                format!("streaming capability {cap} has no stream handler"),
            )),
        }
    }
}

fn ensure_required_context(
    operation: astrcode_extension_sdk::host::HostOperation,
    ctx: &InvokeContext,
) -> Result<(), ErrorPayload> {
    if operation.requires_session_context() && ctx.session_id.is_none() {
        return Err(ErrorPayload::new(
            WireErrorCode::ContextUnavailable,
            format!(
                "{} requires a session-scoped call context",
                operation.wire_name()
            ),
        ));
    }
    if operation.requires_workspace_context() && ctx.working_dir.is_none() {
        return Err(ErrorPayload::new(
            WireErrorCode::ContextUnavailable,
            format!(
                "{} requires a workspace-scoped call context",
                operation.wire_name()
            ),
        ));
    }
    Ok(())
}

pub async fn emit_for_sink_confirmed(
    extension_id: &str,
    declarations: &HashMap<String, ExtensionEventDecl>,
    event_tx: &EventSender,
    event_type: &str,
    schema_version: u32,
    payload: Value,
) -> Result<EventPublishReceipt, ExtensionError> {
    let payload = validated_extension_event_payload(
        extension_id,
        declarations,
        event_type,
        schema_version,
        payload,
    )?;
    event_tx
        .send_confirmed(payload)
        .await
        .map_err(ExtensionError::from)
}

fn validated_extension_event_payload(
    extension_id: &str,
    declarations: &HashMap<String, ExtensionEventDecl>,
    event_type: &str,
    schema_version: u32,
    payload: Value,
) -> Result<EventPayload, ExtensionError> {
    validate_emit(declarations, event_type, schema_version, &payload)?;
    let durable = declarations
        .get(event_type)
        .map(|declaration| declaration.durable)
        .ok_or_else(|| {
            ExtensionError::Internal(format!("undeclared extension event type: {event_type}"))
        })?;
    let event = ExtensionEventData {
        extension_id: extension_id.to_owned(),
        event_type: event_type.to_owned(),
        schema_version,
        payload,
    };
    Ok(if durable {
        EventPayload::Durable(DurableEventPayload::ExtensionEvent(event))
    } else {
        EventPayload::Live(LiveEventPayload::ExtensionEvent(event))
    })
}

fn validate_emit(
    declarations: &HashMap<String, ExtensionEventDecl>,
    event_type: &str,
    schema_version: u32,
    payload: &Value,
) -> Result<(), ExtensionError> {
    let decl = declarations.get(event_type).ok_or_else(|| {
        ExtensionError::Internal(format!("undeclared extension event type: {event_type}"))
    })?;
    if schema_version != decl.schema_version {
        return Err(ExtensionError::Internal(format!(
            "schema_version {schema_version} does not match declared {} for {event_type}",
            decl.schema_version
        )));
    }
    let serialized =
        serde_json::to_string(payload).map_err(|e| ExtensionError::Internal(e.to_string()))?;
    if serialized.len() > decl.max_payload_bytes {
        return Err(ExtensionError::Internal(format!(
            "payload exceeds {} bytes for {event_type}",
            decl.max_payload_bytes
        )));
    }
    Ok(())
}

pub fn decls_to_map(decls: &[ExtensionEventDecl]) -> HashMap<String, ExtensionEventDecl> {
    decls
        .iter()
        .map(|d| (d.event_type.clone(), d.clone()))
        .collect()
}

pub fn build_host_router(backends: HostBackends) -> Arc<HostRouter> {
    Arc::new(HostRouter::from_backends(backends))
}

/// 构造 trusted bundled extensions 与 worker 共用的受限出站网络服务。
pub fn default_outbound_network_service() -> Arc<dyn OutboundNetworkService> {
    Arc::new(network::RestrictedNetworkService::default())
}

pub fn build_host_router_with_public_http_dispatcher(
    backends: HostBackends,
    dispatcher: Arc<dyn PublicHttpDispatcher>,
) -> Arc<HostRouter> {
    Arc::new(HostRouter::from_backends(backends).with_public_http_dispatcher(dispatcher))
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{
            Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use astrcode_core::{
        event::{
            DurableEvent, DurableEventPayload, ParentSessionRef, PersistedSystemPrompt,
            SessionStarted, SystemPromptSource,
        },
        llm::{LlmContent, LlmEvent, LlmMessage, LlmProvider, LlmTokenUsage, ModelLimits},
        permission::ApprovalDecision,
        tool::{
            CreateRootSessionRequest, CreateSessionRequest, SessionAccess, SessionApiError,
            SessionDeliveryOutcome, SessionHandle, SessionLifecycleState, SessionOperations,
            SessionReactivation, SessionState, SessionStatus, SessionToolSelection,
            SubmitTurnRequest, SubmitTurnResult,
        },
        types::MessageId,
    };
    use astrcode_extension_sdk::host::{
        HOST_NETWORK_MAX_BYTES, HOST_NETWORK_MAX_REQUEST_BODY_BYTES, HOST_NETWORK_MAX_TIMEOUT_MS,
        HOST_PROCESS_MAX_TIMEOUT_MS, HOST_SESSION_STATE_KEY_MAX_LENGTH,
        HOST_SESSION_STATE_VALUE_MAX_BYTES, HostLlmChatOutput, HostLlmChatRequest,
        HostLlmCollectedStreamOutput, HostNetworkRedirectPolicy, HostNetworkRequest,
        HostNetworkResponse, HostProcessRequest, HostWorkspaceGrepRequest,
    };
    use astrcode_storage::{
        EventReader, SessionEventJournal, SessionReader, StorageError,
        in_memory::InMemoryEventStore,
    };
    use serde_json::json;
    use tokio::sync::Notify;

    use super::*;

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct MissingSessionReader;

    #[async_trait::async_trait]
    impl SessionReader for MissingSessionReader {
        async fn session_read_model(
            &self,
            session_id: &astrcode_core::types::SessionId,
        ) -> Result<Arc<astrcode_session_projection::SessionReadModel>, StorageError> {
            Err(StorageError::NotFound(session_id.clone()))
        }

        async fn recycled_session_read_model(
            &self,
            session_id: &astrcode_core::types::SessionId,
        ) -> Result<Arc<astrcode_session_projection::SessionReadModel>, StorageError> {
            Err(StorageError::NotFound(session_id.clone()))
        }

        async fn list_session_summaries(
            &self,
        ) -> Result<Vec<astrcode_session_projection::SessionSummary>, StorageError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn persistent_blocking_io_remains_owned_after_the_caller_is_dropped() {
        let tasks = ExtensionTasks::new("persistent-io-test");
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let completed = Arc::new(AtomicBool::new(false));
        let completed_in_task = Arc::clone(&completed);
        let caller_tasks = tasks.clone();
        let caller = tokio::spawn(async move {
            run_blocking_io_to_completion(Some(&caller_tasks), "persistent-write", move || {
                started_tx.send(()).expect("signal blocking write start");
                release_rx.recv().expect("release blocking write");
                completed_in_task.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await
        });

        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocking write should start");
        caller.abort();
        assert!(
            caller
                .await
                .expect_err("caller should be aborted")
                .is_cancelled()
        );

        tasks.cancel();
        let draining_tasks = tasks.clone();
        let drain =
            tokio::spawn(async move { draining_tasks.wait(Duration::from_millis(20)).await });
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!drain.is_finished(), "retirement must wait for host writes");
        assert!(!completed.load(Ordering::SeqCst));

        release_tx.send(()).expect("finish blocking write");
        assert!(drain.await.expect("drain task should not panic"));
        assert!(completed.load(Ordering::SeqCst));
    }

    #[test]
    fn extension_event_emission_requires_the_declared_version_and_payload_bound() {
        let declarations = decls_to_map(&[ExtensionEventDecl {
            event_type: "probe.completed".into(),
            schema_version: 2,
            durable: true,
            max_payload_bytes: 8,
        }]);

        assert!(validate_emit(&declarations, "probe.completed", 2, &json!({})).is_ok());
        for (event_type, version, payload) in [
            ("probe.completed", 1, json!({})),
            ("probe.completed", 3, json!({})),
            ("probe.completed", 2, json!({ "too": "large" })),
            ("undeclared", 2, json!({})),
        ] {
            assert!(
                validate_emit(&declarations, event_type, version, &payload).is_err(),
                "{event_type} v{version} must be rejected"
            );
        }
    }

    #[test]
    fn catalog_includes_session_control_subcaps() {
        let caps = HostRouter::catalog_for_grants(&[ExtensionCapability::SessionControl]);
        let names: Vec<_> = caps.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"astrcode.session.control.create"));
        assert!(names.contains(&"astrcode.session.control.configure_tools"));
        assert!(names.contains(&"astrcode.session.control.state"));
        assert!(names.contains(&"astrcode.session.control.reactivate"));
    }

    #[test]
    fn catalog_includes_session_inspect_surface() {
        let caps = HostRouter::catalog_for_grants(&[ExtensionCapability::SessionInspect]);
        let names = caps
            .iter()
            .map(|descriptor| descriptor.name.as_str())
            .collect::<Vec<_>>();

        assert!(names.contains(&"astrcode.session.inspect.list"));
        assert!(names.contains(&"astrcode.session.inspect.snapshot"));
        assert!(names.contains(&"astrcode.session.inspect.read_model"));
        assert!(names.contains(&"astrcode.session.inspect.provider_messages"));
    }

    #[test]
    fn catalog_includes_typed_session_history_surface() {
        let caps = HostRouter::catalog_for_grants(&[ExtensionCapability::SessionHistory]);
        let names = caps
            .iter()
            .map(|descriptor| descriptor.name.as_str())
            .collect::<Vec<_>>();
        for expected in [
            "astrcode.session.history.list",
            "astrcode.session.history.provider_messages",
            "astrcode.session.history.snapshot",
            "astrcode.session.history.token_usage",
            "astrcode.session.history.transcript",
            "astrcode.session.read_events",
        ] {
            assert!(names.contains(&expected), "missing {expected}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_inspect_maps_storage_model_to_wire_contract() {
        let store = Arc::new(InMemoryEventStore::new());
        let session_id = astrcode_core::types::SessionId::new("inspect-session");
        store
            .create_session(DurableEvent::session(
                session_id.clone(),
                DurableEventPayload::SessionStarted(SessionStarted {
                    working_dir: "/workspace".into(),
                    model_id: "test-model".into(),
                    parent: None,
                    tool_selection: SessionToolSelection::default(),
                    source_extension: None,
                    initial_system_prompt: PersistedSystemPrompt {
                        text: "system".into(),
                        fingerprint: "fingerprint".into(),
                        extra_system_prompt: None,
                        source: SystemPromptSource::Native,
                    },
                }),
            ))
            .await
            .expect("create session");
        for payload in [
            DurableEventPayload::UserMessage {
                message_id: MessageId::new("user-1"),
                text: "hello".into(),
                attachments: Vec::new(),
                accepted_seq: None,
            },
            DurableEventPayload::AssistantMessageCompleted {
                message_id: MessageId::new("assistant-1"),
                text: "world".into(),
                reasoning_content: Some("reasoning".into()),
            },
            DurableEventPayload::TurnCompleted {
                finish_reason: "stop".into(),
            },
            DurableEventPayload::TokenUsageRecorded {
                usage: LlmTokenUsage {
                    input_tokens: Some(120),
                    cached_input_tokens: Some(20),
                    output_tokens: Some(30),
                    ..Default::default()
                },
                model_context_window: 8_192,
            },
        ] {
            store
                .append_event(DurableEvent::session(session_id.clone(), payload))
                .await
                .expect("append history event");
        }
        let event_reader: Arc<dyn EventReader> = store.clone();
        let session_reader: Arc<dyn SessionReader> = store;
        let router = HostRouter::from_backends(HostBackends {
            event_reader: Some(event_reader),
            session_reader: Some(session_reader),
            ..Default::default()
        });
        let ctx = InvokeContext {
            extension_id: "memory".into(),
            session_id: Some("inspect-session".into()),
            declared_capabilities: vec![
                ExtensionCapability::SessionHistory,
                ExtensionCapability::SessionInspect,
            ],
            ..Default::default()
        };

        let list = router
            .invoke("astrcode.session.inspect.list", json!({}), &ctx)
            .await
            .expect("list sessions");
        assert_eq!(list["sessions"][0]["sessionId"], "inspect-session");

        let model = router
            .invoke(
                "astrcode.session.inspect.read_model",
                json!({ "session_id": "inspect-session" }),
                &ctx,
            )
            .await
            .expect("read session model");
        assert_eq!(model["readModel"]["modelId"], "test-model");
        assert_eq!(model["readModel"]["phase"], "idle");

        let snapshot = router
            .invoke(
                "astrcode.session.inspect.snapshot",
                json!({ "session_id": "inspect-session" }),
                &ctx,
            )
            .await
            .expect("read session snapshot");
        assert_eq!(snapshot["snapshot"]["sessionId"], "inspect-session");

        let inspect_messages = router
            .invoke(
                "astrcode.session.inspect.provider_messages",
                json!({ "session_id": "inspect-session" }),
                &ctx,
            )
            .await
            .expect("read provider-visible session messages");
        assert_eq!(inspect_messages["messages"].as_array().unwrap().len(), 2);

        for (capability, input) in [
            (
                "astrcode.session.inspect.list",
                json!({ "unexpected": true }),
            ),
            (
                "astrcode.session.inspect.snapshot",
                json!({ "session_id": "inspect-session", "unexpected": true }),
            ),
            (
                "astrcode.session.inspect.read_model",
                json!({ "session_id": "inspect-session", "unexpected": true }),
            ),
            (
                "astrcode.session.inspect.provider_messages",
                json!({ "session_id": "inspect-session", "unexpected": true }),
            ),
            (
                "astrcode.session.inspect.snapshot",
                json!({ "session_id": "" }),
            ),
            (
                "astrcode.session.inspect.read_model",
                json!({ "session_id": "" }),
            ),
            (
                "astrcode.session.inspect.provider_messages",
                json!({ "session_id": "" }),
            ),
        ] {
            let error = router
                .invoke(capability, input.clone(), &ctx)
                .await
                .expect_err("inspect input must match its published schema");
            assert_eq!(
                error.code_enum(),
                Some(WireErrorCode::InvalidInput),
                "capability: {capability}"
            );
        }

        let history = router
            .invoke(
                "astrcode.session.history.snapshot",
                json!({ "target_session_id": "inspect-session" }),
                &ctx,
            )
            .await
            .expect("read scoped history snapshot");
        assert_eq!(history["lifecycle"], "active");
        assert_eq!(history["readModel"]["modelId"], "test-model");

        let summaries = router
            .invoke("astrcode.session.history.list", json!({}), &ctx)
            .await
            .expect("list session history summaries");
        assert_eq!(summaries["sessions"][0]["session_id"], "inspect-session");
        assert_eq!(summaries["sessions"][0]["latest_cursor"], "4");

        let target = json!({ "target_session_id": "inspect-session" });
        let transcript = router
            .invoke("astrcode.session.history.transcript", target.clone(), &ctx)
            .await
            .expect("read extension-visible transcript");
        assert_eq!(transcript["messages"][0]["message"]["role"], "user");
        assert_eq!(transcript["messages"][1]["message"]["role"], "assistant");

        let provider_messages = router
            .invoke(
                "astrcode.session.history.provider_messages",
                target.clone(),
                &ctx,
            )
            .await
            .expect("read provider-visible history");
        assert_eq!(provider_messages["messages"].as_array().unwrap().len(), 2);

        let usage = router
            .invoke("astrcode.session.history.token_usage", target.clone(), &ctx)
            .await
            .expect("read token usage");
        assert_eq!(usage["usage"]["total_tokens"], 130);
        assert_eq!(usage["usage"]["model_context_window"], 8_192);

        let missing_router = HostRouter::from_backends(HostBackends {
            session_reader: Some(Arc::new(MissingSessionReader)),
            ..Default::default()
        });
        let missing_history = missing_router
            .invoke(
                "astrcode.session.history.snapshot",
                json!({ "target_session_id": "missing-session" }),
                &InvokeContext {
                    extension_id: "memory".into(),
                    session_id: Some("missing-session".into()),
                    declared_capabilities: vec![ExtensionCapability::SessionHistory],
                    ..Default::default()
                },
            )
            .await
            .expect_err("missing recycled history must use the stable not-found code");
        assert_eq!(
            missing_history.code_enum(),
            Some(WireErrorCode::SessionNotFound)
        );

        let missing_attribution = router
            .invoke(
                "astrcode.session.history.transcript",
                target.clone(),
                &InvokeContext {
                    session_id: Some("inspect-session".into()),
                    declared_capabilities: vec![ExtensionCapability::SessionHistory],
                    ..Default::default()
                },
            )
            .await
            .expect_err("history reads require host-owned extension attribution");
        assert_eq!(
            missing_attribution.code_enum(),
            Some(WireErrorCode::ContextUnavailable)
        );
    }

    #[test]
    fn input_delivery_catalog_lists_root_session_operations() {
        let root_caps = HostRouter::catalog_for_grants(&[ExtensionCapability::InputDelivery]);
        let root_names = root_caps
            .iter()
            .map(|descriptor| descriptor.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            root_names,
            [
                "astrcode.session.root.create",
                "astrcode.session.root.state",
                "astrcode.session.root.submit_turn",
                "astrcode.session.state.read",
                "astrcode.session.state.write",
            ]
        );
    }

    #[tokio::test]
    async fn bounded_io_contracts_reject_unknown_fields() {
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("edit.txt"), "old value").expect("seed workspace");
        let router = HostRouter::from_backends(HostBackends {
            default_working_dir: Some(workspace.path().to_string_lossy().into_owned()),
            ..Default::default()
        });
        let ctx = InvokeContext {
            tasks: Some(ExtensionTasks::new("strict-input-test")),
            working_dir: Some(workspace.path().to_string_lossy().into_owned()),
            declared_capabilities: vec![
                ExtensionCapability::WorkspaceRead,
                ExtensionCapability::WorkspaceWrite,
                ExtensionCapability::NetworkClient,
                ExtensionCapability::ProcessSpawn,
            ],
            ..Default::default()
        };
        let cases = [
            (
                "astrcode.process.spawn",
                json!({ "command": "rustc", "timeot_ms": 1 }),
            ),
            (
                "astrcode.network.client",
                json!({ "url": "https://example.com", "timeot_ms": 1 }),
            ),
            (
                "astrcode.workspace.read",
                json!({ "path": "edit.txt", "max_btyes": 1 }),
            ),
            (
                "astrcode.workspace.write",
                json!({ "path": "new.txt", "content": "new", "contnet": "typo" }),
            ),
            (
                "astrcode.workspace.edit",
                json!({
                    "path": "edit.txt",
                    "old_text": "old",
                    "new_text": "new",
                    "replaceAll": true
                }),
            ),
            (
                "astrcode.workspace.list",
                json!({ "path": ".", "detph": 2 }),
            ),
            (
                "astrcode.workspace.grep",
                json!({ "pattern": "old", "max_macthes": 1 }),
            ),
            (
                "astrcode.workspace.glob",
                json!({ "pattern": "*.txt", "includeIgnored": true }),
            ),
        ];

        for (operation, input) in cases {
            let error = router
                .invoke(operation, input.clone(), &ctx)
                .await
                .expect_err("unknown request fields must be rejected");
            assert_eq!(
                error.code_enum(),
                Some(WireErrorCode::InvalidInput),
                "{operation}"
            );
            assert!(
                error.message.contains("unknown field"),
                "{operation}: {}",
                error.message
            );
        }

        let process_with_nulls: HostProcessRequest = serde_json::from_value(json!({
            "command": "rustc",
            "cwd": null,
            "stdin": null,
            "timeout_ms": null
        }))
        .expect("process options accept explicit null");
        assert_eq!(process_with_nulls.cwd, None);
        assert_eq!(process_with_nulls.stdin, None);
        assert_eq!(process_with_nulls.timeout_ms, None);
        let canonical_process =
            serde_json::to_value(&process_with_nulls).expect("serialize process request");
        for property in ["cwd", "stdin", "timeout_ms"] {
            assert!(canonical_process.get(property).is_none());
        }
        assert_eq!(
            serde_json::from_value::<HostProcessRequest>(canonical_process)
                .expect("deserialize canonical process request"),
            process_with_nulls
        );

        let grep_with_nulls: HostWorkspaceGrepRequest = serde_json::from_value(json!({
            "pattern": "needle",
            "path": null,
            "max_matches": null,
            "max_bytes": null,
            "max_line_chars": null
        }))
        .expect("workspace grep options accept explicit null");
        assert_eq!(grep_with_nulls.path, None);
        assert_eq!(grep_with_nulls.max_matches, None);
        assert_eq!(grep_with_nulls.max_bytes, None);
        assert_eq!(grep_with_nulls.max_line_chars, None);
        let canonical_grep =
            serde_json::to_value(&grep_with_nulls).expect("serialize workspace grep request");
        for property in ["path", "max_matches", "max_bytes", "max_line_chars"] {
            assert!(canonical_grep.get(property).is_none());
        }
        assert_eq!(
            serde_json::from_value::<HostWorkspaceGrepRequest>(canonical_grep)
                .expect("deserialize canonical workspace grep request"),
            grep_with_nulls
        );

        for timeout_ms in [0, HOST_PROCESS_MAX_TIMEOUT_MS + 1] {
            let error = router
                .invoke(
                    "astrcode.process.spawn",
                    json!({ "command": "rustc", "timeout_ms": timeout_ms }),
                    &ctx,
                )
                .await
                .expect_err("out-of-range process timeouts must be rejected");
            assert_eq!(
                error.code_enum(),
                Some(WireErrorCode::InvalidInput),
                "timeout_ms={timeout_ms}"
            );
        }

        assert!(!workspace.path().join("new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("edit.txt")).expect("read workspace"),
            "old value"
        );
    }

    #[tokio::test]
    async fn network_capability_rejects_non_http_urls_when_declared() {
        let router = HostRouter::from_backends(HostBackends {
            outbound_network: Some(default_outbound_network_service()),
            ..Default::default()
        });
        let ctx = InvokeContext {
            declared_capabilities: vec![ExtensionCapability::NetworkClient],
            ..Default::default()
        };
        let err = router
            .invoke(
                "astrcode.network.client",
                json!({ "url": "file:///etc/passwd" }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code_enum(), Some(WireErrorCode::PermissionDenied));
    }

    #[tokio::test]
    async fn network_registry_preserves_rich_binary_request_and_structured_authorization() {
        let network = Arc::new(FakeOutboundNetwork::default());
        let router = HostRouter::from_backends(HostBackends {
            outbound_network: Some(network.clone()),
            ..Default::default()
        });
        let mut request = HostNetworkRequest::get("https://example.com/start");
        request.method = "POST".into();
        request.headers.insert("x-test".into(), "typed".into());
        request.body = vec![0, 255, 1];
        request.max_bytes = HOST_NETWORK_MAX_BYTES;
        request.timeout_ms = 55_000;
        request.redirect_policy = HostNetworkRedirectPolicy::Manual;
        let request = serde_json::to_value(&request).expect("serialize network request");
        let allowed = InvokeContext {
            declared_capabilities: vec![ExtensionCapability::NetworkClient],
            ..Default::default()
        };

        let response = router
            .invoke("astrcode.network.client", request.clone(), &allowed)
            .await
            .expect("declared network capability");
        let response = serde_json::from_value::<HostNetworkResponse>(response)
            .expect("deserialize binary network response");
        assert_eq!(response.final_url, "https://example.com/final");
        assert_eq!(response.body, vec![255, 0, 1]);
        assert_eq!(network.calls.load(Ordering::SeqCst), 1);
        {
            let captured = network.request.lock().expect("network request");
            let captured = captured.as_ref().expect("captured request");
            assert_eq!(captured.method, "POST");
            assert_eq!(captured.headers["x-test"], "typed");
            assert_eq!(captured.body, vec![0, 255, 1]);
            assert_eq!(captured.max_bytes, HOST_NETWORK_MAX_BYTES);
            assert_eq!(captured.timeout, Duration::from_secs(55));
            assert_eq!(
                captured.redirect_policy,
                astrcode_extension_sdk::host::internal::NetworkRedirectPolicy::Manual
            );
        }

        for invalid in [
            HostNetworkRequest {
                max_bytes: HOST_NETWORK_MAX_BYTES + 1,
                ..HostNetworkRequest::get("https://example.com")
            },
            HostNetworkRequest {
                timeout_ms: 0,
                ..HostNetworkRequest::get("https://example.com")
            },
            HostNetworkRequest {
                timeout_ms: HOST_NETWORK_MAX_TIMEOUT_MS + 1,
                ..HostNetworkRequest::get("https://example.com")
            },
        ] {
            let error = router
                .invoke(
                    "astrcode.network.client",
                    serde_json::to_value(&invalid).expect("serialize invalid request"),
                    &allowed,
                )
                .await
                .expect_err("network bounds must match the published schema");
            assert_eq!(error.code_enum(), Some(WireErrorCode::InvalidInput));
        }

        let oversized_body = "AAAA".repeat(HOST_NETWORK_MAX_REQUEST_BODY_BYTES / 3 + 1);
        let error = router
            .invoke(
                "astrcode.network.client",
                json!({ "url": "https://example.com", "body": oversized_body }),
                &allowed,
            )
            .await
            .expect_err("oversized outbound body must be rejected before the network service");
        assert_eq!(error.code_enum(), Some(WireErrorCode::InvalidInput));
        assert_eq!(network.calls.load(Ordering::SeqCst), 1);

        let denied = router
            .invoke(
                "astrcode.network.client",
                request.clone(),
                &InvokeContext::default(),
            )
            .await
            .expect_err("missing network grant");
        assert_eq!(denied.code_enum(), Some(WireErrorCode::PermissionDenied));
        assert_eq!(network.calls.load(Ordering::SeqCst), 1);

        let unknown = router
            .invoke("astrcode.network.unknown", json!({}), &allowed)
            .await
            .expect_err("unknown capability");
        assert_eq!(unknown.code_enum(), Some(WireErrorCode::UnknownCapability));
    }

    #[tokio::test]
    async fn session_state_api_is_capability_free_strict_and_collision_safe() {
        let router = HostRouter::from_backends(HostBackends::default());
        let temp = tempfile::tempdir().expect("tempdir");
        let ctx = InvokeContext {
            extension_id: "stateful-test".into(),
            session_id: Some("stateful-test-session".into()),
            session_store_dir: Some(temp.path().to_path_buf()),
            tasks: Some(ExtensionTasks::new("stateful-test")),
            declared_capabilities: Vec::new(),
            ..Default::default()
        };

        router
            .invoke(
                "astrcode.session.state.write",
                json!({ "key": "goal", "content": "active" }),
                &ctx,
            )
            .await
            .expect("write state without capability");
        let read = router
            .invoke(
                "astrcode.session.state.read",
                json!({ "key": "goal" }),
                &ctx,
            )
            .await
            .expect("read state without capability");

        assert_eq!(read["content"], "active");

        let missing = router
            .invoke(
                "astrcode.session.state.read",
                json!({ "key": "missing" }),
                &ctx,
            )
            .await
            .expect("read missing state");
        assert!(missing["content"].is_null());

        router
            .invoke(
                "astrcode.session.state.write",
                json!({ "key": "empty", "content": "" }),
                &ctx,
            )
            .await
            .expect("write empty state");
        let empty = router
            .invoke(
                "astrcode.session.state.read",
                json!({ "key": "empty" }),
                &ctx,
            )
            .await
            .expect("read stored empty state");
        assert_eq!(empty["content"], "");

        for (capability, input) in [
            ("astrcode.session.state.write", json!({ "key": "missing" })),
            (
                "astrcode.session.state.write",
                json!({ "key": "goal", "content": "hidden", "unexpected": true }),
            ),
            (
                "astrcode.session.state.read",
                json!({ "key": "goal", "unexpected": true }),
            ),
            (
                "astrcode.session.state.write",
                json!({ "key": "a/b", "content": "collision" }),
            ),
            (
                "astrcode.session.state.write",
                json!({ "key": "..", "content": "escape" }),
            ),
            (
                "astrcode.session.state.write",
                json!({
                    "key": "x".repeat(HOST_SESSION_STATE_KEY_MAX_LENGTH + 1),
                    "content": "oversized"
                }),
            ),
            (
                "astrcode.session.state.write",
                json!({
                    "key": "goal",
                    "content": "x".repeat(HOST_SESSION_STATE_VALUE_MAX_BYTES + 1)
                }),
            ),
        ] {
            let error = router
                .invoke(capability, input.clone(), &ctx)
                .await
                .expect_err("invalid state contract must be rejected");
            assert_eq!(
                error.code_enum(),
                Some(WireErrorCode::InvalidInput),
                "input: {input}"
            );
        }

        router
            .invoke(
                "astrcode.session.state.write",
                json!({ "key": "a_b", "content": "distinct" }),
                &ctx,
            )
            .await
            .expect("valid normalized-looking key");
        let distinct = router
            .invoke("astrcode.session.state.read", json!({ "key": "a_b" }), &ctx)
            .await
            .expect("read valid key");
        assert_eq!(distinct["content"], "distinct");

        let unchanged = router
            .invoke(
                "astrcode.session.state.read",
                json!({ "key": "goal" }),
                &ctx,
            )
            .await
            .expect("oversized write must not replace existing state");
        assert_eq!(unchanged["content"], "active");

        let legacy_path = temp
            .path()
            .join("extension_data/stateful-test/oversized-existing");
        std::fs::write(
            legacy_path,
            vec![b'x'; HOST_SESSION_STATE_VALUE_MAX_BYTES + 1],
        )
        .expect("write oversized pre-existing state");
        let error = router
            .invoke(
                "astrcode.session.state.read",
                json!({ "key": "oversized-existing" }),
                &ctx,
            )
            .await
            .expect_err("persisted state must be revalidated at the read boundary");
        assert_eq!(error.code_enum(), Some(WireErrorCode::StateTooLarge));
    }

    #[tokio::test]
    async fn invoke_rejects_precancelled_token() {
        let router = HostRouter::from_backends(HostBackends::default());
        let token = CancellationToken::new();
        token.cancel();
        let ctx = InvokeContext {
            cancel_token: Some(token),
            declared_capabilities: vec![ExtensionCapability::WorkspaceRead],
            working_dir: Some("/tmp".into()),
            ..Default::default()
        };
        let err = router
            .invoke("astrcode.workspace.read", json!({ "path": "x" }), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code_enum(), Some(WireErrorCode::Cancelled));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn invoke_cancellation_terminates_process_group() {
        let workspace = tempfile::tempdir().expect("workspace");
        let heartbeat = workspace.path().join("heartbeat");
        let router = Arc::new(HostRouter::from_backends(HostBackends {
            default_working_dir: Some(workspace.path().to_string_lossy().into_owned()),
            ..Default::default()
        }));
        let cancel_token = CancellationToken::new();
        let ctx = InvokeContext {
            cancel_token: Some(cancel_token.clone()),
            working_dir: Some(workspace.path().to_string_lossy().into_owned()),
            declared_capabilities: vec![ExtensionCapability::ProcessSpawn],
            ..Default::default()
        };
        let invocation = tokio::spawn(async move {
            router
                .invoke(
                    "astrcode.process.spawn",
                    json!({
                        "command": "/bin/sh",
                        "args": [
                            "-c",
                            "while :; do printf x >> heartbeat; sleep 0.05; done & wait"
                        ],
                        "timeout_ms": 10_000
                    }),
                    &ctx,
                )
                .await
        });

        let heartbeat_started = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    tokio::fs::metadata(&heartbeat).await,
                    Ok(metadata) if metadata.len() > 0
                ) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;

        cancel_token.cancel();
        let error = tokio::time::timeout(Duration::from_secs(5), invocation)
            .await
            .expect("cancelled host invoke should finish")
            .expect("host invoke task should not panic")
            .expect_err("process invoke should be cancelled");
        assert_eq!(error.code_enum(), Some(WireErrorCode::Cancelled));
        heartbeat_started.expect("descendant process should start before cancellation");

        tokio::time::sleep(Duration::from_millis(150)).await;
        let settled_len = tokio::fs::metadata(&heartbeat)
            .await
            .expect("heartbeat after cancellation")
            .len();
        tokio::time::sleep(Duration::from_millis(250)).await;
        let final_len = tokio::fs::metadata(&heartbeat)
            .await
            .expect("heartbeat remains inspectable")
            .len();
        assert_eq!(
            final_len, settled_len,
            "descendant process kept running after host invoke cancellation"
        );
    }

    #[tokio::test]
    async fn invoke_session_submit_rejects_wait_for_result_on_peer_io_thread() {
        let router = HostRouter::from_backends(HostBackends::default());
        let ctx = InvokeContext {
            declared_capabilities: vec![ExtensionCapability::SessionControl],
            session_id: Some("parent".into()),
            on_peer_io_thread: true,
            ..Default::default()
        };
        let err = router
            .invoke(
                "astrcode.session.control.submit_turn",
                json!({
                    "target_session_id": "child",
                    "user_prompt": "hello",
                    "wait_for_result": true
                }),
                &ctx,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code_enum(), Some(WireErrorCode::InvalidRequest));
    }

    #[tokio::test]
    async fn invoke_session_create_forwards_tool_selection() {
        let router = HostRouter::from_backends(HostBackends::default());
        let ops = Arc::new(CapturingSessionOps::default());
        let ctx = InvokeContext {
            extension_id: "test-extension".into(),
            session_id: Some("parent".into()),
            session_ops: Some(ops.clone()),
            declared_capabilities: vec![ExtensionCapability::SessionControl],
            ..Default::default()
        };

        let output = router
            .invoke(
                "astrcode.session.control.create",
                json!({
                    "name": "worker",
                    "tool_selection": {
                        "mode": "all",
                        "except": ["agent"]
                    }
                }),
                &ctx,
            )
            .await
            .expect("create child session");

        assert_eq!(output["session_id"], "child-1");
        let requests = ops.creates.lock().expect("creates lock");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].tool_call_id.is_none());
        assert_eq!(
            requests[0].tool_selection,
            Some(SessionToolSelection::All {
                except: vec!["agent".into()]
            })
        );
    }

    #[tokio::test]
    async fn invoke_session_control_uses_host_tool_call_provenance() {
        let router = HostRouter::from_backends(HostBackends::default());
        let ops = Arc::new(CapturingSessionOps::default());
        let ctx = InvokeContext {
            extension_id: "test-extension".into(),
            session_id: Some("parent".into()),
            tool_call_id: Some("host-call".into()),
            session_ops: Some(ops.clone()),
            declared_capabilities: vec![ExtensionCapability::SessionControl],
            ..Default::default()
        };

        router
            .invoke(
                "astrcode.session.control.create",
                json!({ "name": "worker" }),
                &ctx,
            )
            .await
            .expect("create child session");
        router
            .invoke(
                "astrcode.session.control.submit_turn",
                json!({
                    "target_session_id": "child-1",
                    "user_prompt": "run",
                    "wait_for_result": false
                }),
                &ctx,
            )
            .await
            .expect("submit child turn");

        assert_eq!(
            ops.creates.lock().expect("creates lock")[0]
                .tool_call_id
                .as_deref(),
            Some("host-call")
        );
        assert_eq!(
            ops.submits.lock().expect("submits lock")[0]
                .tool_call_id
                .as_deref(),
            Some("host-call")
        );

        for (operation, input) in [
            (
                "astrcode.session.control.create",
                json!({ "name": "spoofed", "tool_call_id": "guest-call" }),
            ),
            (
                "astrcode.session.control.submit_turn",
                json!({
                    "target_session_id": "child-1",
                    "user_prompt": "spoofed",
                    "wait_for_result": false,
                    "tool_call_id": "guest-call"
                }),
            ),
        ] {
            let error = router
                .invoke(operation, input.clone(), &ctx)
                .await
                .expect_err("guest tool_call_id must be rejected");
            assert_eq!(
                error.code_enum(),
                Some(WireErrorCode::InvalidInput),
                "{operation}"
            );
        }
    }

    #[tokio::test]
    async fn invoke_session_create_accepts_explicit_empty_tool_set() {
        let router = HostRouter::from_backends(HostBackends::default());
        let ops = Arc::new(CapturingSessionOps::default());
        let ctx = InvokeContext {
            session_id: Some("parent".into()),
            session_ops: Some(ops.clone()),
            declared_capabilities: vec![ExtensionCapability::SessionControl],
            ..Default::default()
        };

        let output = router
            .invoke(
                "astrcode.session.control.create",
                json!({
                    "name": "worker",
                    "tool_selection": {
                        "mode": "only",
                        "names": []
                    }
                }),
                &ctx,
            )
            .await
            .expect("create child session without tools");

        assert_eq!(output["session_id"], "child-1");
        let requests = ops.creates.lock().expect("creates lock");
        assert_eq!(
            requests[0].tool_selection,
            Some(SessionToolSelection::Only { names: Vec::new() })
        );
    }

    #[tokio::test]
    async fn invoke_session_configure_tools_validates_and_canonicalizes_selection() {
        let router = HostRouter::from_backends(HostBackends::default());
        let ops = Arc::new(CapturingSessionOps::default());
        let ctx = InvokeContext {
            session_id: Some("parent".into()),
            session_ops: Some(ops.clone()),
            declared_capabilities: vec![ExtensionCapability::SessionControl],
            ..Default::default()
        };

        let output = router
            .invoke(
                "astrcode.session.control.configure_tools",
                json!({
                    "session_id": "child",
                    "selection": {
                        "mode": "only",
                        "names": ["write", " read ", "write"]
                    }
                }),
                &ctx,
            )
            .await
            .expect("configure session tools");

        assert_eq!(
            output["selection"],
            json!({ "mode": "only", "names": ["read", "write"] })
        );
        assert_eq!(
            ops.tool_configurations
                .lock()
                .expect("tool configurations")
                .as_slice(),
            &[(
                "parent".into(),
                "child".into(),
                SessionToolSelection::Only {
                    names: vec!["read".into(), "write".into()]
                }
            )]
        );

        let error = router
            .invoke(
                "astrcode.session.control.configure_tools",
                json!({
                    "session_id": "child",
                    "selection": {
                        "mode": "only",
                        "names": ["read"],
                        "except": ["write"]
                    }
                }),
                &ctx,
            )
            .await
            .expect_err("cross-variant fields must be rejected");
        assert_eq!(error.code_enum(), Some(WireErrorCode::InvalidInput));
    }

    #[tokio::test]
    async fn invoke_session_inject_returns_delivery_outcome() {
        let router = HostRouter::from_backends(HostBackends::default());
        let ctx = InvokeContext {
            session_id: Some("parent".into()),
            session_ops: Some(Arc::new(CapturingSessionOps::default())),
            declared_capabilities: vec![ExtensionCapability::SessionControl],
            ..Default::default()
        };

        let output = router
            .invoke(
                "astrcode.session.control.inject_or_start",
                json!({
                    "target_session_id": "child",
                    "content": "continue"
                }),
                &ctx,
            )
            .await
            .expect("inject session input");

        assert_eq!(output["status"], "injected");
        assert_eq!(output["turn_id"], "turn-injected");
    }

    #[tokio::test]
    async fn invoke_session_lifecycle_apis_forward_scoped_target() {
        let router = HostRouter::from_backends(HostBackends::default());
        let ops = Arc::new(CapturingSessionOps::default());
        let ctx = InvokeContext {
            session_id: Some("parent".into()),
            session_ops: Some(ops.clone()),
            declared_capabilities: vec![ExtensionCapability::SessionControl],
            ..Default::default()
        };
        let input = json!({ "target_session_id": "child" });

        let state = router
            .invoke("astrcode.session.control.state", input.clone(), &ctx)
            .await
            .expect("read lifecycle state");
        assert_eq!(state["lifecycle"], "recycled");
        assert_eq!(state["message_count"], 2);

        let reactivation = router
            .invoke("astrcode.session.control.reactivate", input.clone(), &ctx)
            .await
            .expect("reactivate session");
        assert_eq!(reactivation["session_id"], "child");
        assert_eq!(reactivation["reactivated"], true);

        let cancellation = router
            .invoke("astrcode.session.control.cancel_turn", input.clone(), &ctx)
            .await
            .expect("cancel active turn");
        assert_eq!(cancellation, json!({ "cancelled": true }));

        for invalid in [
            json!({ "session_id": "child" }),
            json!({ "target_session_id": "child", "unexpected": true }),
        ] {
            let error = router
                .invoke(
                    "astrcode.session.control.cancel_turn",
                    invalid.clone(),
                    &ctx,
                )
                .await
                .expect_err("cancel_turn must enforce its strict request schema");
            assert_eq!(error.code_enum(), Some(WireErrorCode::InvalidInput));
        }
        assert_eq!(
            ops.lifecycle_calls
                .lock()
                .expect("lifecycle calls")
                .as_slice(),
            &[
                ("state".into(), "parent".into(), "child".into()),
                ("reactivate".into(), "parent".into(), "child".into())
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn root_input_delivery_is_attributed_and_scoped_while_event_pages_advance() {
        let store = Arc::new(InMemoryEventStore::new());
        seed_session(&store, "owned-root", None, Some("channel-a")).await;
        seed_session(&store, "foreign-root", None, Some("channel-b")).await;
        seed_session(&store, "owned-child", Some("owned-root"), Some("channel-a")).await;
        let owned_id = astrcode_core::types::SessionId::new("owned-root");
        for payload in [
            DurableEventPayload::TurnStarted,
            DurableEventPayload::TurnCompleted {
                finish_reason: "stop".into(),
            },
            DurableEventPayload::ModelIdChanged {
                model_id: "next-model".into(),
            },
        ] {
            store
                .append_event(DurableEvent::session(owned_id.clone(), payload))
                .await
                .expect("append event");
        }

        let ops = Arc::new(CapturingSessionOps::default());
        let event_reader: Arc<dyn EventReader> = store.clone();
        let session_reader: Arc<dyn SessionReader> = store;
        let router = HostRouter::from_backends(HostBackends {
            event_reader: Some(event_reader),
            session_reader: Some(session_reader),
            ..Default::default()
        });
        let root_ctx = InvokeContext {
            extension_id: "channel-a".into(),
            working_dir: Some("/workspace".into()),
            session_ops: Some(ops.clone()),
            declared_capabilities: vec![ExtensionCapability::InputDelivery],
            ..Default::default()
        };

        let spoof = router
            .invoke(
                "astrcode.session.root.create",
                json!({ "source_extension": "channel-b" }),
                &root_ctx,
            )
            .await
            .expect_err("source attribution is host-owned");
        assert_eq!(spoof.code_enum(), Some(WireErrorCode::InvalidInput));
        let created = router
            .invoke("astrcode.session.root.create", json!({}), &root_ctx)
            .await
            .expect("create attributed root");
        assert_eq!(created["session_id"], "root");
        {
            let root_creates = ops.root_creates.lock().expect("root creates lock");
            assert_eq!(root_creates.len(), 1);
            assert_eq!(
                root_creates[0].source_extension.as_deref(),
                Some("channel-a")
            );
        }

        let state = router
            .invoke(
                "astrcode.session.root.state",
                json!({ "target_session_id": "owned-root" }),
                &root_ctx,
            )
            .await
            .expect("read owned root state without caller session");
        assert_eq!(state["message_count"], 2);
        for target in ["foreign-root", "owned-child"] {
            let error = router
                .invoke(
                    "astrcode.session.root.state",
                    json!({ "target_session_id": target }),
                    &root_ctx,
                )
                .await
                .expect_err("foreign or child session must be rejected");
            assert_eq!(error.code_enum(), Some(WireErrorCode::PermissionDenied));
        }

        router
            .invoke(
                "astrcode.session.root.submit_turn",
                json!({
                    "target_session_id": "owned-root",
                    "user_prompt": "hello",
                    "wait_for_result": false
                }),
                &root_ctx,
            )
            .await
            .expect("submit to owned root");
        {
            let submits = ops.submits.lock().expect("submits lock");
            assert_eq!(submits.len(), 1);
            assert_eq!(submits[0].access.caller_session_id, "owned-root");
            assert_eq!(submits[0].access.target_session_id, "owned-root");
        }
        let denied = router
            .invoke(
                "astrcode.session.root.submit_turn",
                json!({
                    "target_session_id": "foreign-root",
                    "user_prompt": "hello",
                    "wait_for_result": false
                }),
                &root_ctx,
            )
            .await
            .expect_err("foreign root submit must be rejected");
        assert_eq!(denied.code_enum(), Some(WireErrorCode::PermissionDenied));
        assert_eq!(ops.submits.lock().expect("submits lock").len(), 1);

        let missing_grant = router
            .invoke(
                "astrcode.session.root.create",
                json!({}),
                &InvokeContext {
                    extension_id: "channel-a".into(),
                    session_ops: Some(ops),
                    ..Default::default()
                },
            )
            .await
            .expect_err("root creation needs input_delivery");
        assert_eq!(
            missing_grant.code_enum(),
            Some(WireErrorCode::PermissionDenied)
        );

        let missing_extension_identity = router
            .invoke(
                "astrcode.session.root.create",
                json!({}),
                &InvokeContext {
                    working_dir: Some("/workspace".into()),
                    session_ops: root_ctx.session_ops.clone(),
                    declared_capabilities: vec![ExtensionCapability::InputDelivery],
                    ..Default::default()
                },
            )
            .await
            .expect_err("root creation needs an attributed extension identity");
        assert_eq!(
            missing_extension_identity.code_enum(),
            Some(WireErrorCode::ContextUnavailable)
        );
        let missing_root_backend = router
            .invoke(
                "astrcode.session.root.create",
                json!({}),
                &InvokeContext {
                    extension_id: "channel-a".into(),
                    working_dir: Some("/workspace".into()),
                    declared_capabilities: vec![ExtensionCapability::InputDelivery],
                    ..Default::default()
                },
            )
            .await
            .expect_err("root creation needs session operations");
        assert_eq!(
            missing_root_backend.code_enum(),
            Some(WireErrorCode::BackendUnavailable)
        );

        let history_ctx = InvokeContext {
            extension_id: "history-test".into(),
            session_id: Some("owned-root".into()),
            declared_capabilities: vec![ExtensionCapability::SessionHistory],
            ..Default::default()
        };
        let first = router
            .invoke(
                "astrcode.session.read_events",
                json!({ "session_id": "owned-root", "limit": 2 }),
                &history_ctx,
            )
            .await
            .expect("first event page");
        assert_eq!(first["events"][0]["seq"], 1);
        assert_eq!(first["events"][1]["seq"], 2);
        assert_eq!(first["next_cursor"], "2");
        assert_eq!(first["has_more"], true);

        let next = router
            .invoke(
                "astrcode.session.read_events",
                json!({ "session_id": "owned-root", "cursor": "2", "limit": 2 }),
                &history_ctx,
            )
            .await
            .expect("next event page");
        assert_eq!(next["events"].as_array().expect("events").len(), 1);
        assert_eq!(next["events"][0]["seq"], 3);
        assert_eq!(next["next_cursor"], "3");
        assert_eq!(next["has_more"], false);

        let empty = router
            .invoke(
                "astrcode.session.read_events",
                json!({ "session_id": "owned-root", "cursor": "3", "limit": 2 }),
                &history_ctx,
            )
            .await
            .expect("empty terminal page");
        assert!(empty["events"].as_array().expect("events").is_empty());
        assert_eq!(empty["next_cursor"], "3");
        assert_eq!(empty["has_more"], false);

        let invalid_limit = router
            .invoke(
                "astrcode.session.read_events",
                json!({ "session_id": "owned-root", "limit": 0 }),
                &history_ctx,
            )
            .await
            .expect_err("zero event limit must be rejected");
        assert_eq!(invalid_limit.code_enum(), Some(WireErrorCode::InvalidInput));
        let invalid_cursor = router
            .invoke(
                "astrcode.session.read_events",
                json!({ "session_id": "owned-root", "cursor": "not-a-sequence" }),
                &history_ctx,
            )
            .await
            .expect_err("non-numeric event cursor must be rejected");
        assert_eq!(
            invalid_cursor.code_enum(),
            Some(WireErrorCode::InvalidInput)
        );
        let missing_context = router
            .invoke(
                "astrcode.session.read_events",
                json!({ "session_id": "owned-root" }),
                &InvokeContext {
                    declared_capabilities: vec![ExtensionCapability::SessionHistory],
                    ..Default::default()
                },
            )
            .await
            .expect_err("event history needs caller session context");
        assert_eq!(
            missing_context.code_enum(),
            Some(WireErrorCode::ContextUnavailable)
        );
        let missing_backend = HostRouter::from_backends(HostBackends::default())
            .invoke(
                "astrcode.session.read_events",
                json!({ "session_id": "owned-root" }),
                &history_ctx,
            )
            .await
            .expect_err("event history needs event reader");
        assert_eq!(
            missing_backend.code_enum(),
            Some(WireErrorCode::BackendUnavailable)
        );
    }

    #[tokio::test]
    async fn invoke_stream_rejects_precancelled_token() {
        let router = HostRouter::from_backends(HostBackends::default());
        let token = CancellationToken::new();
        token.cancel();
        let ctx = InvokeContext {
            cancel_token: Some(token),
            declared_capabilities: vec![ExtensionCapability::SmallModel],
            ..Default::default()
        };
        let err = router
            .invoke_stream(
                "astrcode.llm.small_chat",
                json!({ "messages": [] }),
                "req-1",
                &ctx,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code_enum(), Some(WireErrorCode::Cancelled));
    }

    #[tokio::test]
    async fn llm_capability_preserves_typed_messages_and_collected_stream_contract() {
        let provider = Arc::new(CapturingLlm::default());
        let router = HostRouter::from_backends(HostBackends {
            main_llm: Some(provider.clone()),
            ..Default::default()
        });
        let messages = vec![
            LlmMessage {
                role: astrcode_core::llm::LlmRole::User,
                content: vec![
                    LlmContent::Text {
                        text: "describe".into(),
                    },
                    LlmContent::Image {
                        base64: "AAE=".into(),
                        media_type: "image/png".into(),
                        filename: Some("input.png".into()),
                    },
                ],
                name: None,
                reasoning_content: None,
            },
            LlmMessage {
                role: astrcode_core::llm::LlmRole::Assistant,
                content: vec![LlmContent::ToolCall {
                    call_id: "call-1".into(),
                    name: "lookup".into(),
                    arguments: json!({ "query": "typed" }),
                    raw_arguments: None,
                }],
                name: None,
                reasoning_content: Some("must inspect".into()),
            },
            LlmMessage::tool("lookup", "call-1", "done", false),
        ];
        let input = serde_json::to_value(HostLlmChatRequest::new(messages.clone()))
            .expect("serialize typed model request");
        let ctx = InvokeContext {
            declared_capabilities: vec![ExtensionCapability::MainModel],
            ..Default::default()
        };

        let output = router
            .invoke("astrcode.llm.main_chat", input.clone(), &ctx)
            .await
            .expect("invoke typed main model");
        let output = serde_json::from_value::<HostLlmChatOutput>(output)
            .expect("deserialize typed model output");
        assert_eq!(output.content, "hello world");
        assert_eq!(output.model, "main_llm");

        let events = router
            .invoke_stream("astrcode.llm.main_chat", input.clone(), "stream-1", &ctx)
            .await
            .expect("collect typed model stream");
        assert_eq!(events.len(), 4);
        let completed = events.last().expect("completed event");
        let WireMessage::Event(completed) = completed else {
            panic!("expected stream event");
        };
        assert_eq!(completed.phase, EventPhase::Completed);
        let collected =
            serde_json::from_value::<HostLlmCollectedStreamOutput>(completed.output.clone())
                .expect("deserialize collected stream output");
        assert_eq!(collected.content, "hello world");
        assert_eq!(collected.chunks[0].delta, "hello ");
        assert_eq!(collected.chunks[1].delta, "world");

        {
            let captured = provider.messages.lock().expect("captured messages");
            assert_eq!(captured.as_slice(), &[messages.clone(), messages]);
        }

        let legacy = router
            .invoke(
                "astrcode.llm.main_chat",
                json!({ "messages": [{ "role": "user", "content": "legacy" }] }),
                &ctx,
            )
            .await
            .expect_err("legacy string-only messages must not be silently coerced");
        assert_eq!(legacy.code_enum(), Some(WireErrorCode::InvalidInput));
    }

    #[tokio::test]
    async fn caller_control_drops_stalled_session_and_llm_futures() {
        let session_dropped = Arc::new(AtomicBool::new(false));
        let session_ops = Arc::new(CapturingSessionOps {
            stalled_state: Some(Arc::clone(&session_dropped)),
            ..Default::default()
        });
        let session_router = HostRouter::from_backends(HostBackends::default());
        let session_ctx = InvokeContext {
            session_id: Some("parent".into()),
            session_ops: Some(session_ops),
            declared_capabilities: vec![ExtensionCapability::SessionControl],
            ..Default::default()
        };
        let session_input = json!({ "target_session_id": "child" });

        let timed = tokio::time::timeout(
            Duration::from_millis(20),
            session_router.invoke(
                "astrcode.session.control.state",
                session_input,
                &session_ctx,
            ),
        )
        .await;

        assert!(
            timed.is_err(),
            "the session operation should remain stalled"
        );
        assert!(
            session_dropped.load(Ordering::SeqCst),
            "the caller timeout must drop the SessionOperations future"
        );

        let llm_started = Arc::new(Notify::new());
        let llm_dropped = Arc::new(AtomicBool::new(false));
        let llm_router = HostRouter::from_backends(HostBackends {
            main_llm: Some(Arc::new(CapturingLlm {
                stalled: Some((Arc::clone(&llm_started), Arc::clone(&llm_dropped))),
                ..Default::default()
            })),
            ..Default::default()
        });
        let cancellation = CancellationToken::new();
        let llm_ctx = InvokeContext {
            cancel_token: Some(cancellation.clone()),
            declared_capabilities: vec![ExtensionCapability::MainModel],
            ..Default::default()
        };
        let llm_input =
            serde_json::to_value(HostLlmChatRequest::new(vec![LlmMessage::user("hello")]))
                .expect("serialize LLM request");
        let invoke = llm_router.invoke("astrcode.llm.main_chat", llm_input, &llm_ctx);
        tokio::pin!(invoke);
        tokio::select! {
            () = llm_started.notified() => {},
            output = &mut invoke => panic!("stalled LLM returned early: {output:?}"),
        }
        cancellation.cancel();

        let error = invoke
            .await
            .expect_err("cancellation should end the invoke");
        assert_eq!(error.code_enum(), Some(WireErrorCode::Cancelled));
        assert!(
            llm_dropped.load(Ordering::SeqCst),
            "cancelling the caller must drop LlmProvider::generate"
        );
    }

    #[derive(Default)]
    struct FakeOutboundNetwork {
        calls: AtomicUsize,
        request: Mutex<Option<astrcode_extension_sdk::host::internal::OutboundNetworkRequest>>,
    }

    #[derive(Default)]
    struct CapturingLlm {
        messages: Mutex<Vec<Vec<LlmMessage>>>,
        stalled: Option<(Arc<Notify>, Arc<AtomicBool>)>,
    }

    #[async_trait::async_trait]
    impl LlmProvider for CapturingLlm {
        async fn generate(
            &self,
            messages: Vec<LlmMessage>,
            _tools: Vec<astrcode_core::tool::ToolDefinition>,
        ) -> Result<tokio::sync::mpsc::UnboundedReceiver<LlmEvent>, astrcode_core::llm::LlmError>
        {
            if let Some((started, dropped)) = &self.stalled {
                let _probe = DropProbe(Arc::clone(dropped));
                started.notify_one();
                std::future::pending::<()>().await;
            }
            self.messages
                .lock()
                .expect("captured messages")
                .push(messages);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            tx.send(LlmEvent::ContentDelta {
                delta: "hello ".into(),
            })
            .expect("send first delta");
            tx.send(LlmEvent::ContentDelta {
                delta: "world".into(),
            })
            .expect("send second delta");
            tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            })
            .expect("send completion");
            Ok(rx)
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 8_192,
                max_output_tokens: 1_024,
            }
        }
    }

    #[async_trait::async_trait]
    impl OutboundNetworkService for FakeOutboundNetwork {
        async fn request(
            &self,
            request: astrcode_extension_sdk::host::internal::OutboundNetworkRequest,
            _cancellation: Option<CancellationToken>,
        ) -> Result<
            astrcode_extension_sdk::host::internal::OutboundNetworkResponse,
            astrcode_extension_sdk::host::internal::OutboundNetworkError,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request.url, "https://example.com/start");
            *self.request.lock().expect("network request") = Some(request);
            Ok(
                astrcode_extension_sdk::host::internal::OutboundNetworkResponse {
                    final_url: "https://example.com/final".into(),
                    status: 200,
                    headers: BTreeMap::new(),
                    body: vec![255, 0, 1],
                },
            )
        }
    }

    #[derive(Default)]
    struct CapturingSessionOps {
        root_creates: Mutex<Vec<CreateRootSessionRequest>>,
        creates: Mutex<Vec<CreateSessionRequest>>,
        submits: Mutex<Vec<SubmitTurnRequest>>,
        tool_configurations: Mutex<Vec<(String, String, SessionToolSelection)>>,
        lifecycle_calls: Mutex<Vec<(String, String, String)>>,
        stalled_state: Option<Arc<AtomicBool>>,
    }

    async fn seed_session(
        store: &InMemoryEventStore,
        session_id: &str,
        parent_session_id: Option<&str>,
        source_extension: Option<&str>,
    ) {
        let session_id = astrcode_core::types::SessionId::new(session_id);
        store
            .create_session(DurableEvent::session(
                session_id,
                DurableEventPayload::SessionStarted(SessionStarted {
                    working_dir: "/workspace".into(),
                    model_id: "test-model".into(),
                    parent: parent_session_id.map(|parent| ParentSessionRef {
                        session_id: astrcode_core::types::SessionId::new(parent),
                    }),
                    tool_selection: SessionToolSelection::default(),
                    source_extension: source_extension.map(str::to_owned),
                    initial_system_prompt: PersistedSystemPrompt {
                        text: "system".into(),
                        fingerprint: "fingerprint".into(),
                        extra_system_prompt: None,
                        source: SystemPromptSource::Native,
                    },
                }),
            ))
            .await
            .expect("seed session");
    }

    #[async_trait::async_trait]
    impl SessionOperations for CapturingSessionOps {
        async fn create_root_session(
            &self,
            request: CreateRootSessionRequest,
        ) -> Result<SessionHandle, SessionApiError> {
            self.root_creates
                .lock()
                .expect("root creates lock")
                .push(request);
            Ok(SessionHandle {
                session_id: "root".into(),
            })
        }

        async fn create_session(
            &self,
            _parent_session_id: &str,
            request: CreateSessionRequest,
        ) -> Result<SessionHandle, SessionApiError> {
            let mut creates = self.creates.lock().expect("creates lock");
            creates.push(request);
            Ok(SessionHandle {
                session_id: format!("child-{}", creates.len()),
            })
        }

        async fn inject_message(
            &self,
            _access: SessionAccess<'_>,
            _content: String,
        ) -> Result<SessionDeliveryOutcome, SessionApiError> {
            Ok(SessionDeliveryOutcome::Injected {
                turn_id: "turn-injected".into(),
            })
        }

        async fn configure_tools(
            &self,
            access: SessionAccess<'_>,
            selection: SessionToolSelection,
        ) -> Result<SessionToolSelection, SessionApiError> {
            self.tool_configurations
                .lock()
                .expect("tool configurations")
                .push((
                    access.caller_session_id.into(),
                    access.target_session_id.into(),
                    selection.clone(),
                ));
            Ok(selection)
        }

        async fn submit_turn(
            &self,
            request: SubmitTurnRequest,
        ) -> Result<SubmitTurnResult, SessionApiError> {
            self.submits.lock().expect("submits lock").push(request);
            Ok(SubmitTurnResult::Backgrounded {
                task_id: "task".into(),
                session_id: "child".into(),
            })
        }

        async fn query_session(
            &self,
            _access: SessionAccess<'_>,
        ) -> Result<SessionStatus, SessionApiError> {
            Ok(SessionStatus {
                alive: true,
                has_active_turn: false,
                last_finish_reason: None,
                message_count: 0,
            })
        }

        async fn session_state(
            &self,
            access: SessionAccess<'_>,
        ) -> Result<SessionState, SessionApiError> {
            if let Some(dropped) = &self.stalled_state {
                let _probe = DropProbe(Arc::clone(dropped));
                std::future::pending::<()>().await;
            }
            self.lifecycle_calls.lock().expect("lifecycle calls").push((
                "state".into(),
                access.caller_session_id.into(),
                access.target_session_id.into(),
            ));
            Ok(SessionState {
                lifecycle: SessionLifecycleState::Recycled,
                phase: astrcode_core::event::Phase::Idle,
                active_turn_id: None,
                queued_inputs: 0,
                message_count: 2,
            })
        }

        async fn recycle_session(&self, _access: SessionAccess<'_>) -> Result<(), SessionApiError> {
            Ok(())
        }

        async fn cancel_turn(&self, _access: SessionAccess<'_>) -> Result<bool, SessionApiError> {
            Ok(true)
        }

        async fn delete_session(&self, _access: SessionAccess<'_>) -> Result<(), SessionApiError> {
            Ok(())
        }

        async fn restore_session(&self, _access: SessionAccess<'_>) -> Result<(), SessionApiError> {
            Ok(())
        }

        async fn reactivate_session(
            &self,
            access: SessionAccess<'_>,
        ) -> Result<SessionReactivation, SessionApiError> {
            self.lifecycle_calls.lock().expect("lifecycle calls").push((
                "reactivate".into(),
                access.caller_session_id.into(),
                access.target_session_id.into(),
            ));
            Ok(SessionReactivation { reactivated: true })
        }

        async fn resolve_tool_approval(
            &self,
            _target_session_id: &str,
            _call_id: &str,
            _decision: ApprovalDecision,
        ) -> Result<(), SessionApiError> {
            Ok(())
        }
    }
}
