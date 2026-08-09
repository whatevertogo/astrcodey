//! Worker 侧调用宿主的抽象（可注入 mock）。

use std::{
    future::Future,
    sync::{Arc, OnceLock},
};

use astrcode_core::wire::WireErrorCode;
use async_trait::async_trait;
use serde_json::Value;

#[cfg(test)]
use crate::{extension::ExtensionHttpDispatchRequest, llm::LlmMessage};
use crate::{
    host::{
        HostClientTransport, HostOperation, TypedEventClient, TypedExtensionHttpClient,
        TypedModelClient, TypedNetworkClient, TypedProcessClient, TypedSessionControlClient,
        TypedSessionHistoryClient, TypedSessionInspectClient, TypedSessionStateClient,
        TypedWorkspaceClient,
    },
    runtime::{OutboundInvokeControl, Peer, PeerError},
    s5r::ErrorPayload,
};
pub use crate::{
    host::{
        HostConfigureSessionToolsOutput, HostConfigureSessionToolsRequest, HostEventEmitOutput,
        HostEventEmitRequest, HostLlmChatOutput, HostLlmCollectedStreamOutput, HostLlmContent,
        HostLlmMessage, HostLlmRole, HostLlmTextDelta, HostNetworkRedirectPolicy,
        HostNetworkRequest, HostNetworkResponse, HostProcessOutput, HostProcessRequest,
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
    async fn call(&self, capability: &str, input: Value) -> Result<Value, ErrorPayload>;

    async fn call_stream(&self, capability: &str, input: Value) -> Result<Value, ErrorPayload>;
}

pub(crate) struct PeerHostApi<T: crate::runtime::FrameTransport + 'static> {
    peer: Arc<Peer<T>>,
}

impl<T: crate::runtime::FrameTransport + 'static> PeerHostApi<T> {
    pub fn new(peer: Arc<Peer<T>>) -> Self {
        Self { peer }
    }
}

#[async_trait]
impl<T> HostApi for PeerHostApi<T>
where
    T: crate::runtime::FrameTransport + Send + Sync + 'static,
{
    async fn call(&self, capability: &str, input: Value) -> Result<Value, ErrorPayload> {
        self.peer
            .invoke(capability, input, OutboundInvokeControl::default())
            .await
            .map_err(peer_error_to_payload)
    }

    async fn call_stream(&self, capability: &str, input: Value) -> Result<Value, ErrorPayload> {
        self.peer
            .invoke_stream_collect(capability, input)
            .await
            .map_err(peer_error_to_payload)
    }
}

fn peer_error_to_payload(err: PeerError) -> ErrorPayload {
    match err {
        PeerError::Closed => ErrorPayload::new(WireErrorCode::PeerClosed, "host peer closed"),
        PeerError::Timeout => ErrorPayload::new(WireErrorCode::Timeout, "host invoke timed out"),
        PeerError::Busy => ErrorPayload::new(
            WireErrorCode::PeerBusy,
            "host invoke concurrency limit reached",
        )
        .retryable(true),
        PeerError::Payload(payload) => payload,
        PeerError::Msg(msg) => ErrorPayload::new(WireErrorCode::Transport, msg),
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
        call_host(operation.wire_name(), input).await
    }

    async fn invoke_collected_stream(
        &self,
        operation: HostOperation,
        input: Value,
    ) -> Result<Value, Self::Error> {
        call_host_stream(operation.wire_name(), input).await
    }

    fn client_error(code: &'static str, message: String) -> Self::Error {
        ErrorPayload::new(
            WireErrorCode::parse(code).unwrap_or(WireErrorCode::InvalidResponse),
            message,
        )
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

async fn call_host(capability: &str, input: Value) -> Result<Value, ErrorPayload> {
    host_api()?.call(capability, input).await
}

async fn call_host_stream(capability: &str, input: Value) -> Result<Value, ErrorPayload> {
    host_api()?.call_stream(capability, input).await
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

/// Invokes a raw host capability for transport or request-context integration tests.
pub async fn invoke_host(capability: &str, input: Value) -> Result<Value, ErrorPayload> {
    call_host(capability, input).await
}
#[cfg(test)]
mod host_tests {
    use std::{
        collections::HashSet,
        future::Future,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::*;
    use crate::{s5r::ErrorPayload, session::SessionToolSelectionDto};

    struct MockHost {
        marker: &'static str,
    }

    #[derive(Default)]
    struct RecordingHost {
        operations: Mutex<Vec<HostOperation>>,
    }

    async fn expect_backend_error<T>(future: impl Future<Output = Result<T, ErrorPayload>>) {
        let Err(error) = future.await else {
            panic!("mock host unexpectedly succeeded");
        };
        assert_eq!(error.code_enum(), Some(WireErrorCode::BackendUnavailable));
    }

    #[test]
    fn peer_error_mapping_preserves_host_payload_and_transport_contracts() {
        let mut expected = ErrorPayload::new(
            WireErrorCode::ProviderRateLimited,
            "provider rate limit reached",
        )
        .with_hint("retry after the provider backoff")
        .retryable(true);
        expected.details = Some(json!({ "retry_after_ms": 250 }));

        let actual = peer_error_to_payload(PeerError::Payload(expected.clone()));
        assert_eq!(actual.code, expected.code);
        assert_eq!(actual.message, expected.message);
        assert_eq!(actual.hint, expected.hint);
        assert_eq!(actual.retryable, expected.retryable);
        assert_eq!(actual.details, expected.details);

        let transport_errors = [
            (PeerError::Closed, "peer_closed", "host peer closed", false),
            (
                PeerError::Timeout,
                "timeout",
                "host invoke timed out",
                false,
            ),
            (
                PeerError::Busy,
                "peer_busy",
                "host invoke concurrency limit reached",
                true,
            ),
            (
                PeerError::Msg("invalid frame".into()),
                "transport_error",
                "invalid frame",
                false,
            ),
        ];
        for (error, code, message, retryable) in transport_errors {
            let payload = peer_error_to_payload(error);
            assert_eq!(payload.code, code);
            assert_eq!(payload.message, message);
            assert_eq!(payload.retryable, retryable);
            assert!(payload.hint.is_none());
            assert!(payload.details.is_none());
        }
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

        async fn call_stream(&self, capability: &str, input: Value) -> Result<Value, ErrorPayload> {
            if capability == "astrcode.llm.main_chat" {
                assert_eq!(input["messages"][0]["role"], "user");
                return Ok(json!({
                    "content": "typed",
                    "model": "main_llm",
                    "chunks": [{ "delta": "ty" }, { "delta": "ped" }]
                }));
            }
            self.call(capability, input).await
        }
    }

    #[async_trait]
    impl HostApi for RecordingHost {
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

        async fn call_stream(&self, capability: &str, input: Value) -> Result<Value, ErrorPayload> {
            self.call(capability, input).await
        }
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
        let expected = crate::host::HOST_OPERATION_SPECS
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
            expect_backend_error(HostClient::models().main_chat_stream(messages())).await;
            expect_backend_error(HostClient::models().small_chat_stream(messages())).await;
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
                .main_chat_stream(vec![LlmMessage::user("hello")])
                .await
                .unwrap();
            assert_eq!(stream.chunks[0].delta, "ty");
            assert_eq!(stream.chunks[1].delta, "ped");

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
