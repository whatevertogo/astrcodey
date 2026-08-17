//! Worker 侧调用宿主的抽象（可注入 mock）。

use std::{future::Future, sync::Arc};

use async_trait::async_trait;
use serde_json::Value;

use crate::{
    WireErrorCode,
    host::internal::{
        HostClientTransport, TypedEventClient, TypedExtensionHttpClient, TypedModelClient,
        TypedNetworkClient, TypedProcessClient, TypedSessionControlClient,
        TypedSessionHistoryClient, TypedSessionInspectClient, TypedSessionStateClient,
        TypedToolResultClient, TypedWorkspaceClient,
    },
    model_stream::ModelStream,
    s5r::ErrorPayload,
};
#[cfg(test)]
use crate::{extension::ExtensionHttpDispatchRequest, llm::LlmMessage};
pub use crate::{
    host::{
        HostConfigureSessionToolsOutput, HostConfigureSessionToolsRequest, HostEventEmitOutput,
        HostEventEmitRequest, HostLlmChatOutput, HostLlmChatRequest, HostLlmContent,
        HostLlmMessage, HostLlmRole, HostNetworkRedirectPolicy, HostNetworkRequest,
        HostNetworkResponse, HostOperation, HostProcessHandleOutput, HostProcessInputAction,
        HostProcessInputRequest, HostProcessListOutput, HostProcessOutput, HostProcessReadOutput,
        HostProcessReadRequest, HostProcessRequest, HostProcessStartRequest, HostProcessState,
        HostProcessStatusOutput, HostProcessTargetRequest, HostSessionCancelOutput,
        HostSessionDeliveryOutput, HostSessionExecutionView, HostSessionInputRequest,
        HostSessionProviderMessagesOutput, HostSessionStateReadOutput, HostSessionStateReadRequest,
        HostSessionStateWriteRequest, HostSessionSummariesOutput, HostSessionSummary,
        HostSessionTokenUsage, HostSessionTokenUsageOutput, HostSessionTranscript,
        HostSessionTranscriptMessage, HostToolResultReadOutput, HostToolResultReadRequest,
        HostWorkspaceApplyPatchOutput, HostWorkspaceApplyPatchRequest, HostWorkspaceEditOutput,
        HostWorkspaceEditRequest, HostWorkspaceGlobOutput, HostWorkspaceGlobRequest,
        HostWorkspaceGrepContextLine, HostWorkspaceGrepEntry, HostWorkspaceGrepMode,
        HostWorkspaceGrepOutput, HostWorkspaceGrepRequest, HostWorkspaceListEntry,
        HostWorkspaceListOutput, HostWorkspaceListRequest, HostWorkspaceReadOutput,
        HostWorkspaceReadRequest, HostWorkspaceTextChange, HostWorkspaceWriteOutput,
        HostWorkspaceWriteRequest, llm_chat_request,
    },
    session::{
        HostCreateRootSessionRequest, HostCreateSessionOutput, HostCreateSessionRequest,
        HostRecycleSessionRequest, HostRootSubmitTurnRequest, HostSessionEvent,
        HostSessionEventsPageOutput, HostSessionEventsPageRequest, HostSessionReactivateOutput,
        HostSessionStateOutput, HostSessionTargetRequest, HostSubmitTurnOutput,
        HostSubmitTurnRequest,
    },
};

/// 扩展子进程调用 `astrcode.*` 能力的接口。
#[async_trait]
pub trait HostApi: Send + Sync {
    fn host_supports(&self, operation: HostOperation) -> bool;

    async fn call(&self, capability: &str, input: Value) -> Result<Value, ErrorPayload>;

    async fn open_stream(
        &self,
        capability: &str,
        input: Value,
    ) -> Result<ModelStream, ErrorPayload> {
        let _ = (capability, input);
        Err(ErrorPayload::new(
            WireErrorCode::StreamNotSupported,
            "host transport does not expose incremental streaming",
        ))
    }
}

pub(crate) struct V3PeerHostApi {
    peer: astrcode_extension_sdk::wire::PeerHandle,
}

impl V3PeerHostApi {
    pub fn new(peer: astrcode_extension_sdk::wire::PeerHandle) -> Self {
        Self { peer }
    }
}

#[async_trait]
impl HostApi for V3PeerHostApi {
    fn host_supports(&self, operation: HostOperation) -> bool {
        self.peer.host_supports(operation.wire_name())
    }

    async fn call(&self, capability: &str, input: Value) -> Result<Value, ErrorPayload> {
        self.peer
            .invoke(capability, input)
            .await
            .map_err(ErrorPayload::from)
    }

    async fn open_stream(
        &self,
        capability: &str,
        input: Value,
    ) -> Result<ModelStream, ErrorPayload> {
        let stream = self
            .peer
            .invoke_stream(capability, input)
            .await
            .map_err(ErrorPayload::from)?;
        Ok(ModelStream::from_stream(
            stream,
            tokio_util::sync::CancellationToken::new(),
        ))
    }
}

tokio::task_local! {
    static SCOPED_HOST_API: Arc<dyn HostApi>;
}

/// 在一个入站调用作用域内绑定宿主 API。
///
/// 该作用域不会传播到 `tokio::spawn` 创建的新任务；脱离当前调用后继续访问宿主必须失败，
/// 否则新任务会绕过 planning 与本次调用的 resource lease。
pub async fn with_host_api<T>(api: Arc<dyn HostApi>, future: impl Future<Output = T>) -> T {
    SCOPED_HOST_API.scope(api, future).await
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub struct WorkerHostTransport;

#[async_trait]
impl HostClientTransport for WorkerHostTransport {
    type Error = ErrorPayload;

    async fn invoke(&self, operation: HostOperation, input: Value) -> Result<Value, Self::Error> {
        let api = supported_host_api(operation)?;
        api.call(operation.wire_name(), input).await
    }

    async fn invoke_stream(
        &self,
        operation: HostOperation,
        input: Value,
    ) -> Result<ModelStream, Self::Error> {
        let api = supported_host_api(operation)?;
        api.open_stream(operation.wire_name(), input).await
    }

    fn client_error(code: WireErrorCode, message: String) -> Self::Error {
        ErrorPayload::new(code, message)
    }

    fn payload_error(error: ErrorPayload) -> Self::Error {
        error
    }
}

/// 持有 `Arc<dyn HostApi>` 的传输实现,与 task-local 的 `WorkerHostTransport` 平行。
///
/// 直接调用注入的 API,不做 task-local 解析与 host_supports 预检;支持性查询走
/// [`BackgroundHost::host_supports`],不支持的操作由宿主侧返回 `UnknownCapability`。
#[derive(Clone)]
pub(crate) struct BoundHostTransport {
    api: Arc<dyn HostApi>,
}

#[async_trait]
impl HostClientTransport for BoundHostTransport {
    type Error = ErrorPayload;

    async fn invoke(&self, operation: HostOperation, input: Value) -> Result<Value, Self::Error> {
        self.api.call(operation.wire_name(), input).await
    }

    async fn invoke_stream(
        &self,
        operation: HostOperation,
        input: Value,
    ) -> Result<ModelStream, Self::Error> {
        self.api.open_stream(operation.wire_name(), input).await
    }

    fn client_error(code: WireErrorCode, message: String) -> Self::Error {
        ErrorPayload::new(code, message)
    }

    fn payload_error(error: ErrorPayload) -> Self::Error {
        error
    }
}

/// 可逃逸出 handler 的宿主句柄:无 turn 作用域,仅暴露 root session 域。
///
/// 由根部 `PeerHandle` 构造(`parent_invoke_id` 为 `None`),宿主侧按 detached context
/// 处理,Session/Workspace 上下文要求的操作会失败关闭。经
/// [`Worker::background_host`](crate::Worker::background_host) 注册的通道在 transport
/// handshake 完成后交付。
#[derive(Clone)]
pub struct BackgroundHost {
    transport: BoundHostTransport,
}

impl BackgroundHost {
    pub(crate) fn new(api: Arc<dyn HostApi>) -> Self {
        Self {
            transport: BoundHostTransport { api },
        }
    }

    pub fn host_supports(&self, operation: HostOperation) -> bool {
        self.transport.api.host_supports(operation)
    }

    /// 窄 session 面:只含无 session 上下文的 root 域方法。
    pub fn root_sessions(&self) -> BackgroundRootSessionClient {
        BackgroundRootSessionClient {
            inner: TypedSessionControlClient::new(self.transport.clone()),
        }
    }
}

/// [`BackgroundHost`] 的 root session 域客户端。
#[derive(Clone)]
pub struct BackgroundRootSessionClient {
    inner: TypedSessionControlClient<BoundHostTransport>,
}

impl BackgroundRootSessionClient {
    pub async fn create_root(
        &self,
        request: HostCreateRootSessionRequest,
    ) -> Result<HostCreateSessionOutput, ErrorPayload> {
        self.inner.create_root(request).await
    }

    pub async fn submit_root_turn(
        &self,
        request: HostRootSubmitTurnRequest,
    ) -> Result<HostSubmitTurnOutput, ErrorPayload> {
        self.inner.submit_root_turn(request).await
    }

    pub async fn root_state(
        &self,
        target: HostSessionTargetRequest,
    ) -> Result<HostSessionStateOutput, ErrorPayload> {
        self.inner.root_state(target).await
    }

    pub async fn dispose_root(&self, target: HostSessionTargetRequest) -> Result<(), ErrorPayload> {
        self.inner.dispose_root(target).await
    }
}

pub type ModelClient = TypedModelClient<WorkerHostTransport>;
pub type EventClient = TypedEventClient<WorkerHostTransport>;
pub type SessionControlClient = TypedSessionControlClient<WorkerHostTransport>;
pub type SessionHistoryClient = TypedSessionHistoryClient<WorkerHostTransport>;
pub type SessionStateClient = TypedSessionStateClient<WorkerHostTransport>;
pub type SessionInspectClient = TypedSessionInspectClient<WorkerHostTransport>;
pub type ToolResultClient = TypedToolResultClient<WorkerHostTransport>;
pub type WorkspaceClient = TypedWorkspaceClient<WorkerHostTransport>;
pub type ProcessClient = TypedProcessClient<WorkerHostTransport>;
pub type NetworkClient = TypedNetworkClient<WorkerHostTransport>;
pub type ExtensionHttpClient = TypedExtensionHttpClient<WorkerHostTransport>;

/// Worker-side entry point for typed host domains.
pub struct HostClient;

impl HostClient {
    pub fn host_supports(operation: HostOperation) -> Result<bool, ErrorPayload> {
        Ok(host_api()?.host_supports(operation))
    }

    pub const fn events() -> EventClient {
        EventClient::new(WorkerHostTransport)
    }

    pub const fn models() -> ModelClient {
        ModelClient::new(WorkerHostTransport)
    }

    pub const fn session_control() -> SessionControlClient {
        SessionControlClient::new(WorkerHostTransport)
    }

    pub const fn session_history() -> SessionHistoryClient {
        SessionHistoryClient::new(WorkerHostTransport)
    }

    pub const fn session_state() -> SessionStateClient {
        SessionStateClient::new(WorkerHostTransport)
    }

    pub const fn session_inspect() -> SessionInspectClient {
        SessionInspectClient::new(WorkerHostTransport)
    }

    pub const fn workspace() -> WorkspaceClient {
        WorkspaceClient::new(WorkerHostTransport)
    }

    pub const fn tool_results() -> ToolResultClient {
        ToolResultClient::new(WorkerHostTransport)
    }

    pub const fn process() -> ProcessClient {
        ProcessClient::new(WorkerHostTransport)
    }

    pub const fn network() -> NetworkClient {
        NetworkClient::new(WorkerHostTransport)
    }

    pub const fn extension_http() -> ExtensionHttpClient {
        ExtensionHttpClient::new(WorkerHostTransport)
    }
}

fn host_api() -> Result<Arc<dyn HostApi>, ErrorPayload> {
    SCOPED_HOST_API.try_with(Arc::clone).map_err(|_| {
        ErrorPayload::new(
            WireErrorCode::ContextUnavailable,
            "host API is only available while handling an invocation",
        )
    })
}

fn supported_host_api(operation: HostOperation) -> Result<Arc<dyn HostApi>, ErrorPayload> {
    let api = host_api()?;
    if api.host_supports(operation) {
        Ok(api)
    } else {
        Err(ErrorPayload::new(
            WireErrorCode::Unsupported,
            format!("host does not support operation {}", operation.wire_name()),
        ))
    }
}

/// Invokes a raw host capability for transport or request-context integration tests.
#[cfg(any(test, feature = "testing"))]
pub async fn invoke_host(capability: &str, input: Value) -> Result<Value, ErrorPayload> {
    host_api()?.call(capability, input).await
}
#[cfg(test)]
mod host_tests {
    use std::{
        collections::HashSet,
        future::Future,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use async_trait::async_trait;
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{s5r::ErrorPayload, session::SessionToolSelectionDto};

    struct MockHost {
        marker: &'static str,
    }

    #[derive(Default)]
    struct RecordingHost {
        operations: Mutex<Vec<HostOperation>>,
    }

    #[derive(Default)]
    struct LimitedHost {
        calls: AtomicUsize,
    }

    async fn expect_backend_error<T>(future: impl Future<Output = Result<T, ErrorPayload>>) {
        let Err(error) = future.await else {
            panic!("mock host unexpectedly succeeded");
        };
        assert_eq!(error.code_enum(), Some(WireErrorCode::BackendUnavailable));
    }

    #[test]
    fn bounded_io_contracts_match_wire_shape() {
        let mut process = HostProcessRequest::new("rustc");
        process.args.push("--version".into());
        process.timeout_ms = Some(1_000);
        let value = serde_json::to_value(&process).expect("serialize process request");
        assert_eq!(value["command"], "rustc");
        assert_eq!(value["args"], json!(["--version"]));
        assert_eq!(value["timeout_ms"], 1_000);

        let mut network = HostNetworkRequest::get("https://example.com");
        network.body = vec![0, 255];
        network.redirect_policy = HostNetworkRedirectPolicy::Manual;
        let value = serde_json::to_value(&network).expect("serialize binary network request");
        assert_eq!(value["body"], "AP8=");
        assert_eq!(value["redirect_policy"], "manual");
        assert_eq!(value["max_bytes"], 10 * 1024 * 1024);

        let response: HostNetworkResponse = serde_json::from_value(json!({
            "final_url": "https://example.com/final",
            "status": 200,
            "headers": {},
            "body": "b2s="
        }))
        .expect("deserialize network response");
        assert_eq!(response.final_url, "https://example.com/final");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"ok");
    }

    #[test]
    fn session_control_contracts_match_wire_shape_and_safe_worker_defaults() {
        let mut create = HostCreateSessionRequest::new("reviewer");
        create.tool_selection = Some(SessionToolSelectionDto::only(["read", "grep"]));
        create.ephemeral = true;
        let value = serde_json::to_value(&create).expect("serialize create session request");
        assert_eq!(value["name"], "reviewer");
        assert_eq!(value["tool_selection"]["mode"], "only");
        assert_eq!(value["tool_selection"]["names"], json!(["read", "grep"]));
        assert_eq!(value["ephemeral"], true);

        let submit = HostSubmitTurnRequest::background("child-1", "review this");
        let value = serde_json::to_value(&submit).expect("serialize submit turn request");
        assert_eq!(value["target_session_id"], "child-1");
        assert_eq!(value["wait_for_result"], false);
        assert_eq!(value["recycle_on_complete"], true);

        let synchronous_default: HostSubmitTurnRequest = serde_json::from_value(json!({
            "target_session_id": "child-1",
            "user_prompt": "review this"
        }))
        .expect("deserialize submit turn defaults");
        assert!(synchronous_default.wait_for_result);
        assert!(!synchronous_default.recycle_on_complete);
        assert!(
            serde_json::from_value::<HostCreateSessionRequest>(json!({
                "name": "reviewer",
                "unknown": true
            }))
            .is_err()
        );

        let output: HostSubmitTurnOutput = serde_json::from_value(json!({
            "status": "backgrounded",
            "task_id": "turn-1",
            "session_id": "child-1"
        }))
        .expect("deserialize submit turn response");
        assert_eq!(
            output,
            HostSubmitTurnOutput::Backgrounded {
                task_id: "turn-1".into(),
                session_id: "child-1".into()
            }
        );
    }

    #[async_trait]
    impl HostApi for MockHost {
        fn host_supports(&self, _operation: HostOperation) -> bool {
            true
        }

        async fn call(&self, capability: &str, input: Value) -> Result<Value, ErrorPayload> {
            match capability {
                "astrcode.llm.main_chat" => {
                    assert_eq!(input["messages"][0]["role"], "user");
                    Ok(json!({ "content": "typed", "model": "main_llm" }))
                },
                "astrcode.network.client" => Ok(json!({
                    "final_url": "https://example.com/final",
                    "status": 200,
                    "headers": {},
                    "body": "AP8="
                })),
                "astrcode.session.history.list" => Ok(json!({
                    "sessions": [{
                        "session_id": "session-1",
                        "parent_session_id": null,
                        "source_extension": null,
                        "working_dir": "/workspace",
                        "model_id": "model",
                        "created_at": "2026-01-01T00:00:00Z",
                        "updated_at": "2026-01-01T00:00:00Z",
                        "latest_cursor": "1"
                    }]
                })),
                "astrcode.session.control.create" => Ok(json!({ "session_id": "child-1" })),
                "astrcode.session.root.create" => {
                    assert_eq!(input, json!({}));
                    Ok(json!({ "session_id": "root-1" }))
                },
                "astrcode.session.control.submit_turn" => Ok(json!({
                    "status": "backgrounded",
                    "task_id": "turn-1",
                    "session_id": "child-1"
                })),
                "astrcode.session.state.read" => Ok(json!({ "content": "active" })),
                capability
                    if matches!(
                        capability,
                        "astrcode.session.control.dispose" | "astrcode.session.state.write"
                    ) && self.marker == "invalid_ack" =>
                {
                    Ok(json!({ "ok": true, "unexpected": true }))
                },
                capability
                    if matches!(
                        capability,
                        "astrcode.session.control.dispose" | "astrcode.session.state.write"
                    ) && self.marker == "false_ack" =>
                {
                    Ok(json!({ "ok": false }))
                },
                "astrcode.event.emit" => Ok(json!({ "status": "accepted" })),
                "astrcode.session.control.dispose" | "astrcode.session.state.write" => {
                    Ok(json!({ "ok": true }))
                },
                _ => Ok(json!({
                    "capability": capability,
                    "host": self.marker,
                })),
            }
        }

        async fn open_stream(
            &self,
            capability: &str,
            input: Value,
        ) -> Result<ModelStream, ErrorPayload> {
            assert_eq!(capability, "astrcode.llm.main_chat");
            assert_eq!(input["messages"][0]["role"], "user");
            Ok(ModelStream::from_stream(
                futures_util::stream::iter([
                    astrcode_extension_sdk::wire::protocol::ModelStreamEvent::ContentDelta {
                        content: "ty".into(),
                    },
                    astrcode_extension_sdk::wire::protocol::ModelStreamEvent::ContentDelta {
                        content: "ped".into(),
                    },
                    astrcode_extension_sdk::wire::protocol::ModelStreamEvent::Completed {
                        output: json!({ "content": "typed", "model": "main_llm" }),
                    },
                ]),
                CancellationToken::new(),
            ))
        }
    }

    #[async_trait]
    impl HostApi for RecordingHost {
        fn host_supports(&self, _operation: HostOperation) -> bool {
            true
        }

        async fn call(&self, capability: &str, _input: Value) -> Result<Value, ErrorPayload> {
            self.operations.lock().unwrap().push(
                HostOperation::from_wire_name(capability)
                    .unwrap_or_else(|| panic!("unknown host operation {capability}")),
            );
            Err(ErrorPayload::new(
                WireErrorCode::BackendUnavailable,
                "test backend unavailable",
            ))
        }

        async fn open_stream(
            &self,
            capability: &str,
            _input: Value,
        ) -> Result<ModelStream, ErrorPayload> {
            self.operations.lock().unwrap().push(
                HostOperation::from_wire_name(capability)
                    .unwrap_or_else(|| panic!("unknown host operation {capability}")),
            );
            Err(ErrorPayload::new(
                WireErrorCode::BackendUnavailable,
                "test backend unavailable",
            ))
        }
    }

    #[async_trait]
    impl HostApi for LimitedHost {
        fn host_supports(&self, operation: HostOperation) -> bool {
            operation == HostOperation::EventEmit
        }

        async fn call(&self, _capability: &str, _input: Value) -> Result<Value, ErrorPayload> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(ErrorPayload::new(
                WireErrorCode::BackendUnavailable,
                "test backend unavailable",
            ))
        }
    }

    #[tokio::test]
    async fn support_query_and_typed_preflight_use_the_handshake_catalog() {
        let host = Arc::new(LimitedHost::default());
        with_host_api(host.clone(), async {
            assert!(HostClient::host_supports(HostOperation::EventEmit).unwrap());
            assert!(!HostClient::host_supports(HostOperation::LlmMainChat).unwrap());

            expect_backend_error(HostClient::events().emit(HostEventEmitRequest {
                event_type: "review.completed".into(),
                schema_version: 1,
                payload: json!({}),
            }))
            .await;
            for error in [
                HostClient::models()
                    .main_chat(vec![LlmMessage::user("hello")])
                    .await
                    .unwrap_err(),
                HostClient::models()
                    .main_chat_collected(vec![LlmMessage::user("hello")])
                    .await
                    .unwrap_err(),
            ] {
                assert_eq!(error.code_enum(), Some(WireErrorCode::Unsupported));
            }
        })
        .await;
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn worker_clients_route_every_typed_host_operation() {
        let expected_operations = [
            HostOperation::EventEmit,
            HostOperation::LlmMainChat,
            HostOperation::LlmSmallChat,
            HostOperation::LlmMainChat,
            HostOperation::LlmSmallChat,
            HostOperation::ProcessSpawn,
            HostOperation::ProcessStart,
            HostOperation::ProcessRead,
            HostOperation::ProcessInput,
            HostOperation::ProcessInput,
            HostOperation::ProcessStatus,
            HostOperation::ProcessPromote,
            HostOperation::ProcessKill,
            HostOperation::ProcessList,
            HostOperation::NetworkClient,
            HostOperation::ExtensionHttpPublic,
            HostOperation::SessionRootCreate,
            HostOperation::SessionRootSubmitTurn,
            HostOperation::SessionRootState,
            HostOperation::SessionRootDispose,
            HostOperation::SessionControlInjectOrStart,
            HostOperation::SessionControlQueueOrStart,
            HostOperation::SessionControlDeferContext,
            HostOperation::SessionControlInterruptAndSubmit,
            HostOperation::SessionControlCancelTurn,
            HostOperation::SessionControlExecutionView,
            HostOperation::SessionControlState,
            HostOperation::SessionControlReactivate,
            HostOperation::SessionControlCreate,
            HostOperation::SessionControlSubmitTurn,
            HostOperation::SessionControlConfigureTools,
            HostOperation::SessionControlDispose,
            HostOperation::SessionHistoryList,
            HostOperation::SessionHistoryTranscript,
            HostOperation::SessionHistoryProviderMessages,
            HostOperation::SessionHistoryTokenUsage,
            HostOperation::SessionHistorySnapshot,
            HostOperation::SessionReadEvents,
            HostOperation::SessionStateRead,
            HostOperation::SessionStateWrite,
            HostOperation::WorkspaceApplyPatch,
            HostOperation::ToolResultRead,
            HostOperation::WorkspaceRead,
            HostOperation::WorkspaceWrite,
            HostOperation::WorkspaceEdit,
            HostOperation::WorkspaceList,
            HostOperation::WorkspaceGrep,
            HostOperation::WorkspaceGlob,
            HostOperation::SessionInspectList,
            HostOperation::SessionInspectSnapshot,
            HostOperation::SessionInspectReadModel,
            HostOperation::SessionInspectProviderMessages,
        ];
        let covered = expected_operations.iter().copied().collect::<HashSet<_>>();
        let expected = crate::host::internal::HOST_OPERATION_SPECS
            .iter()
            .map(|spec| spec.operation)
            .collect::<HashSet<_>>();
        assert_eq!(covered, expected, "worker client operation coverage");

        let host = Arc::new(RecordingHost::default());
        with_host_api(host.clone(), async {
            let target = || HostSessionTargetRequest {
                target_session_id: "child-1".into(),
            };
            let input = || HostSessionInputRequest {
                target_session_id: "child-1".into(),
                content: "continue".into(),
            };
            let messages = || vec![LlmMessage::user("hello")];

            expect_backend_error(HostClient::events().emit(HostEventEmitRequest {
                event_type: "review.completed".into(),
                schema_version: 1,
                payload: json!({ "status": "ok" }),
            }))
            .await;
            expect_backend_error(HostClient::models().main_chat(messages())).await;
            expect_backend_error(HostClient::models().small_chat(messages())).await;
            expect_backend_error(HostClient::models().main_chat_collected(messages())).await;
            expect_backend_error(HostClient::models().small_chat_collected(messages())).await;
            expect_backend_error(HostClient::process().spawn(HostProcessRequest::new("true")))
                .await;
            expect_backend_error(HostClient::process().start(HostProcessStartRequest::new("true")))
                .await;
            let process_target = || HostProcessTargetRequest {
                id: "process-1".into(),
            };
            expect_backend_error(HostClient::process().read(HostProcessReadRequest {
                id: "process-1".into(),
                wait_ms: None,
            }))
            .await;
            expect_backend_error(HostClient::process().write("process-1", "continue\n")).await;
            expect_backend_error(HostClient::process().close_stdin("process-1")).await;
            expect_backend_error(HostClient::process().status(process_target())).await;
            expect_backend_error(HostClient::process().promote(process_target())).await;
            expect_backend_error(HostClient::process().kill(process_target())).await;
            expect_backend_error(HostClient::process().list()).await;
            expect_backend_error(
                HostClient::network().send(HostNetworkRequest::get("https://example.com")),
            )
            .await;
            expect_backend_error(HostClient::extension_http().dispatch_public(
                ExtensionHttpDispatchRequest::new(
                    crate::extension::ExtensionHttpMethod::Get,
                    "/health",
                ),
            ))
            .await;
            expect_backend_error(
                HostClient::session_control().create_root(HostCreateRootSessionRequest::default()),
            )
            .await;
            expect_backend_error(
                HostClient::session_control()
                    .submit_root_turn(HostRootSubmitTurnRequest::new("root-1", "continue")),
            )
            .await;
            expect_backend_error(HostClient::session_control().root_state(
                HostSessionTargetRequest {
                    target_session_id: "root-1".into(),
                },
            ))
            .await;
            expect_backend_error(HostClient::session_control().dispose_root(
                HostSessionTargetRequest {
                    target_session_id: "root-1".into(),
                },
            ))
            .await;
            expect_backend_error(HostClient::session_control().inject_or_start(input())).await;
            expect_backend_error(HostClient::session_control().queue_or_start(input())).await;
            expect_backend_error(HostClient::session_control().defer_context(input())).await;
            expect_backend_error(HostClient::session_control().interrupt_and_submit(input())).await;
            expect_backend_error(HostClient::session_control().cancel_turn(target())).await;
            expect_backend_error(HostClient::session_control().execution_view(target())).await;
            expect_backend_error(HostClient::session_control().state(target())).await;
            expect_backend_error(HostClient::session_control().reactivate(target())).await;
            expect_backend_error(
                HostClient::session_control().create_child(HostCreateSessionRequest::new("child")),
            )
            .await;
            expect_backend_error(
                HostClient::session_control()
                    .submit_turn(HostSubmitTurnRequest::background("child-1", "review")),
            )
            .await;
            expect_backend_error(HostClient::session_control().configure_tools(
                HostConfigureSessionToolsRequest {
                    session_id: "child-1".into(),
                    selection: SessionToolSelectionDto::no_tools(),
                },
            ))
            .await;
            expect_backend_error(
                HostClient::session_control().recycle(HostRecycleSessionRequest::new("child-1")),
            )
            .await;
            expect_backend_error(HostClient::session_history().list_summaries()).await;
            expect_backend_error(HostClient::session_history().transcript(target())).await;
            expect_backend_error(HostClient::session_history().provider_messages(target())).await;
            expect_backend_error(HostClient::session_history().token_usage(target())).await;
            expect_backend_error(HostClient::session_history().snapshot(target())).await;
            expect_backend_error(
                HostClient::session_history()
                    .events_page(HostSessionEventsPageRequest::new("child-1")),
            )
            .await;
            expect_backend_error(
                HostClient::session_state()
                    .read(HostSessionStateReadRequest { key: "goal".into() }),
            )
            .await;
            expect_backend_error(
                HostClient::session_state().write(HostSessionStateWriteRequest {
                    key: "goal".into(),
                    content: "active".into(),
                }),
            )
            .await;
            expect_backend_error(HostClient::workspace().apply_patch(
                HostWorkspaceApplyPatchRequest {
                    patch: "*** Begin Patch\n*** End Patch".into(),
                },
            ))
            .await;
            expect_backend_error(HostClient::tool_results().read(HostToolResultReadRequest {
                artifact_id: "result.txt".into(),
                byte_offset: 0,
                max_bytes: 1_024,
            }))
            .await;
            expect_backend_error(HostClient::workspace().read(HostWorkspaceReadRequest {
                path: "notes.txt".into(),
                max_bytes: None,
                line_offset: 0,
                line_limit: None,
            }))
            .await;
            expect_backend_error(HostClient::workspace().write(HostWorkspaceWriteRequest {
                path: "notes.txt".into(),
                content: "hello".into(),
                create_dirs: false,
            }))
            .await;
            expect_backend_error(HostClient::workspace().edit(HostWorkspaceEditRequest {
                path: "notes.txt".into(),
                old_text: Some("hello".into()),
                new_text: Some("hi".into()),
                replace_all: false,
                edits: Vec::new(),
            }))
            .await;
            expect_backend_error(HostClient::workspace().list(HostWorkspaceListRequest {
                path: ".".into(),
                depth: 1,
                limit: None,
            }))
            .await;
            expect_backend_error(HostClient::workspace().grep(HostWorkspaceGrepRequest {
                pattern: "hello".into(),
                path: None,
                offset: 0,
                max_matches: None,
                max_bytes: None,
                max_line_chars: None,
                recursive: true,
                multiline: false,
                path_filters: Vec::new(),
                before_context: 0,
                after_context: 0,
                mode: HostWorkspaceGrepMode::FilesWithMatches,
            }))
            .await;
            expect_backend_error(HostClient::workspace().glob(HostWorkspaceGlobRequest {
                pattern: "**/*.rs".into(),
                root: None,
                offset: 0,
                max_matches: None,
                respect_gitignore: true,
                include_hidden: true,
                include_directories: true,
            }))
            .await;
            expect_backend_error(HostClient::session_inspect().list()).await;
            expect_backend_error(HostClient::session_inspect().snapshot("session-1")).await;
            expect_backend_error(HostClient::session_inspect().read_model("session-1")).await;
            expect_backend_error(HostClient::session_inspect().provider_messages("session-1"))
                .await;
        })
        .await;

        assert_eq!(*host.operations.lock().unwrap(), expected_operations);
    }

    #[tokio::test]
    async fn scoped_host_api_is_concurrent_and_serves_typed_domain_clients() {
        let (left, right) = tokio::join!(
            with_host_api(
                Arc::new(MockHost { marker: "left" }),
                invoke_host("astrcode.test", json!({})),
            ),
            with_host_api(
                Arc::new(MockHost { marker: "right" }),
                invoke_host("astrcode.test", json!({})),
            ),
        );
        assert_eq!(left.unwrap()["host"], "left");
        assert_eq!(right.unwrap()["host"], "right");

        with_host_api(Arc::new(MockHost { marker: "typed" }), async {
            let chat = HostClient::models()
                .main_chat(vec![LlmMessage::user("hello")])
                .await
                .unwrap();
            assert_eq!(chat.content, "typed");
            let stream = HostClient::models()
                .main_chat_collected(vec![LlmMessage::user("hello")])
                .await
                .unwrap();
            assert_eq!(stream.content, "typed");

            let network = HostClient::network()
                .send(HostNetworkRequest::get("https://example.com"))
                .await
                .unwrap();
            assert_eq!(network.body, vec![0, 255]);

            let receipt = HostClient::events()
                .emit(HostEventEmitRequest {
                    event_type: "review.completed".into(),
                    schema_version: 1,
                    payload: json!({ "status": "ok" }),
                })
                .await
                .unwrap();
            assert_eq!(receipt, HostEventEmitOutput::Accepted);

            let history = HostClient::session_history()
                .list_summaries()
                .await
                .unwrap();
            assert_eq!(history.sessions[0].session_id.as_str(), "session-1");

            let root = HostClient::session_control()
                .create_root(HostCreateRootSessionRequest::default())
                .await
                .unwrap();
            assert_eq!(root.session_id, "root-1");

            let created = HostClient::session_control()
                .create_child(HostCreateSessionRequest::new("reviewer"))
                .await
                .unwrap();
            assert_eq!(created.session_id, "child-1");

            let submitted = HostClient::session_control()
                .submit_turn(HostSubmitTurnRequest::background("child-1", "review"))
                .await
                .unwrap();
            assert_eq!(
                submitted,
                HostSubmitTurnOutput::Backgrounded {
                    task_id: "turn-1".into(),
                    session_id: "child-1".into()
                }
            );

            HostClient::session_control()
                .recycle(HostRecycleSessionRequest::new("child-1"))
                .await
                .unwrap();

            HostClient::session_state()
                .write(HostSessionStateWriteRequest {
                    key: "goal".into(),
                    content: "active".into(),
                })
                .await
                .unwrap();
            let state = HostClient::session_state()
                .read(HostSessionStateReadRequest { key: "goal".into() })
                .await
                .unwrap();
            assert_eq!(state.content.as_deref(), Some("active"));
        })
        .await;

        for marker in ["false_ack", "invalid_ack"] {
            let error = with_host_api(Arc::new(MockHost { marker }), async {
                HostClient::session_control()
                    .recycle(HostRecycleSessionRequest::new("child-1"))
                    .await
            })
            .await
            .unwrap_err();
            assert_eq!(
                error.code_enum(),
                Some(WireErrorCode::InvalidResponse),
                "{marker}"
            );
        }

        let host = Arc::new(LimitedHost::default());
        let error = with_host_api(host.clone(), async {
            tokio::spawn(async {
                HostClient::events()
                    .emit(HostEventEmitRequest {
                        event_type: "detached".into(),
                        schema_version: 1,
                        payload: json!({}),
                    })
                    .await
            })
            .await
            .unwrap()
            .unwrap_err()
        })
        .await;
        assert_eq!(error.code_enum(), Some(WireErrorCode::ContextUnavailable));
        assert_eq!(host.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn background_host_routes_root_domain_operations_and_reads_the_catalog() {
        let host = Arc::new(RecordingHost::default());
        let background = BackgroundHost::new(host.clone());
        let sessions = background.root_sessions();
        expect_backend_error(sessions.create_root(HostCreateRootSessionRequest::default())).await;
        expect_backend_error(
            sessions.submit_root_turn(HostRootSubmitTurnRequest::new("root-1", "continue")),
        )
        .await;
        expect_backend_error(sessions.root_state(HostSessionTargetRequest {
            target_session_id: "root-1".into(),
        }))
        .await;
        expect_backend_error(sessions.dispose_root(HostSessionTargetRequest {
            target_session_id: "root-1".into(),
        }))
        .await;
        assert_eq!(
            *host.operations.lock().unwrap(),
            [
                HostOperation::SessionRootCreate,
                HostOperation::SessionRootSubmitTurn,
                HostOperation::SessionRootState,
                HostOperation::SessionRootDispose,
            ]
        );

        let limited = BackgroundHost::new(Arc::new(LimitedHost::default()));
        assert!(limited.host_supports(HostOperation::EventEmit));
        assert!(!limited.host_supports(HostOperation::SessionRootState));
    }

    /// 跨 crate 无法复用 sdk 内部的 loopback 传输,测试内按帧格式(`{len}\n{payload}`)写一个。
    struct DuplexTransport {
        reader: Arc<tokio::sync::Mutex<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
        writer: Arc<tokio::sync::Mutex<tokio::io::WriteHalf<tokio::io::DuplexStream>>>,
    }

    impl DuplexTransport {
        fn pair() -> (Self, Self) {
            let (left, right) = tokio::io::duplex(64 * 1024);
            let (left_reader, left_writer) = tokio::io::split(left);
            let (right_reader, right_writer) = tokio::io::split(right);
            (
                Self {
                    reader: Arc::new(tokio::sync::Mutex::new(left_reader)),
                    writer: Arc::new(tokio::sync::Mutex::new(left_writer)),
                },
                Self {
                    reader: Arc::new(tokio::sync::Mutex::new(right_reader)),
                    writer: Arc::new(tokio::sync::Mutex::new(right_writer)),
                },
            )
        }
    }

    #[async_trait]
    impl astrcode_extension_sdk::wire::FrameTransport for DuplexTransport {
        async fn read_frame(
            &self,
        ) -> Result<Vec<u8>, astrcode_extension_sdk::wire::frame::FrameError> {
            use tokio::io::AsyncReadExt;
            let mut header = Vec::new();
            let mut reader = self.reader.lock().await;
            loop {
                let byte = reader.read_u8().await?;
                if byte == b'\n' {
                    break;
                }
                header.push(byte);
            }
            let size = std::str::from_utf8(&header)
                .unwrap()
                .parse::<usize>()
                .unwrap();
            let mut payload = vec![0; size];
            reader.read_exact(&mut payload).await?;
            Ok(payload)
        }

        async fn write_frame(
            &self,
            payload: &[u8],
        ) -> Result<(), astrcode_extension_sdk::wire::frame::FrameError> {
            use tokio::io::AsyncWriteExt;
            let mut writer = self.writer.lock().await;
            writer
                .write_all(format!("{}\n", payload.len()).as_bytes())
                .await?;
            writer.write_all(payload).await?;
            writer.flush().await?;
            Ok(())
        }
    }

    #[derive(Default)]
    struct RootStateHostHandler {
        parents: Mutex<Vec<Option<String>>>,
    }

    #[async_trait]
    impl astrcode_extension_sdk::wire::PeerInvokeHandler for RootStateHostHandler {
        async fn invoke(
            &self,
            invocation: astrcode_extension_sdk::wire::InboundInvoke,
        ) -> Result<
            astrcode_extension_sdk::wire::InvocationResponse,
            astrcode_extension_sdk::wire::protocol::ErrorPayload,
        > {
            self.parents
                .lock()
                .unwrap()
                .push(invocation.request.parent_invoke_id.clone());
            assert_eq!(invocation.request.operation, "astrcode.session.root.state");
            Ok(astrcode_extension_sdk::wire::InvocationResponse::Unary(
                json!({
                    "lifecycle": "active",
                    "phase": "idle",
                    "active_turn_id": null,
                    "queued_inputs": 0,
                    "message_count": 0
                }),
            ))
        }
    }

    struct RejectAllWorkerHandler;

    #[async_trait]
    impl astrcode_extension_sdk::wire::PeerInvokeHandler for RejectAllWorkerHandler {
        async fn invoke(
            &self,
            _invocation: astrcode_extension_sdk::wire::InboundInvoke,
        ) -> Result<
            astrcode_extension_sdk::wire::InvocationResponse,
            astrcode_extension_sdk::wire::protocol::ErrorPayload,
        > {
            Err(astrcode_extension_sdk::wire::protocol::ErrorPayload::new(
                WireErrorCode::UnknownHandler,
                "unexpected host invoke",
            ))
        }
    }

    #[tokio::test]
    async fn background_host_invokes_over_real_peer_without_parent_invoke_id() {
        use astrcode_extension_sdk::wire::{
            FeatureName, HostInitialization, Peer, PeerInfo, WorkerInitialization,
            manifest::InitializeManifest,
        };

        let features = std::collections::BTreeSet::from([
            FeatureName::nested_invoke_v1(),
            FeatureName::model_stream_v1(),
        ]);
        let (host_transport, worker_transport) = DuplexTransport::pair();
        let info = |name: &str| PeerInfo {
            name: name.into(),
            version: None,
        };
        let host = Peer::new(host_transport, info("host"));
        let worker = Peer::new(worker_transport, info("test-extension"));

        let mut host_initialization = HostInitialization::new("initialize-1", "test-extension");
        host_initialization.supported_features = features.clone();
        host_initialization.host_operations = vec!["astrcode.session.root.state".into()];
        let worker_initialization = WorkerInitialization {
            supported_features: features,
            ..WorkerInitialization::new(InitializeManifest::default())
        };
        let (host, worker) = tokio::join!(
            host.initialize(host_initialization),
            worker.accept(worker_initialization)
        );
        let (host, worker) = tokio::join!(
            host.unwrap().0.activate("activate-1", Value::Null),
            worker.unwrap().accept_activation(|_| async { Ok(()) })
        );
        let (_host_handle, host_driver) = host.unwrap().into_runtime();
        let (worker_handle, worker_driver) = worker.unwrap().into_runtime();

        let handler = Arc::new(RootStateHostHandler::default());
        let host_task = tokio::spawn(host_driver.run(Arc::clone(&handler)));
        let worker_task = tokio::spawn(worker_driver.run(Arc::new(RejectAllWorkerHandler)));

        let background = BackgroundHost::new(Arc::new(V3PeerHostApi::new(worker_handle)));
        assert!(background.host_supports(HostOperation::SessionRootState));
        assert!(!background.host_supports(HostOperation::WorkspaceRead));

        let state = tokio::spawn(async move {
            background
                .root_sessions()
                .root_state(HostSessionTargetRequest {
                    target_session_id: "root-1".into(),
                })
                .await
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(state.phase, crate::session::SessionPhaseDto::Idle);

        assert_eq!(*handler.parents.lock().unwrap(), vec![None]);

        host_task.abort();
        worker_task.abort();
    }
}
