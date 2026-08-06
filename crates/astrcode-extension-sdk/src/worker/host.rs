//! Worker 侧调用宿主的抽象（可注入 mock）。

use std::{
    future::Future,
    sync::{Arc, OnceLock},
};

use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{
    extension::{ExtensionHttpRequest, ExtensionHttpResponse},
    host::{HostAcknowledgement, HostLlmChatRequest, HostOperation},
    llm::LlmMessage,
    runtime::{OutboundInvokeControl, Peer, PeerError},
    s5r::ErrorPayload,
    session_inspect::{
        SessionHistorySnapshotOutput, SessionInspectListOutput,
        SessionInspectProviderMessagesOutput, SessionInspectReadModelOutput,
        SessionInspectSnapshotOutput,
    },
};
pub use crate::{
    host::{
        HostConfigureSessionToolsOutput, HostConfigureSessionToolsRequest, HostLlmChatOutput,
        HostLlmCollectedStreamOutput, HostLlmTextDelta, HostNetworkRedirectPolicy,
        HostNetworkRequest, HostNetworkResponse, HostProcessOutput, HostProcessRequest,
        HostSessionCancelOutput, HostSessionDeliveryOutput, HostSessionExecutionView,
        HostSessionInputRequest, HostSessionProviderMessagesOutput, HostSessionSummariesOutput,
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
    caller_extension_id: String,
}

impl<T: crate::runtime::FrameTransport + 'static> PeerHostApi<T> {
    pub fn new(peer: Arc<Peer<T>>, caller_extension_id: impl Into<String>) -> Self {
        Self {
            peer,
            caller_extension_id: caller_extension_id.into(),
        }
    }
}

#[async_trait]
impl<T> HostApi for PeerHostApi<T>
where
    T: crate::runtime::FrameTransport + Send + Sync + 'static,
{
    async fn call(&self, capability: &str, input: Value) -> Result<Value, ErrorPayload> {
        self.peer
            .invoke(
                capability,
                input,
                Some(self.caller_extension_id.as_str()),
                OutboundInvokeControl::default(),
            )
            .await
            .map_err(peer_error_to_payload)
    }

    async fn call_stream(&self, capability: &str, input: Value) -> Result<Value, ErrorPayload> {
        self.peer
            .invoke_stream_collect(capability, input, Some(self.caller_extension_id.as_str()))
            .await
            .map_err(peer_error_to_payload)
    }
}

fn peer_error_to_payload(err: PeerError) -> ErrorPayload {
    match err {
        PeerError::Closed => ErrorPayload::new("peer_closed", "host peer closed"),
        PeerError::Timeout => ErrorPayload::new("timeout", "host invoke timed out"),
        PeerError::Busy => {
            ErrorPayload::new("peer_busy", "host invoke concurrency limit reached").retryable(true)
        },
        PeerError::Payload(payload) => payload,
        PeerError::Msg(msg) => ErrorPayload::new("transport_error", msg),
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

/// Worker-side entry point for typed host domains.
#[derive(Debug, Clone, Copy)]
pub struct HostClient;

impl HostClient {
    pub const fn models() -> ModelClient {
        ModelClient::new()
    }

    pub const fn session_control() -> SessionControlClient {
        SessionControlClient::new()
    }

    pub const fn session_history() -> SessionHistoryClient {
        SessionHistoryClient::new()
    }

    pub const fn session_inspect() -> SessionInspectClient {
        SessionInspectClient::new()
    }

    pub const fn workspace() -> WorkspaceClient {
        WorkspaceClient::new()
    }

    pub const fn process() -> ProcessClient {
        ProcessClient::new()
    }

    pub const fn network() -> NetworkClient {
        NetworkClient::new()
    }

    pub const fn extension_http() -> ExtensionHttpClient {
        ExtensionHttpClient::new()
    }
}

macro_rules! domain_client {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy)]
        pub struct $name {
            _private: (),
        }

        impl $name {
            const fn new() -> Self {
                Self { _private: () }
            }
        }
    };
}

domain_client!(ModelClient);
domain_client!(SessionControlClient);
domain_client!(SessionHistoryClient);
domain_client!(SessionInspectClient);
domain_client!(WorkspaceClient);
domain_client!(ProcessClient);
domain_client!(NetworkClient);
domain_client!(ExtensionHttpClient);

impl ModelClient {
    pub async fn main_chat(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<HostLlmChatOutput, ErrorPayload> {
        invoke(
            HostOperation::LlmMainChat,
            &HostLlmChatRequest::new(messages),
        )
        .await
    }

    pub async fn small_chat(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<HostLlmChatOutput, ErrorPayload> {
        invoke(
            HostOperation::LlmSmallChat,
            &HostLlmChatRequest::new(messages),
        )
        .await
    }

    /// Runs the main model stream and returns all ordered text deltas after completion.
    pub async fn main_chat_stream(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<HostLlmCollectedStreamOutput, ErrorPayload> {
        invoke_collected_stream(
            HostOperation::LlmMainChat,
            &HostLlmChatRequest::new(messages),
        )
        .await
    }

    /// Runs the small model stream and returns all ordered text deltas after completion.
    pub async fn small_chat_stream(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<HostLlmCollectedStreamOutput, ErrorPayload> {
        invoke_collected_stream(
            HostOperation::LlmSmallChat,
            &HostLlmChatRequest::new(messages),
        )
        .await
    }
}

impl SessionControlClient {
    pub async fn create_root(&self) -> Result<HostCreateSessionOutput, ErrorPayload> {
        invoke(HostOperation::SessionRootCreate, &json!({})).await
    }

    pub async fn submit_root_turn(
        &self,
        request: HostRootSubmitTurnRequest,
    ) -> Result<HostSubmitTurnOutput, ErrorPayload> {
        invoke(HostOperation::SessionRootSubmitTurn, &request).await
    }

    pub async fn root_state(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionStateOutput, ErrorPayload> {
        invoke(HostOperation::SessionRootState, &request).await
    }

    pub async fn inject_or_start(
        &self,
        request: HostSessionInputRequest,
    ) -> Result<HostSessionDeliveryOutput, ErrorPayload> {
        invoke(HostOperation::SessionControlInjectOrStart, &request).await
    }

    pub async fn interrupt_and_submit(
        &self,
        request: HostSessionInputRequest,
    ) -> Result<HostSessionDeliveryOutput, ErrorPayload> {
        invoke(HostOperation::SessionControlInterruptAndSubmit, &request).await
    }

    pub async fn cancel_turn(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionCancelOutput, ErrorPayload> {
        invoke(HostOperation::SessionControlCancelTurn, &request).await
    }

    pub async fn execution_view(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionExecutionView, ErrorPayload> {
        invoke(HostOperation::SessionControlExecutionView, &request).await
    }

    pub async fn state(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionStateOutput, ErrorPayload> {
        invoke(HostOperation::SessionControlState, &request).await
    }

    pub async fn reactivate(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionReactivateOutput, ErrorPayload> {
        invoke(HostOperation::SessionControlReactivate, &request).await
    }

    pub async fn create_child(
        &self,
        request: HostCreateSessionRequest,
    ) -> Result<HostCreateSessionOutput, ErrorPayload> {
        invoke(HostOperation::SessionControlCreate, &request).await
    }

    pub async fn submit_turn(
        &self,
        request: HostSubmitTurnRequest,
    ) -> Result<HostSubmitTurnOutput, ErrorPayload> {
        invoke(HostOperation::SessionControlSubmitTurn, &request).await
    }

    pub async fn configure_tools(
        &self,
        request: HostConfigureSessionToolsRequest,
    ) -> Result<HostConfigureSessionToolsOutput, ErrorPayload> {
        invoke(HostOperation::SessionControlConfigureTools, &request).await
    }

    pub async fn recycle(&self, request: HostRecycleSessionRequest) -> Result<(), ErrorPayload> {
        invoke_unit(HostOperation::SessionControlDispose, &request).await
    }
}

impl SessionHistoryClient {
    pub async fn list_summaries(&self) -> Result<HostSessionSummariesOutput, ErrorPayload> {
        invoke(HostOperation::SessionHistoryList, &json!({})).await
    }

    pub async fn transcript(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionTranscript, ErrorPayload> {
        invoke(HostOperation::SessionHistoryTranscript, &request).await
    }

    pub async fn provider_messages(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionProviderMessagesOutput, ErrorPayload> {
        invoke(HostOperation::SessionHistoryProviderMessages, &request).await
    }

    pub async fn token_usage(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionTokenUsageOutput, ErrorPayload> {
        invoke(HostOperation::SessionHistoryTokenUsage, &request).await
    }

    pub async fn events_page(
        &self,
        request: HostSessionEventsPageRequest,
    ) -> Result<HostSessionEventsPageOutput, ErrorPayload> {
        invoke(HostOperation::SessionReadEvents, &request).await
    }

    pub async fn snapshot(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<SessionHistorySnapshotOutput, ErrorPayload> {
        invoke(HostOperation::SessionHistorySnapshot, &request).await
    }
}

impl SessionInspectClient {
    pub async fn list(&self) -> Result<SessionInspectListOutput, ErrorPayload> {
        invoke(HostOperation::SessionInspectList, &json!({})).await
    }

    pub async fn snapshot(
        &self,
        session_id: &str,
    ) -> Result<SessionInspectSnapshotOutput, ErrorPayload> {
        self.inspect(HostOperation::SessionInspectSnapshot, session_id)
            .await
    }

    pub async fn read_model(
        &self,
        session_id: &str,
    ) -> Result<SessionInspectReadModelOutput, ErrorPayload> {
        self.inspect(HostOperation::SessionInspectReadModel, session_id)
            .await
    }

    pub async fn provider_messages(
        &self,
        session_id: &str,
    ) -> Result<SessionInspectProviderMessagesOutput, ErrorPayload> {
        self.inspect(HostOperation::SessionInspectProviderMessages, session_id)
            .await
    }

    async fn inspect<T>(
        &self,
        operation: HostOperation,
        session_id: &str,
    ) -> Result<T, ErrorPayload>
    where
        T: DeserializeOwned,
    {
        invoke(operation, &json!({ "session_id": session_id })).await
    }
}

impl WorkspaceClient {
    pub async fn read(
        &self,
        request: HostWorkspaceReadRequest,
    ) -> Result<HostWorkspaceReadOutput, ErrorPayload> {
        invoke(HostOperation::WorkspaceRead, &request).await
    }

    pub async fn write(
        &self,
        request: HostWorkspaceWriteRequest,
    ) -> Result<HostWorkspaceWriteOutput, ErrorPayload> {
        invoke(HostOperation::WorkspaceWrite, &request).await
    }

    pub async fn edit(
        &self,
        request: HostWorkspaceEditRequest,
    ) -> Result<HostWorkspaceEditOutput, ErrorPayload> {
        invoke(HostOperation::WorkspaceEdit, &request).await
    }

    pub async fn list(
        &self,
        request: HostWorkspaceListRequest,
    ) -> Result<HostWorkspaceListOutput, ErrorPayload> {
        invoke(HostOperation::WorkspaceList, &request).await
    }

    pub async fn grep(
        &self,
        request: HostWorkspaceGrepRequest,
    ) -> Result<HostWorkspaceGrepOutput, ErrorPayload> {
        invoke(HostOperation::WorkspaceGrep, &request).await
    }

    pub async fn glob(
        &self,
        request: HostWorkspaceGlobRequest,
    ) -> Result<HostWorkspaceGlobOutput, ErrorPayload> {
        invoke(HostOperation::WorkspaceGlob, &request).await
    }
}

impl ProcessClient {
    pub async fn spawn(
        &self,
        request: HostProcessRequest,
    ) -> Result<HostProcessOutput, ErrorPayload> {
        invoke(HostOperation::ProcessSpawn, &request).await
    }
}

impl NetworkClient {
    pub async fn send(
        &self,
        request: HostNetworkRequest,
    ) -> Result<HostNetworkResponse, ErrorPayload> {
        invoke(HostOperation::NetworkClient, &request).await
    }
}

impl ExtensionHttpClient {
    pub async fn dispatch_public(
        &self,
        request: ExtensionHttpRequest,
    ) -> Result<ExtensionHttpResponse, ErrorPayload> {
        invoke(HostOperation::ExtensionHttpPublic, &request).await
    }
}

async fn invoke<I, O>(operation: HostOperation, input: &I) -> Result<O, ErrorPayload>
where
    I: Serialize + ?Sized,
    O: DeserializeOwned,
{
    let output = call_host(operation.wire_name(), serialize_request(input)?).await?;
    deserialize_response(output, operation.wire_name())
}

async fn invoke_collected_stream<I, O>(
    operation: HostOperation,
    input: &I,
) -> Result<O, ErrorPayload>
where
    I: Serialize + ?Sized,
    O: DeserializeOwned,
{
    let output = call_host_stream(operation.wire_name(), serialize_request(input)?).await?;
    deserialize_response(output, operation.wire_name())
}

async fn invoke_unit<I>(operation: HostOperation, input: &I) -> Result<(), ErrorPayload>
where
    I: Serialize + ?Sized,
{
    invoke::<I, HostAcknowledgement>(operation, input)
        .await
        .map(|_| ())
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
        .ok_or_else(|| ErrorPayload::new("host_not_ready", "host peer not ready"))
}

/// Invokes a raw host capability for transport or request-context integration tests.
pub async fn invoke_host(capability: &str, input: Value) -> Result<Value, ErrorPayload> {
    call_host(capability, input).await
}

fn serialize_request<T: Serialize + ?Sized>(request: &T) -> Result<Value, ErrorPayload> {
    serde_json::to_value(request).map_err(|error| {
        ErrorPayload::new(
            "serialization_failed",
            format!("failed to serialize host request: {error}"),
        )
    })
}

fn deserialize_response<T: DeserializeOwned>(
    output: Value,
    capability: &str,
) -> Result<T, ErrorPayload> {
    serde_json::from_value(output).map_err(|error| {
        ErrorPayload::new(
            "invalid_host_response",
            format!("invalid {capability} response: {error}"),
        )
    })
}

#[cfg(test)]
mod host_tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::*;
    use crate::{s5r::ErrorPayload, session::SessionToolSelectionDto};

    struct MockHost {
        marker: &'static str,
    }

    #[test]
    fn peer_error_mapping_preserves_host_payload_and_transport_contracts() {
        let mut expected =
            ErrorPayload::new("provider_rate_limited", "provider rate limit reached")
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
        let value = serialize_request(&process).expect("serialize process request");
        assert_eq!(value["command"], "rustc");
        assert_eq!(value["args"], json!(["--version"]));
        assert_eq!(value["timeout_ms"], 1_000);

        let mut network = HostNetworkRequest::get("https://example.com");
        network.body = vec![0, 255];
        network.redirect_policy = HostNetworkRedirectPolicy::Manual;
        let value = serialize_request(&network).expect("serialize binary network request");
        assert_eq!(value["body"], "AP8=");
        assert_eq!(value["redirect_policy"], "manual");
        assert_eq!(value["max_bytes"], 10 * 1024 * 1024);

        let response = deserialize_response::<HostNetworkResponse>(
            json!({
                "final_url": "https://example.com/final",
                "status": 200,
                "headers": {},
                "body": "b2s="
            }),
            "network.client",
        )
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
        let value = serialize_request(&create).expect("serialize create session request");
        assert_eq!(value["name"], "reviewer");
        assert_eq!(value["tool_selection"]["mode"], "only");
        assert_eq!(value["tool_selection"]["names"], json!(["read", "grep"]));
        assert_eq!(value["ephemeral"], true);
        assert!(value.get("working_dir").is_none());

        let submit = HostSubmitTurnRequest::background("child-1", "review this");
        let value = serialize_request(&submit).expect("serialize submit turn request");
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

        let output = deserialize_response::<HostSubmitTurnOutput>(
            json!({
                "status": "backgrounded",
                "task_id": "turn-1",
                "session_id": "child-1"
            }),
            "session.control.submit_turn",
        )
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
                "astrcode.session.control.dispose" if self.marker == "invalid_ack" => {
                    Ok(json!({ "ok": true, "unexpected": true }))
                },
                "astrcode.session.control.dispose" => Ok(json!({ "ok": true })),
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
        })
        .await;

        let invalid_ack = with_host_api(
            Arc::new(MockHost {
                marker: "invalid_ack",
            }),
            async {
                HostClient::session_control()
                    .recycle(HostRecycleSessionRequest::new("child-1"))
                    .await
            },
        )
        .await
        .unwrap_err();
        assert_eq!(invalid_ack.code, "invalid_host_response");
    }
}
