use std::{sync::Arc, time::Duration};

use astrcode_context::{manager::LlmContextAssembler, settings::ContextWindowSettings};
use astrcode_core::{
    config::{EffectiveConfig, LlmSettings, OpenAiApiMode},
    event::{Event, EventPayload},
    llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
    tool::ToolDefinition,
};
use astrcode_extensions::{runner::ExtensionRunner, runtime::ExtensionRuntime};
use astrcode_protocol::{
    events::ClientNotification,
    http::{ConversationSnapshotResponseDto, CreateSessionResponseDto},
};
use astrcode_server::{bootstrap::ServerRuntime, http::router, session::SessionManager};
use astrcode_storage::in_memory::InMemoryEventStore;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode},
};
use http_body_util::BodyExt;
use tokio::sync::{broadcast, mpsc};
use tower::ServiceExt;

struct ImmediateLlm;

#[async_trait::async_trait]
impl LlmProvider for ImmediateLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::ContentDelta {
            delta: "hello from http".into(),
        });
        let _ = tx.send(LlmEvent::Done {
            finish_reason: "stop".into(),
        });
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 200_000,
            max_output_tokens: 1024,
        }
    }
}

struct PendingLlm;

#[async_trait::async_trait]
impl LlmProvider for PendingLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        std::future::pending().await
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 1024,
            max_output_tokens: 1024,
        }
    }
}

#[tokio::test]
async fn concurrent_prompt_accepts_one_and_conflicts_one() {
    let runtime = runtime(Arc::new(PendingLlm));
    let (event_tx, _) = broadcast::channel(64);
    let app = router(Arc::clone(&runtime), event_tx);
    let session_id = create_session(app.clone()).await;
    let prompt_uri = format!("/api/sessions/{session_id}/prompt");

    let first = post_json(app.clone(), &prompt_uri, r#"{"text":"first"}"#);
    let second = post_json(app, &prompt_uri, r#"{"text":"second"}"#);

    let (first, second) = tokio::join!(first, second);
    let statuses = [first.status(), second.status()];

    assert!(statuses.contains(&StatusCode::OK));
    assert!(statuses.contains(&StatusCode::CONFLICT));
}

#[tokio::test]
async fn sse_receiver_lag_emits_rehydrate_and_closes() {
    let runtime = runtime(Arc::new(ImmediateLlm));
    let start = runtime
        .session_manager
        .create(".", "mock-model", 2048, None)
        .await
        .unwrap();
    let session_id = start.session_id.clone();
    let (event_tx, _) = broadcast::channel(1);
    let app = router(Arc::clone(&runtime), event_tx.clone());

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/sessions/{session_id}/stream"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    for text in ["one", "two", "three"] {
        let event = runtime
            .session_manager
            .append_event(Event::new(
                session_id.clone(),
                None,
                EventPayload::UserMessage {
                    message_id: format!("message-{text}"),
                    text: text.into(),
                },
            ))
            .await
            .unwrap();
        let _ = event_tx.send(ClientNotification::Event(event));
    }

    let body = String::from_utf8(
        to_bytes(response.into_body(), 64 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();

    assert!(body.contains("rehydrateRequired"));
}

#[tokio::test]
async fn create_snapshot_then_stream_receives_live_prompt_delta() {
    let runtime = runtime(Arc::new(ImmediateLlm));
    let (event_tx, _) = broadcast::channel(64);
    let app = router(Arc::clone(&runtime), event_tx);
    let session_id = create_session(app.clone()).await;

    let snapshot = get_json::<ConversationSnapshotResponseDto>(
        app.clone(),
        &format!("/api/sessions/{session_id}/conversation"),
    )
    .await;
    assert_eq!(snapshot.session_id, session_id);
    assert_eq!(snapshot.cursor.value, "1");
    assert!(snapshot.blocks.is_empty());

    let stream_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/sessions/{session_id}/stream"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let accepted = post_json(
        app,
        &format!("/api/sessions/{session_id}/prompt"),
        r#"{"text":"hello"}"#,
    )
    .await;
    assert_eq!(accepted.status(), StatusCode::OK);

    let body = read_sse_until(stream_response.into_body(), "hello from http").await;
    assert!(body.contains("conversation"));
    assert!(body.contains("hello"));
    assert!(body.contains("hello from http"));

    let after = get_json::<ConversationSnapshotResponseDto>(
        router(runtime, broadcast::channel(64).0),
        &format!("/api/sessions/{session_id}/conversation"),
    )
    .await;
    assert_eq!(after.blocks.len(), 2);
}

async fn create_session(app: Router) -> String {
    let response = post_json(app, "/api/sessions", r#"{"workingDir":"."}"#).await;
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice::<CreateSessionResponseDto>(&body_bytes(response).await)
        .unwrap()
        .session_id
}

async fn post_json(app: Router, uri: &str, body: &'static str) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
    .unwrap()
}

async fn get_json<T: serde::de::DeserializeOwned>(app: Router, uri: &str) -> T {
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&body_bytes(response).await).unwrap()
}

async fn body_bytes(response: axum::response::Response) -> Vec<u8> {
    to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap()
        .to_vec()
}

async fn read_sse_until(mut body: Body, needle: &str) -> String {
    let deadline = tokio::time::sleep(Duration::from_secs(2));
    tokio::pin!(deadline);
    let mut collected = String::new();

    loop {
        tokio::select! {
            _ = &mut deadline => panic!("timed out waiting for SSE payload containing {needle}"),
            frame = body.frame() => {
                let frame = frame.expect("sse body should stay open").unwrap();
                let Some(chunk) = frame.data_ref() else {
                    continue;
                };
                collected.push_str(std::str::from_utf8(chunk).unwrap());
                if collected.contains(needle) {
                    return collected;
                }
            },
        }
    }
}

fn runtime(llm_provider: Arc<dyn LlmProvider>) -> Arc<ServerRuntime> {
    Arc::new(ServerRuntime {
        session_manager: Arc::new(SessionManager::new(Arc::new(InMemoryEventStore::new()))),
        llm_provider,
        context_assembler: Arc::new(LlmContextAssembler::new(ContextWindowSettings::default())),
        extension_runner: Arc::new(ExtensionRunner::new(
            Duration::from_secs(1),
            Arc::new(ExtensionRuntime::new()),
        )),
        effective: EffectiveConfig {
            llm: LlmSettings {
                provider_kind: "mock".into(),
                base_url: String::new(),
                api_key: String::new(),
                api_mode: OpenAiApiMode::ChatCompletions,
                model_id: "mock-model".into(),
                max_tokens: 1024,
                context_limit: 1024,
                connect_timeout_secs: 1,
                read_timeout_secs: 1,
                max_retries: 0,
                retry_base_delay_ms: 0,
                temperature: None,
                supports_prompt_cache_key: false,
                prompt_cache_retention: None,
            },
        },
    })
}
