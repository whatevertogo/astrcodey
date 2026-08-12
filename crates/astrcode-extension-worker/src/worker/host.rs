//! Worker 侧调用宿主的抽象（可注入 mock）。

use std::{
    future::Future,
    sync::{Arc, OnceLock},
};

use async_trait::async_trait;
use serde_json::Value;

use crate::{
    WireErrorCode,
    host::internal::{
        HostClientTransport, TypedEventClient, TypedExtensionHttpClient, TypedModelClient,
        TypedNetworkClient, TypedProcessClient, TypedSessionControlClient,
        TypedSessionHistoryClient, TypedSessionInspectClient, TypedSessionStateClient,
        TypedWorkspaceClient,
    },
    model_stream::ModelStream,
    s5r::ErrorPayload,
};
#[cfg(test)]
use crate::{extension::ExtensionHttpDispatchRequest, llm::LlmMessage};
pub use crate::{
    host::{
        HostConfigureSessionToolsOutput, HostConfigureSessionToolsRequest, HostEventEmitOutput,
        HostEventEmitRequest, HostLlmChatOutput, HostLlmCollectedStreamOutput, HostLlmContent,
        HostLlmMessage, HostLlmRole, HostNetworkRedirectPolicy, HostNetworkRequest,
        HostNetworkResponse, HostOperation, HostProcessOutput, HostProcessRequest,
        HostSessionCancelOutput, HostSessionDeliveryOutput, HostSessionExecutionView,
        HostSessionInputRequest, HostSessionProviderMessagesOutput, HostSessionStateReadOutput,
        HostSessionStateReadRequest, HostSessionStateWriteRequest, HostSessionSummariesOutput,
        HostSessionSummary, HostSessionTokenUsage, HostSessionTokenUsageOutput,
        HostSessionTranscript, HostSessionTranscriptMessage, HostWorkspaceEditOutput,
        HostWorkspaceEditRequest, HostWorkspaceGlobOutput, HostWorkspaceGlobRequest,
        HostWorkspaceGrepMatch, HostWorkspaceGrepOutput, HostWorkspaceGrepRequest,
        HostWorkspaceListEntry, HostWorkspaceListOutput, HostWorkspaceListRequest,
        HostWorkspaceReadOutput, HostWorkspaceReadRequest, HostWorkspaceWriteOutput,
        HostWorkspaceWriteRequest,
    },
    session::{
        HostCreateSessionOutput, HostCreateSessionRequest, HostRecycleSessionRequest,
        HostRootSubmitTurnRequest, HostSessionEvent, HostSessionEventsPageOutput,
        HostSessionEventsPageRequest, HostSessionReactivateOutput, HostSessionStateOutput,
        HostSessionTargetRequest, HostSubmitTurnOutput, HostSubmitTurnRequest,
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
    peer: astrcode_extension_contract::PeerHandle,
}

impl V3PeerHostApi {
    pub fn new(peer: astrcode_extension_contract::PeerHandle) -> Self {
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
            .map_err(v3_invoke_error_to_payload)
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
            .map_err(v3_invoke_error_to_payload)?;
        Ok(ModelStream::from_stream(
            stream,
            tokio_util::sync::CancellationToken::new(),
        ))
    }
}

fn v3_invoke_error_to_payload(error: astrcode_extension_contract::InvokeError) -> ErrorPayload {
    use astrcode_extension_contract::InvokeError;

    match error {
        InvokeError::Local(error) | InvokeError::Remote(error) => error,
        InvokeError::DriverUnavailable => ErrorPayload::new(
            WireErrorCode::HostNotReady,
            "S5R 3.0 host peer driver is not running",
        ),
        InvokeError::PeerClosed => {
            ErrorPayload::new(WireErrorCode::PeerClosed, "S5R 3.0 host peer closed")
        },
    }
}

static HOST_API: OnceLock<Arc<dyn HostApi>> = OnceLock::new();

tokio::task_local! {
    static SCOPED_HOST_API: Arc<dyn HostApi>;
}

/// 在 `Worker::run_stdio` 启动前由运行时注入。
pub(crate) fn set_host_api(api: Arc<dyn HostApi>) -> Result<(), ()> {
    HOST_API.set(api).map_err(|_| ())
}

/// 在一个异步作用域内使用指定宿主 API，供 worker 单元测试隔离 mock。
///
/// 该作用域不会自动传播到 `tokio::spawn` 创建的新任务。
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

pub type ModelClient = TypedModelClient<WorkerHostTransport>;
pub type EventClient = TypedEventClient<WorkerHostTransport>;
pub type SessionControlClient = TypedSessionControlClient<WorkerHostTransport>;
pub type SessionHistoryClient = TypedSessionHistoryClient<WorkerHostTransport>;
pub type SessionStateClient = TypedSessionStateClient<WorkerHostTransport>;
pub type SessionInspectClient = TypedSessionInspectClient<WorkerHostTransport>;
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
    if let Ok(api) = SCOPED_HOST_API.try_with(Arc::clone) {
        return Ok(api);
    }
    HOST_API
        .get()
        .cloned()
        .ok_or_else(|| ErrorPayload::new(WireErrorCode::HostNotReady, "host peer not ready"))
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
                    astrcode_extension_contract::protocol::ModelStreamEvent::ContentDelta {
                        content: "ty".into(),
                    },
                    astrcode_extension_contract::protocol::ModelStreamEvent::ContentDelta {
                        content: "ped".into(),
                    },
                    astrcode_extension_contract::protocol::ModelStreamEvent::Completed {
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
            HostOperation::NetworkClient,
            HostOperation::ExtensionHttpPublic,
            HostOperation::SessionRootCreate,
            HostOperation::SessionRootSubmitTurn,
            HostOperation::SessionRootState,
            HostOperation::SessionControlInjectOrStart,
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
            expect_backend_error(HostClient::session_control().create_root()).await;
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
            expect_backend_error(HostClient::session_control().inject_or_start(input())).await;
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
            expect_backend_error(HostClient::workspace().read(HostWorkspaceReadRequest {
                path: "notes.txt".into(),
                max_bytes: None,
            }))
            .await;
            expect_backend_error(HostClient::workspace().write(HostWorkspaceWriteRequest {
                path: "notes.txt".into(),
                content: "hello".into(),
            }))
            .await;
            expect_backend_error(HostClient::workspace().edit(HostWorkspaceEditRequest {
                path: "notes.txt".into(),
                old_text: "hello".into(),
                new_text: "hi".into(),
                replace_all: false,
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
                max_matches: None,
                max_bytes: None,
                max_line_chars: None,
            }))
            .await;
            expect_backend_error(HostClient::workspace().glob(HostWorkspaceGlobRequest {
                pattern: "**/*.rs".into(),
                root: None,
                max_matches: None,
                include_ignored: false,
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
            assert_eq!(stream.chunks, ["ty", "ped"]);

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

            let root = HostClient::session_control().create_root().await.unwrap();
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
    }
}
