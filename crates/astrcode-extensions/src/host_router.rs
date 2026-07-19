//! 宿主能力路由 — 唯一实现 `astrcode.*` RPC 与扩展事件发射。

mod capability;
mod context;
mod extension_http;
mod llm;
mod network;
mod process;
mod session;
mod session_inspect;
mod workspace;

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use astrcode_core::{
    event::EventPayload,
    extension::{
        ExtensionCapability, ExtensionError, ExtensionEventDecl, ExtensionHostServices,
        ExtensionHttpRequest, ExtensionHttpResponse, OutboundNetworkService,
    },
    llm::LlmProvider,
    tool::SessionOperations,
};
use astrcode_extension_sdk::s5r::{
    CapabilityDescriptor, ErrorPayload, EventMsg, EventPhase, WireMessage,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use self::{
    capability::HostCapability, context::ContextGroup, extension_http::ExtensionHttpGroup,
    llm::LlmGroup, network::NetworkGroup, process::ProcessGroup, session::SessionGroup,
    workspace::WorkspaceGroup,
};

pub(super) const HOST_INVOKE_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn block_on_async<F>(future: F) -> Result<F::Output, ErrorPayload>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    static RUNTIME: std::sync::OnceLock<Result<tokio::runtime::Runtime, String>> =
        std::sync::OnceLock::new();
    let rt = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())
    });
    let rt = rt.as_ref().map_err(|message| {
        ErrorPayload::new(
            "host_runtime_unavailable",
            format!("failed to initialize host runtime: {message}"),
        )
    })?;

    // 从 tokio 异步任务里直接 block_on 会占满 test/runtime worker，嵌套 host invoke 会死锁。
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        if matches!(
            handle.runtime_flavor(),
            tokio::runtime::RuntimeFlavor::MultiThread
        ) {
            return tokio::task::block_in_place(|| run_on_host_runtime(rt, future));
        }
        match std::thread::spawn(move || run_on_host_runtime(rt, future)).join() {
            Ok(output) => return output,
            Err(_) => {
                return Err(ErrorPayload::new(
                    "host_runtime_failed",
                    "host runtime thread panicked",
                ));
            },
        }
    }
    run_on_host_runtime(rt, future)
}

fn run_on_host_runtime<F>(
    runtime: &tokio::runtime::Runtime,
    future: F,
) -> Result<F::Output, ErrorPayload>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    runtime.block_on(async move {
        tokio::spawn(future).await.map_err(|error| {
            ErrorPayload::new(
                "host_runtime_failed",
                format!("host runtime task failed: {error}"),
            )
        })
    })
}

pub(super) fn run_blocking_io<T>(operation: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle)
            if matches!(
                handle.runtime_flavor(),
                tokio::runtime::RuntimeFlavor::MultiThread
            ) =>
        {
            tokio::task::block_in_place(operation)
        },
        _ => operation(),
    }
}

/// 单次 guest→host invoke 的运行时上下文。
#[derive(Clone, Default)]
pub struct InvokeContext {
    pub extension_id: String,
    pub session_id: Option<String>,
    pub session_store_dir: Option<PathBuf>,
    pub session_ops: Option<Arc<dyn SessionOperations>>,
    pub event_tx: Option<mpsc::UnboundedSender<EventPayload>>,
    pub working_dir: Option<String>,
    pub cancel_token: Option<CancellationToken>,
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
    pub session_read: Option<Arc<dyn astrcode_core::storage::EventReader>>,
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
    pub fn new(host_services: &ExtensionHostServices, default_working_dir: Option<String>) -> Self {
        Self::from_backends(HostBackends {
            main_llm: host_services.main_llm.clone(),
            small_llm: host_services.small_llm.clone(),
            session_read: host_services.session_read.clone(),
            default_working_dir,
            public_http_dispatcher: None,
            outbound_network: host_services.outbound_network.clone(),
        })
    }

    pub fn from_backends(backends: HostBackends) -> Self {
        let HostBackends {
            main_llm,
            small_llm,
            session_read,
            default_working_dir,
            public_http_dispatcher,
            outbound_network,
        } = backends;
        Self {
            llm: LlmGroup::new(main_llm, small_llm),
            session: SessionGroup::new(session_read),
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

    pub fn authorize_astrcode(
        cap: &str,
        declared: &[ExtensionCapability],
    ) -> Result<(), ErrorPayload> {
        capability::authorize(capability::lookup(cap)?, declared)
    }

    /// 同步 invoke（IPC guest 线程调用）。流式能力在内部收集后一次性返回。
    pub fn invoke_sync(
        &self,
        cap: &str,
        input: &str,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        if let Some(token) = &ctx.cancel_token {
            if token.is_cancelled() {
                return Err(ErrorPayload::new("cancelled", "invoke cancelled"));
            }
        }
        let spec = capability::lookup(cap)?;
        capability::authorize(spec, &ctx.declared_capabilities)?;

        let input: Value = serde_json::from_str(input)
            .map_err(|error| ErrorPayload::new("invalid_input", error.to_string()))?;

        match spec.capability {
            HostCapability::Llm(capability) => {
                self.llm
                    .invoke(capability, &input, ctx.cancel_token.as_ref())
            },
            HostCapability::Session(capability) => self.session.invoke(capability, input, ctx),
            HostCapability::Context(capability) => self.context.invoke(capability, &input, ctx),
            HostCapability::Workspace(capability) => {
                self.workspace
                    .invoke(capability, &input, ctx.working_dir.as_deref())
            },
            HostCapability::Process(capability) => self.process.invoke(
                capability,
                input,
                ctx.working_dir.as_deref(),
                ctx.cancel_token.as_ref(),
            ),
            HostCapability::Network(capability) => {
                self.network
                    .invoke(capability, input, ctx.cancel_token.as_ref())
            },
            HostCapability::ExtensionHttp(capability) => {
                self.extension_http
                    .invoke(capability, input, &ctx.extension_id)
            },
        }
    }

    /// 流式 invoke：返回 Event 序列（`started` + `delta*` + `completed`/`failed`）。
    pub fn invoke_stream_sync(
        &self,
        cap: &str,
        input: &str,
        request_id: &str,
        ctx: &InvokeContext,
    ) -> Result<Vec<WireMessage>, ErrorPayload> {
        if let Some(token) = &ctx.cancel_token {
            if token.is_cancelled() {
                return Err(ErrorPayload::new("cancelled", "invoke cancelled"));
            }
        }
        let spec = capability::lookup(cap)?;
        capability::authorize(spec, &ctx.declared_capabilities)?;
        if !spec.supports_stream {
            return Err(ErrorPayload::new(
                "stream_not_supported",
                format!("stream not supported for {cap}"),
            ));
        }
        let input: Value = serde_json::from_str(input)
            .map_err(|error| ErrorPayload::new("invalid_input", error.to_string()))?;
        let request_id = request_id.to_string();

        match spec.capability {
            HostCapability::Llm(capability) => {
                let invoke = self
                    .llm
                    .invoke_stream(capability, &input, ctx.cancel_token.as_ref());
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
                "invalid_capability_registry",
                format!("streaming capability {cap} has no stream handler"),
            )),
        }
    }
}


/// 供 runner 内 `ExtensionEventSink` 与 IPC 路径复用。
pub fn emit_for_sink(
    extension_id: &str,
    declarations: &HashMap<String, ExtensionEventDecl>,
    event_tx: &mpsc::UnboundedSender<EventPayload>,
    event_type: &str,
    schema_version: u32,
    payload: Value,
) -> Result<(), ExtensionError> {
    validate_emit(declarations, event_type, schema_version, &payload)?;
    event_tx
        .send(EventPayload::ExtensionEvent {
            extension_id: extension_id.to_owned(),
            event_type: event_type.to_owned(),
            schema_version,
            payload,
        })
        .map_err(|_| ExtensionError::Internal("event channel closed".into()))
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
    if schema_version > decl.schema_version {
        return Err(ExtensionError::Internal(format!(
            "schema_version {schema_version} exceeds declared {} for {event_type}",
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

/// 从 [`ExtensionHostServices`] 构造共享 [`HostRouter`]。
pub fn build_host_router(
    host_services: Arc<ExtensionHostServices>,
    default_working_dir: Option<String>,
) -> Arc<HostRouter> {
    Arc::new(HostRouter::new(&host_services, default_working_dir))
}

/// 构造 trusted bundled extensions 与 worker 共用的受限出站网络服务。
pub fn default_outbound_network_service() -> Arc<dyn OutboundNetworkService> {
    Arc::new(network::RestrictedNetworkService::default())
}

pub fn build_host_router_with_public_http_dispatcher(
    host_services: Arc<ExtensionHostServices>,
    default_working_dir: Option<String>,
    dispatcher: Arc<dyn PublicHttpDispatcher>,
) -> Arc<HostRouter> {
    Arc::new(
        HostRouter::new(&host_services, default_working_dir)
            .with_public_http_dispatcher(dispatcher),
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use astrcode_core::{
        extension::ChildToolPolicy,
        permission::ApprovalDecision,
        storage::{EventReader, EventStore},
        tool::{
            CreateRootSessionRequest, CreateSessionRequest, SessionAccess, SessionApiError,
            SessionDeliveryOutcome, SessionHandle, SessionStatus, SubmitTurnRequest,
            SubmitTurnResult,
        },
    };
    use astrcode_storage::in_memory::InMemoryEventStore;
    use serde_json::json;

    use super::*;

    #[test]
    fn host_runtime_contains_extension_task_panics() {
        let result = block_on_async(async {
            panic!("extension task panic");
            #[allow(unreachable_code)]
            42
        });

        let error = result.expect_err("task panic should become a host error");
        assert_eq!(error.code, "host_runtime_failed");
    }

    #[test]
    fn catalog_includes_session_control_subcaps() {
        let caps = HostRouter::catalog_for_grants(&[ExtensionCapability::SessionControl]);
        let names: Vec<_> = caps.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"astrcode.session.control.create"));
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

    #[tokio::test(flavor = "multi_thread")]
    async fn session_inspect_maps_storage_model_to_wire_contract() {
        let store = Arc::new(InMemoryEventStore::new());
        let session_id = astrcode_core::types::SessionId::new("inspect-session");
        store
            .create_session(&session_id, "/workspace", "test-model", None, None, None)
            .await
            .expect("create session");
        let reader: Arc<dyn EventReader> = store;
        let router = HostRouter::from_backends(HostBackends {
            session_read: Some(reader),
            ..Default::default()
        });
        let ctx = InvokeContext {
            declared_capabilities: vec![ExtensionCapability::SessionInspect],
            ..Default::default()
        };

        let list = router
            .invoke_sync("astrcode.session.inspect.list", "{}", &ctx)
            .expect("list sessions");
        assert_eq!(list["sessions"][0]["sessionId"], "inspect-session");

        let model = router
            .invoke_sync(
                "astrcode.session.inspect.read_model",
                &json!({ "session_id": "inspect-session" }).to_string(),
                &ctx,
            )
            .expect("read session model");
        assert_eq!(model["readModel"]["modelId"], "test-model");
        assert_eq!(model["readModel"]["phase"], "idle");
    }

    #[test]
    fn session_control_create_schema_includes_tool_policy() {
        let caps = HostRouter::catalog_for_grants(&[ExtensionCapability::SessionControl]);
        let create = caps
            .iter()
            .find(|cap| cap.name == "astrcode.session.control.create")
            .expect("create capability");

        assert!(create.input_schema["properties"]["tool_policy"].is_object());
    }

    #[test]
    fn catalog_includes_bounded_io_capabilities() {
        let caps = HostRouter::catalog_for_grants(&[
            ExtensionCapability::WorkspaceRead,
            ExtensionCapability::WorkspaceWrite,
            ExtensionCapability::NetworkClient,
            ExtensionCapability::ProcessSpawn,
        ]);
        let names: Vec<_> = caps.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"astrcode.workspace.read"));
        assert!(names.contains(&"astrcode.workspace.list"));
        assert!(names.contains(&"astrcode.workspace.grep"));
        assert!(names.contains(&"astrcode.workspace.glob"));
        assert!(names.contains(&"astrcode.workspace.write"));
        assert!(names.contains(&"astrcode.workspace.edit"));
        assert!(names.contains(&"astrcode.network.client"));
        assert!(names.contains(&"astrcode.process.spawn"));
    }

    #[test]
    fn network_capability_rejects_non_http_urls_when_declared() {
        let router = HostRouter::from_backends(HostBackends {
            outbound_network: Some(default_outbound_network_service()),
            ..Default::default()
        });
        let ctx = InvokeContext {
            declared_capabilities: vec![ExtensionCapability::NetworkClient],
            ..Default::default()
        };
        let err = router
            .invoke_sync(
                "astrcode.network.client",
                &json!({ "url": "file:///etc/passwd" }).to_string(),
                &ctx,
            )
            .unwrap_err();
        assert_eq!(err.code, "permission_denied");
    }

    #[test]
    fn network_registry_routes_authorizes_and_preserves_final_url() {
        let network = Arc::new(FakeOutboundNetwork::default());
        let router = HostRouter::from_backends(HostBackends {
            outbound_network: Some(network.clone()),
            ..Default::default()
        });
        let request = json!({ "url": "https://example.com/start" }).to_string();
        let allowed = InvokeContext {
            declared_capabilities: vec![ExtensionCapability::NetworkClient],
            ..Default::default()
        };

        let response = router
            .invoke_sync("astrcode.network.client", &request, &allowed)
            .expect("declared network capability");
        assert_eq!(response["final_url"], "https://example.com/final");
        assert_eq!(response["body"], "ok");
        assert_eq!(network.calls.load(Ordering::SeqCst), 1);

        let denied = router
            .invoke_sync(
                "astrcode.network.client",
                &request,
                &InvokeContext::default(),
            )
            .expect_err("missing network grant");
        assert_eq!(denied.code, "permission_denied");
        assert_eq!(network.calls.load(Ordering::SeqCst), 1);

        let unknown = router
            .invoke_sync("astrcode.network.unknown", "{}", &allowed)
            .expect_err("unknown capability");
        assert_eq!(unknown.code, "unknown_capability");
    }

    #[test]
    fn session_state_api_does_not_require_declared_capability() {
        let router = HostRouter::from_backends(HostBackends::default());
        let temp = tempfile::tempdir().expect("tempdir");
        let ctx = InvokeContext {
            extension_id: "stateful-test".into(),
            session_store_dir: Some(temp.path().to_path_buf()),
            declared_capabilities: Vec::new(),
            ..Default::default()
        };

        router
            .invoke_sync(
                "astrcode.session.state.write",
                &json!({ "key": "goal", "content": "active" }).to_string(),
                &ctx,
            )
            .expect("write state without capability");
        let read = router
            .invoke_sync(
                "astrcode.session.state.read",
                &json!({ "key": "goal" }).to_string(),
                &ctx,
            )
            .expect("read state without capability");

        assert_eq!(read["content"], "active");
    }

    #[test]
    fn invoke_sync_rejects_precancelled_token() {
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
            .invoke_sync(
                "astrcode.workspace.read",
                &json!({ "path": "x" }).to_string(),
                &ctx,
            )
            .unwrap_err();
        assert_eq!(err.code, "cancelled");
    }

    #[test]
    fn invoke_session_submit_rejects_wait_for_result_on_peer_io_thread() {
        let router = HostRouter::from_backends(HostBackends::default());
        let ctx = InvokeContext {
            declared_capabilities: vec![ExtensionCapability::SessionControl],
            session_id: Some("parent".into()),
            on_peer_io_thread: true,
            ..Default::default()
        };
        let err = router
            .invoke_sync(
                "astrcode.session.control.submit_turn",
                &json!({
                    "target_session_id": "child",
                    "user_prompt": "hello",
                    "wait_for_result": true
                })
                .to_string(),
                &ctx,
            )
            .unwrap_err();
        assert_eq!(err.code, "invalid_request");
    }

    #[test]
    fn invoke_session_create_forwards_tool_policy() {
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
            .invoke_sync(
                "astrcode.session.control.create",
                &json!({
                    "name": "worker",
                    "tool_policy": {
                        "mode": "deny",
                        "tools": ["agent"]
                    }
                })
                .to_string(),
                &ctx,
            )
            .expect("create child session");

        assert_eq!(output["session_id"], "child-1");
        let requests = ops.creates.lock().expect("creates lock");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].tool_policy,
            Some(ChildToolPolicy::Deny {
                tools: vec!["agent".into()]
            })
        );
    }

    #[test]
    fn invoke_session_create_rejects_invalid_tool_policy() {
        let router = HostRouter::from_backends(HostBackends::default());
        let ctx = InvokeContext {
            session_id: Some("parent".into()),
            session_ops: Some(Arc::new(CapturingSessionOps::default())),
            declared_capabilities: vec![ExtensionCapability::SessionControl],
            ..Default::default()
        };

        let err = router
            .invoke_sync(
                "astrcode.session.control.create",
                &json!({
                    "tool_policy": {
                        "mode": "allow",
                        "tools": []
                    }
                })
                .to_string(),
                &ctx,
            )
            .unwrap_err();

        assert_eq!(err.code, "invalid_input");
    }

    #[test]
    fn invoke_session_inject_returns_delivery_outcome() {
        let router = HostRouter::from_backends(HostBackends::default());
        let ctx = InvokeContext {
            session_id: Some("parent".into()),
            session_ops: Some(Arc::new(CapturingSessionOps::default())),
            declared_capabilities: vec![ExtensionCapability::SessionControl],
            ..Default::default()
        };

        let output = router
            .invoke_sync(
                "astrcode.session.control.inject_or_start",
                &json!({
                    "target_session_id": "child",
                    "content": "continue"
                })
                .to_string(),
                &ctx,
            )
            .expect("inject session input");

        assert_eq!(output["status"], "injected");
        assert_eq!(output["turn_id"], "turn-injected");
    }

    #[test]
    fn invoke_stream_sync_rejects_precancelled_token() {
        let router = HostRouter::from_backends(HostBackends::default());
        let token = CancellationToken::new();
        token.cancel();
        let ctx = InvokeContext {
            cancel_token: Some(token),
            declared_capabilities: vec![ExtensionCapability::SmallModel],
            ..Default::default()
        };
        let err = router
            .invoke_stream_sync(
                "astrcode.llm.small_chat",
                &json!({ "messages": [] }).to_string(),
                "req-1",
                &ctx,
            )
            .unwrap_err();
        assert_eq!(err.code, "cancelled");
    }

    #[derive(Default)]
    struct FakeOutboundNetwork {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl OutboundNetworkService for FakeOutboundNetwork {
        async fn request(
            &self,
            request: astrcode_core::extension::OutboundNetworkRequest,
            _cancellation: Option<CancellationToken>,
        ) -> Result<
            astrcode_core::extension::OutboundNetworkResponse,
            astrcode_core::extension::OutboundNetworkError,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request.url, "https://example.com/start");
            Ok(astrcode_core::extension::OutboundNetworkResponse {
                final_url: "https://example.com/final".into(),
                status: 200,
                headers: BTreeMap::new(),
                body: b"ok".to_vec(),
            })
        }
    }

    #[derive(Default)]
    struct CapturingSessionOps {
        creates: Mutex<Vec<CreateSessionRequest>>,
    }

    #[async_trait::async_trait]
    impl SessionOperations for CapturingSessionOps {
        async fn create_root_session(
            &self,
            _request: CreateRootSessionRequest,
        ) -> Result<SessionHandle, SessionApiError> {
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

        async fn submit_turn(
            &self,
            _request: SubmitTurnRequest,
        ) -> Result<SubmitTurnResult, SessionApiError> {
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

        async fn recycle_session(&self, _access: SessionAccess<'_>) -> Result<(), SessionApiError> {
            Ok(())
        }

        async fn delete_session(&self, _access: SessionAccess<'_>) -> Result<(), SessionApiError> {
            Ok(())
        }

        async fn restore_session(&self, _access: SessionAccess<'_>) -> Result<(), SessionApiError> {
            Ok(())
        }

        async fn resolve_tool_approval(
            &self,
            _target_session_id: &str,
            _call_id: &str,
            _decision: ApprovalDecision,
        ) -> Result<(), SessionApiError> {
            Ok(())
        }

        async fn resolve_tool_ui_response(
            &self,
            _target_session_id: &str,
            _call_id: &str,
            _answers: std::collections::BTreeMap<String, String>,
        ) -> Result<(), SessionApiError> {
            Ok(())
        }
    }
}
