mod error;
mod transport;

use std::sync::Arc;

pub use astrcode_protocol::http::{
    AgentLifecycleDto, AuthExchangeRequest, AuthExchangeResponse, CompactSessionRequest,
    CompactSessionResponse, CreateSessionRequest, CurrentModelInfoDto, ExecutionControlDto,
    ModeSummaryDto, ModelOptionDto, PhaseDto, PromptRequest, PromptSkillInvocation,
    PromptSubmitResponse, SaveActiveSelectionRequest, SessionListItem, SessionModeStateDto,
    SwitchModeRequest,
    conversation::v1::{
        ConversationAssistantBlockDto, ConversationBannerDto, ConversationBannerErrorCodeDto,
        ConversationBlockDto, ConversationBlockPatchDto, ConversationBlockStatusDto,
        ConversationChildSummaryDto, ConversationControlStateDto, ConversationCursorDto,
        ConversationDeltaDto, ConversationErrorEnvelopeDto, ConversationSlashActionKindDto,
        ConversationSlashCandidateDto, ConversationSlashCandidatesResponseDto,
        ConversationSnapshotResponseDto, ConversationStreamEnvelopeDto,
    },
};
pub use error::{ClientError, ClientErrorKind};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::sync::{RwLock, broadcast};
pub use transport::{
    ClientTransport, ReqwestTransport, SseEvent, TransportError, TransportMethod, TransportRequest,
    TransportResponse,
};

const DEFAULT_STREAM_BUFFER: usize = 128;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub origin: String,
    pub api_token: Option<String>,
    pub api_token_expires_at_ms: Option<i64>,
    pub stream_buffer: usize,
}

impl ClientConfig {
    pub fn new(origin: impl Into<String>) -> Self {
        Self {
            origin: origin.into(),
            api_token: None,
            api_token_expires_at_ms: None,
            stream_buffer: DEFAULT_STREAM_BUFFER,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationStreamItem {
    Delta(Box<ConversationStreamEnvelopeDto>),
    RehydrateRequired(ConversationErrorEnvelopeDto),
    Lagged { skipped: u64 },
    Disconnected { message: String },
}

pub struct ConversationStream {
    receiver: broadcast::Receiver<ConversationStreamItem>,
}

impl ConversationStream {
    pub async fn recv(&mut self) -> Result<Option<ConversationStreamItem>, ClientError> {
        match self.receiver.recv().await {
            Ok(item) => Ok(Some(item)),
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                Ok(Some(ConversationStreamItem::Lagged { skipped }))
            },
            Err(broadcast::error::RecvError::Closed) => Ok(None),
        }
    }
}

#[derive(Debug, Clone)]
struct AuthState {
    api_token: String,
    expires_at_ms: Option<i64>,
}

#[derive(Debug)]
pub struct AstrcodeClient<T = ReqwestTransport> {
    origin: String,
    transport: Arc<T>,
    auth_state: Arc<RwLock<Option<AuthState>>>,
    stream_buffer: usize,
}

impl AstrcodeClient<ReqwestTransport> {
    pub fn new(config: ClientConfig) -> Self {
        Self::with_transport(config, ReqwestTransport::new())
    }
}

impl<T> Clone for AstrcodeClient<T> {
    fn clone(&self) -> Self {
        Self {
            origin: self.origin.clone(),
            transport: Arc::clone(&self.transport),
            auth_state: Arc::clone(&self.auth_state),
            stream_buffer: self.stream_buffer,
        }
    }
}

impl<T> AstrcodeClient<T>
where
    T: ClientTransport + 'static,
{
    pub fn with_transport(config: ClientConfig, transport: T) -> Self {
        let auth_state = config.api_token.map(|api_token| AuthState {
            api_token,
            expires_at_ms: config.api_token_expires_at_ms,
        });

        Self {
            origin: config.origin.trim_end_matches('/').to_string(),
            transport: Arc::new(transport),
            auth_state: Arc::new(RwLock::new(auth_state)),
            stream_buffer: config.stream_buffer.max(1),
        }
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub async fn exchange_auth(
        &self,
        bootstrap_token: impl Into<String>,
    ) -> Result<AuthExchangeResponse, ClientError> {
        let response: AuthExchangeResponse = self
            .send_json(
                TransportMethod::Post,
                "/api/auth/exchange",
                Vec::new(),
                Some(AuthExchangeRequest {
                    token: bootstrap_token.into(),
                }),
                false,
            )
            .await?;

        let mut auth_state = self.auth_state.write().await;
        *auth_state = Some(AuthState {
            api_token: response.token.clone(),
            expires_at_ms: Some(response.expires_at_ms),
        });

        Ok(response)
    }

    pub async fn set_api_token(&self, token: impl Into<String>, expires_at_ms: Option<i64>) {
        let mut auth_state = self.auth_state.write().await;
        *auth_state = Some(AuthState {
            api_token: token.into(),
            expires_at_ms,
        });
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionListItem>, ClientError> {
        self.send_json::<Vec<SessionListItem>, Value>(
            TransportMethod::Get,
            "/api/sessions",
            Vec::new(),
            None,
            true,
        )
        .await
    }

    pub async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<SessionListItem, ClientError> {
        self.send_json(
            TransportMethod::Post,
            "/api/sessions",
            Vec::new(),
            Some(request),
            true,
        )
        .await
    }

    pub async fn list_modes(&self) -> Result<Vec<ModeSummaryDto>, ClientError> {
        self.send_json::<Vec<ModeSummaryDto>, Value>(
            TransportMethod::Get,
            "/api/modes",
            Vec::new(),
            None,
            true,
        )
        .await
    }

    pub async fn get_session_mode(
        &self,
        session_id: &str,
    ) -> Result<SessionModeStateDto, ClientError> {
        self.send_json::<SessionModeStateDto, Value>(
            TransportMethod::Get,
            &format!("/api/sessions/{session_id}/mode"),
            Vec::new(),
            None,
            true,
        )
        .await
    }

    pub async fn switch_mode(
        &self,
        session_id: &str,
        request: SwitchModeRequest,
    ) -> Result<SessionModeStateDto, ClientError> {
        self.send_json(
            TransportMethod::Post,
            &format!("/api/sessions/{session_id}/mode"),
            Vec::new(),
            Some(request),
            true,
        )
        .await
    }

    pub async fn submit_prompt(
        &self,
        session_id: &str,
        request: PromptRequest,
    ) -> Result<PromptSubmitResponse, ClientError> {
        self.send_json(
            TransportMethod::Post,
            &format!("/api/sessions/{session_id}/prompts"),
            Vec::new(),
            Some(request),
            true,
        )
        .await
    }

    pub async fn get_current_model(&self) -> Result<CurrentModelInfoDto, ClientError> {
        self.send_json::<CurrentModelInfoDto, Value>(
            TransportMethod::Get,
            "/api/models/current",
            Vec::new(),
            None,
            true,
        )
        .await
    }

    pub async fn list_models(&self) -> Result<Vec<ModelOptionDto>, ClientError> {
        self.send_json::<Vec<ModelOptionDto>, Value>(
            TransportMethod::Get,
            "/api/models",
            Vec::new(),
            None,
            true,
        )
        .await
    }

    pub async fn save_active_selection(
        &self,
        request: SaveActiveSelectionRequest,
    ) -> Result<(), ClientError> {
        let response = self
            .transport
            .execute(TransportRequest {
                method: TransportMethod::Post,
                url: self.url("/api/config/active-selection"),
                auth_token: Some(self.require_api_token().await?),
                query: Vec::new(),
                json_body: Some(serde_json::to_value(request).map_err(|error| {
                    ClientError::new(
                        ClientErrorKind::UnexpectedResponse,
                        format!("serialize request body failed: {error}"),
                    )
                })?),
            })
            .await
            .map_err(ClientError::from_transport)?;
        if response.status == 204 {
            Ok(())
        } else {
            Err(ClientError::new(
                ClientErrorKind::UnexpectedResponse,
                format!(
                    "save active selection expected 204 response, got {}",
                    response.status
                ),
            )
            .with_status(response.status))
        }
    }

    pub async fn request_compact(
        &self,
        session_id: &str,
        mut request: CompactSessionRequest,
    ) -> Result<CompactSessionResponse, ClientError> {
        if request
            .control
            .as_ref()
            .and_then(|control| control.manual_compact)
            .is_none()
        {
            let control = request.control.get_or_insert(ExecutionControlDto {
                manual_compact: None,
            });
            control.manual_compact = Some(true);
        }
        self.send_json(
            TransportMethod::Post,
            &format!("/api/sessions/{session_id}/compact"),
            Vec::new(),
            Some(request),
            true,
        )
        .await
    }

    pub async fn fetch_conversation_snapshot(
        &self,
        session_id: &str,
        focus: Option<&str>,
    ) -> Result<ConversationSnapshotResponseDto, ClientError> {
        let mut query = Vec::new();
        if let Some(focus) = focus.map(str::trim).filter(|focus| !focus.is_empty()) {
            query.push(("focus".to_string(), focus.to_string()));
        }
        self.send_json::<ConversationSnapshotResponseDto, Value>(
            TransportMethod::Get,
            &format!("/api/v1/conversation/sessions/{session_id}/snapshot"),
            query,
            None,
            true,
        )
        .await
    }

    pub async fn stream_conversation(
        &self,
        session_id: &str,
        cursor: Option<&ConversationCursorDto>,
        focus: Option<&str>,
    ) -> Result<ConversationStream, ClientError> {
        let mut query = cursor
            .map(|cursor| vec![("cursor".to_string(), cursor.0.clone())])
            .unwrap_or_default();
        if let Some(focus) = focus.map(str::trim).filter(|focus| !focus.is_empty()) {
            query.push(("focus".to_string(), focus.to_string()));
        }
        let receiver = self
            .transport
            .open_sse(
                TransportRequest {
                    method: TransportMethod::Get,
                    url: self.url(&format!(
                        "/api/v1/conversation/sessions/{session_id}/stream"
                    )),
                    auth_token: Some(self.require_api_token().await?),
                    query,
                    json_body: None,
                },
                self.stream_buffer,
            )
            .await
            .map_err(ClientError::from_transport)?;

        let (sender, output) = broadcast::channel(self.stream_buffer);
        tokio::spawn(async move {
            let mut receiver = receiver;
            while let Some(event) = receiver.recv().await {
                if sender.receiver_count() == 0 {
                    break;
                }
                match event {
                    Ok(event) => {
                        match serde_json::from_str::<ConversationStreamEnvelopeDto>(&event.data) {
                            Ok(delta) => match delta.delta.clone() {
                                ConversationDeltaDto::RehydrateRequired { error } => {
                                    if sender
                                        .send(ConversationStreamItem::RehydrateRequired(error))
                                        .is_err()
                                    {
                                        break;
                                    }
                                },
                                _ => {
                                    if sender
                                        .send(ConversationStreamItem::Delta(Box::new(delta)))
                                        .is_err()
                                    {
                                        break;
                                    }
                                },
                            },
                            Err(error) => {
                                let _ = sender.send(ConversationStreamItem::Disconnected {
                                    message: format!(
                                        "failed to decode conversation sse payload: {error}"
                                    ),
                                });
                                break;
                            },
                        }
                    },
                    Err(TransportError::StreamDisconnected { message }) => {
                        let _ = sender.send(ConversationStreamItem::Disconnected { message });
                        break;
                    },
                    Err(error) => {
                        let _ = sender.send(ConversationStreamItem::Disconnected {
                            message: ClientError::from_transport(error).message,
                        });
                        break;
                    },
                }
            }
        });

        Ok(ConversationStream { receiver: output })
    }

    pub async fn list_conversation_slash_candidates(
        &self,
        session_id: &str,
        query: Option<&str>,
    ) -> Result<ConversationSlashCandidatesResponseDto, ClientError> {
        let mut query_params = Vec::new();
        if let Some(query) = query.map(str::trim).filter(|query| !query.is_empty()) {
            query_params.push(("q".to_string(), query.to_string()));
        }
        self.send_json::<ConversationSlashCandidatesResponseDto, Value>(
            TransportMethod::Get,
            &format!("/api/v1/conversation/sessions/{session_id}/slash-candidates"),
            query_params,
            None,
            true,
        )
        .await
    }

    async fn send_json<Response, Body>(
        &self,
        method: TransportMethod,
        path: &str,
        query: Vec<(String, String)>,
        body: Option<Body>,
        requires_auth: bool,
    ) -> Result<Response, ClientError>
    where
        Response: DeserializeOwned,
        Body: Serialize,
    {
        let auth_token = if requires_auth {
            Some(self.require_api_token().await?)
        } else {
            None
        };
        let json_body = body
            .map(serde_json::to_value)
            .transpose()
            .map_err(|error| {
                ClientError::new(
                    ClientErrorKind::UnexpectedResponse,
                    format!("serialize request body failed: {error}"),
                )
            })?;

        let response = self
            .transport
            .execute(TransportRequest {
                method,
                url: self.url(path),
                auth_token,
                query,
                json_body,
            })
            .await
            .map_err(ClientError::from_transport)?;

        serde_json::from_str::<Response>(&response.body).map_err(|error| {
            ClientError::new(
                ClientErrorKind::UnexpectedResponse,
                format!("decode response body failed: {error}"),
            )
            .with_status(response.status)
        })
    }

    async fn require_api_token(&self) -> Result<String, ClientError> {
        let auth_state = self.auth_state.read().await;
        let Some(auth_state) = auth_state.as_ref() else {
            return Err(ClientError::new(
                ClientErrorKind::AuthExpired,
                "API session token is missing; call exchange_auth first",
            ));
        };

        if auth_state
            .expires_at_ms
            .is_some_and(|expires_at_ms| current_timestamp_ms() >= expires_at_ms)
        {
            return Err(ClientError::new(
                ClientErrorKind::AuthExpired,
                "API session token has expired; call exchange_auth again",
            ));
        }

        Ok(auth_state.api_token.clone())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.origin, path)
    }
}

fn current_timestamp_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{Arc, Mutex},
    };

    use astrcode_protocol::http::{
        CompactSessionRequest, CompactSessionResponse, ConversationSlashActionKindDto,
        ConversationSlashCandidateDto, ConversationSlashCandidatesResponseDto, CurrentModelInfoDto,
        ExecutionControlDto, PhaseDto, SaveActiveSelectionRequest,
    };
    use async_trait::async_trait;
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::{
        AstrcodeClient, ClientConfig, ClientErrorKind, ConversationStreamItem,
        transport::{
            ClientTransport, SseEvent, TransportError, TransportEventReceiver, TransportMethod,
            TransportRequest, TransportResponse,
        },
    };

    #[derive(Debug)]
    enum MockCall {
        Request {
            expected: TransportRequest,
            result: Result<TransportResponse, TransportError>,
        },
        Stream {
            expected: TransportRequest,
            events: Vec<Result<SseEvent, TransportError>>,
        },
    }

    #[derive(Debug, Default, Clone)]
    struct MockTransport {
        calls: Arc<Mutex<VecDeque<MockCall>>>,
    }

    impl MockTransport {
        fn push(&self, call: MockCall) {
            self.calls
                .lock()
                .expect("mock lock poisoned")
                .push_back(call);
        }
    }

    #[async_trait]
    impl ClientTransport for MockTransport {
        async fn execute(
            &self,
            request: TransportRequest,
        ) -> Result<TransportResponse, TransportError> {
            let Some(MockCall::Request { expected, result }) =
                self.calls.lock().expect("mock lock poisoned").pop_front()
            else {
                panic!("expected request call");
            };
            assert_eq!(request, expected);
            result
        }

        async fn open_sse(
            &self,
            request: TransportRequest,
            buffer: usize,
        ) -> Result<TransportEventReceiver, TransportError> {
            let Some(MockCall::Stream { expected, events }) =
                self.calls.lock().expect("mock lock poisoned").pop_front()
            else {
                panic!("expected stream call");
            };
            assert_eq!(request, expected);
            let (sender, receiver) = mpsc::channel(buffer.max(1));
            tokio::spawn(async move {
                for event in events {
                    let _ = sender.send(event).await;
                }
            });
            Ok(receiver)
        }
    }

    #[tokio::test]
    async fn exchange_auth_caches_api_token_and_reuses_it() {
        let transport = MockTransport::default();
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Post,
                url: "http://localhost:5529/api/auth/exchange".to_string(),
                auth_token: None,
                query: Vec::new(),
                json_body: Some(json!({ "token": "bootstrap-token" })),
            },
            result: Ok(TransportResponse {
                status: 200,
                body: json!({
                    "ok": true,
                    "token": "session-token",
                    "expiresAtMs": super::current_timestamp_ms() + 30_000
                })
                .to_string(),
            }),
        });
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Get,
                url: "http://localhost:5529/api/sessions".to_string(),
                auth_token: Some("session-token".to_string()),
                query: Vec::new(),
                json_body: None,
            },
            result: Ok(TransportResponse {
                status: 200,
                body: "[]".to_string(),
            }),
        });

        let client =
            AstrcodeClient::with_transport(ClientConfig::new("http://localhost:5529"), transport);
        let exchange = client
            .exchange_auth("bootstrap-token")
            .await
            .expect("exchange should succeed");
        assert_eq!(exchange.token, "session-token");
        assert!(
            client
                .list_sessions()
                .await
                .expect("list should succeed")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn fetch_conversation_snapshot_uses_cached_auth_and_decodes_payload() {
        let transport = MockTransport::default();
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Get,
                url: "http://localhost:5529/api/v1/conversation/sessions/session-1/snapshot"
                    .to_string(),
                auth_token: Some("session-token".to_string()),
                query: Vec::new(),
                json_body: None,
            },
            result: Ok(TransportResponse {
                status: 200,
                body: json!({
                    "sessionId": "session-1",
                    "sessionTitle": "Session 1",
                    "cursor": "cursor:1",
                    "phase": "idle",
                    "control": {
                        "phase": "idle",
                        "canSubmitPrompt": true,
                        "canRequestCompact": true,
                        "compactPending": false,
                        "compacting": false,
                        "currentModeId": "default"
                    }
                })
                .to_string(),
            }),
        });

        let config = ClientConfig {
            origin: "http://localhost:5529".to_string(),
            api_token: Some("session-token".to_string()),
            api_token_expires_at_ms: Some(super::current_timestamp_ms() + 30_000),
            stream_buffer: 8,
        };
        let client = AstrcodeClient::with_transport(config, transport);
        let snapshot = client
            .fetch_conversation_snapshot("session-1", None)
            .await
            .expect("snapshot should decode");

        assert_eq!(snapshot.session_id, "session-1");
        assert_eq!(snapshot.phase, PhaseDto::Idle);
        assert!(snapshot.blocks.is_empty());
    }

    #[tokio::test]
    async fn stream_conversation_surfaces_delta_rehydrate_and_disconnect_events() {
        let transport = MockTransport::default();
        transport.push(MockCall::Stream {
            expected: TransportRequest {
                method: TransportMethod::Get,
                url: "http://localhost:5529/api/v1/conversation/sessions/session-1/stream"
                    .to_string(),
                auth_token: Some("session-token".to_string()),
                query: vec![("cursor".to_string(), "cursor:40".to_string())],
                json_body: None,
            },
            events: vec![
                Ok(SseEvent {
                    id: Some("41".to_string()),
                    event: Some("message".to_string()),
                    data: json!({
                        "sessionId": "session-1",
                        "cursor": "cursor:41",
                        "kind": "append_block",
                        "block": {
                            "kind": "assistant",
                            "id": "assistant-1",
                            "status": "streaming",
                            "markdown": "hello"
                        }
                    })
                    .to_string(),
                }),
                Ok(SseEvent {
                    id: Some("42".to_string()),
                    event: Some("message".to_string()),
                    data: json!({
                        "sessionId": "session-1",
                        "cursor": "cursor:42",
                        "kind": "rehydrate_required",
                        "error": {
                            "code": "cursor_expired",
                            "message": "cursor expired",
                            "rehydrateRequired": true
                        }
                    })
                    .to_string(),
                }),
                Err(TransportError::StreamDisconnected {
                    message: "socket closed".to_string(),
                }),
            ],
        });

        let config = ClientConfig {
            origin: "http://localhost:5529".to_string(),
            api_token: Some("session-token".to_string()),
            api_token_expires_at_ms: Some(super::current_timestamp_ms() + 30_000),
            stream_buffer: 8,
        };
        let client = AstrcodeClient::with_transport(config, transport);
        let mut stream = client
            .stream_conversation(
                "session-1",
                Some(
                    &astrcode_protocol::http::conversation::v1::ConversationCursorDto(
                        "cursor:40".to_string(),
                    ),
                ),
                None,
            )
            .await
            .expect("stream should open");

        let first = stream.recv().await.expect("stream read should succeed");
        assert!(matches!(first, Some(ConversationStreamItem::Delta(_))));

        let second = stream.recv().await.expect("stream read should succeed");
        assert!(matches!(
            second,
            Some(ConversationStreamItem::RehydrateRequired(_))
        ));

        let third = stream.recv().await.expect("stream read should succeed");
        assert_eq!(
            third,
            Some(ConversationStreamItem::Disconnected {
                message: "socket closed".to_string()
            })
        );
    }

    #[tokio::test]
    async fn list_conversation_slash_candidates_uses_conversation_surface_contract() {
        let transport = MockTransport::default();
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Get,
                url:
                    "http://localhost:5529/api/v1/conversation/sessions/session-1/slash-candidates"
                        .to_string(),
                auth_token: Some("session-token".to_string()),
                query: vec![("q".to_string(), "skill".to_string())],
                json_body: None,
            },
            result: Ok(TransportResponse {
                status: 200,
                body: json!({
                    "items": [{
                        "id": "review",
                        "title": "Review skill",
                        "description": "插入 review skill",
                        "keywords": ["skill", "review"],
                        "actionKind": "insert_text",
                        "actionValue": "/review"
                    }]
                })
                .to_string(),
            }),
        });

        let config = ClientConfig {
            origin: "http://localhost:5529".to_string(),
            api_token: Some("session-token".to_string()),
            api_token_expires_at_ms: Some(super::current_timestamp_ms() + 30_000),
            stream_buffer: 8,
        };
        let client = AstrcodeClient::with_transport(config, transport);
        let candidates = client
            .list_conversation_slash_candidates("session-1", Some("skill"))
            .await
            .expect("slash candidates should load");

        assert_eq!(
            candidates,
            ConversationSlashCandidatesResponseDto {
                items: vec![ConversationSlashCandidateDto {
                    id: "review".to_string(),
                    title: "Review skill".to_string(),
                    description: "插入 review skill".to_string(),
                    keywords: vec!["skill".to_string(), "review".to_string()],
                    action_kind: ConversationSlashActionKindDto::InsertText,
                    action_value: "/review".to_string(),
                }]
            }
        );
    }

    #[tokio::test]
    async fn normalizes_cursor_expired_and_auth_failures() {
        let transport = MockTransport::default();
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Get,
                url: "http://localhost:5529/api/v1/conversation/sessions/session-1/snapshot"
                    .to_string(),
                auth_token: Some("session-token".to_string()),
                query: Vec::new(),
                json_body: None,
            },
            result: Err(TransportError::Http {
                status: 409,
                body: json!({
                    "code": "cursor_expired",
                    "message": "cursor expired",
                    "details": { "cursor": "cursor:12" }
                })
                .to_string(),
            }),
        });
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Post,
                url: "http://localhost:5529/api/sessions/session-1/compact".to_string(),
                auth_token: Some("expired-token".to_string()),
                query: Vec::new(),
                json_body: Some(json!({
                    "control": {
                        "manualCompact": true
                    }
                })),
            },
            result: Err(TransportError::Http {
                status: 401,
                body: json!({ "error": "unauthorized" }).to_string(),
            }),
        });

        let client = AstrcodeClient::with_transport(
            ClientConfig {
                origin: "http://localhost:5529".to_string(),
                api_token: Some("session-token".to_string()),
                api_token_expires_at_ms: Some(super::current_timestamp_ms() + 30_000),
                stream_buffer: 8,
            },
            transport.clone(),
        );

        let snapshot_error = client
            .fetch_conversation_snapshot("session-1", None)
            .await
            .expect_err("snapshot should fail");
        assert_eq!(snapshot_error.kind, ClientErrorKind::CursorExpired);
        assert_eq!(snapshot_error.status_code, Some(409));

        client
            .set_api_token(
                "expired-token",
                Some(super::current_timestamp_ms() + 30_000),
            )
            .await;
        let compact_error = client
            .request_compact(
                "session-1",
                CompactSessionRequest {
                    control: None,
                    instructions: None,
                },
            )
            .await
            .expect_err("compact should fail");
        assert_eq!(compact_error.kind, ClientErrorKind::AuthExpired);
    }

    #[tokio::test]
    async fn missing_or_expired_tokens_fail_before_transport() {
        let transport = MockTransport::default();
        let client = AstrcodeClient::with_transport(
            ClientConfig::new("http://localhost:5529"),
            transport.clone(),
        );
        let missing = client
            .list_sessions()
            .await
            .expect_err("missing token should fail");
        assert_eq!(missing.kind, ClientErrorKind::AuthExpired);

        let expired_client = AstrcodeClient::with_transport(
            ClientConfig {
                origin: "http://localhost:5529".to_string(),
                api_token: Some("expired".to_string()),
                api_token_expires_at_ms: Some(super::current_timestamp_ms() - 1),
                stream_buffer: 8,
            },
            transport,
        );
        let expired = expired_client
            .list_sessions()
            .await
            .expect_err("expired token should fail");
        assert_eq!(expired.kind, ClientErrorKind::AuthExpired);
    }

    #[tokio::test]
    async fn request_compact_preserves_existing_compact_contract() {
        let transport = MockTransport::default();
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Post,
                url: "http://localhost:5529/api/sessions/session-1/compact".to_string(),
                auth_token: Some("session-token".to_string()),
                query: Vec::new(),
                json_body: Some(json!({
                    "control": {
                        "manualCompact": true
                    }
                })),
            },
            result: Ok(TransportResponse {
                status: 202,
                body: json!({
                    "accepted": true,
                    "deferred": false,
                    "message": "手动 compact 已执行。"
                })
                .to_string(),
            }),
        });

        let client = AstrcodeClient::with_transport(
            ClientConfig {
                origin: "http://localhost:5529".to_string(),
                api_token: Some("session-token".to_string()),
                api_token_expires_at_ms: Some(super::current_timestamp_ms() + 30_000),
                stream_buffer: 8,
            },
            transport,
        );
        let response = client
            .request_compact(
                "session-1",
                CompactSessionRequest {
                    control: None,
                    instructions: None,
                },
            )
            .await
            .expect("compact should succeed");
        assert_eq!(
            response,
            CompactSessionResponse {
                accepted: true,
                deferred: false,
                message: "手动 compact 已执行。".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn request_compact_normalizes_missing_manual_compact_to_explicit_control() {
        let transport = MockTransport::default();
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Post,
                url: "http://localhost:5529/api/sessions/session-1/compact".to_string(),
                auth_token: Some("session-token".to_string()),
                query: Vec::new(),
                json_body: Some(json!({
                    "control": {
                        "manualCompact": true
                    },
                    "instructions": "保留错误和文件路径"
                })),
            },
            result: Ok(TransportResponse {
                status: 202,
                body: json!({
                    "accepted": true,
                    "deferred": true,
                    "message": "手动 compact 已登记，会在当前 turn 完成后执行。"
                })
                .to_string(),
            }),
        });

        let client = AstrcodeClient::with_transport(
            ClientConfig {
                origin: "http://localhost:5529".to_string(),
                api_token: Some("session-token".to_string()),
                api_token_expires_at_ms: Some(super::current_timestamp_ms() + 30_000),
                stream_buffer: 8,
            },
            transport,
        );
        let response = client
            .request_compact(
                "session-1",
                CompactSessionRequest {
                    control: Some(ExecutionControlDto {
                        manual_compact: None,
                    }),
                    instructions: Some("保留错误和文件路径".to_string()),
                },
            )
            .await
            .expect("compact should succeed");
        assert!(response.deferred);
    }

    #[tokio::test]
    async fn save_active_selection_accepts_no_content_response() {
        let transport = MockTransport::default();
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Post,
                url: "http://localhost:5529/api/config/active-selection".to_string(),
                auth_token: Some("session-token".to_string()),
                query: Vec::new(),
                json_body: Some(json!({
                    "activeProfile": "openai",
                    "activeModel": "gpt-4.1"
                })),
            },
            result: Ok(TransportResponse {
                status: 204,
                body: String::new(),
            }),
        });

        let client = AstrcodeClient::with_transport(
            ClientConfig {
                origin: "http://localhost:5529".to_string(),
                api_token: Some("session-token".to_string()),
                api_token_expires_at_ms: Some(super::current_timestamp_ms() + 30_000),
                stream_buffer: 8,
            },
            transport,
        );

        client
            .save_active_selection(SaveActiveSelectionRequest {
                active_profile: "openai".to_string(),
                active_model: "gpt-4.1".to_string(),
            })
            .await
            .expect("save active selection should succeed");
    }

    #[tokio::test]
    async fn get_current_model_decodes_model_summary() {
        let transport = MockTransport::default();
        transport.push(MockCall::Request {
            expected: TransportRequest {
                method: TransportMethod::Get,
                url: "http://localhost:5529/api/models/current".to_string(),
                auth_token: Some("session-token".to_string()),
                query: Vec::new(),
                json_body: None,
            },
            result: Ok(TransportResponse {
                status: 200,
                body: json!({
                    "profileName": "openai",
                    "model": "gpt-4.1",
                    "providerKind": "openai"
                })
                .to_string(),
            }),
        });

        let client = AstrcodeClient::with_transport(
            ClientConfig {
                origin: "http://localhost:5529".to_string(),
                api_token: Some("session-token".to_string()),
                api_token_expires_at_ms: Some(super::current_timestamp_ms() + 30_000),
                stream_buffer: 8,
            },
            transport,
        );

        assert_eq!(
            client
                .get_current_model()
                .await
                .expect("current model should decode"),
            CurrentModelInfoDto {
                profile_name: "openai".to_string(),
                model: "gpt-4.1".to_string(),
                provider_kind: "openai".to_string(),
            }
        );
    }
}
