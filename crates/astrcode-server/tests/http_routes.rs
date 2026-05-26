use std::{collections::BTreeMap, fs, sync::Arc, time::Duration};

use astrcode_context::{ContextSettings, context_assembler::LlmContextAssembler};
use astrcode_core::{
    config::{EffectiveConfig, ExtensionSettings, LlmSettings, OpenAiApiMode},
    event::{Event, EventPayload},
    llm::{LlmContent, LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
    storage::EventStore,
    tool::{ToolDefinition, ToolResult},
    types::{SessionId, new_message_id, new_session_id},
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_protocol::{
    events::ClientNotification,
    http::{
        CompactSessionResponse, ConversationSnapshotResponseDto, CreateSessionResponseDto,
        PromptSubmitResponse, SlashCommandListResponseDto,
    },
};
use astrcode_server::{bootstrap::ServerRuntime, http::router};
use astrcode_storage::in_memory::InMemoryEventStore;
use astrcode_support::event_fanout::EventFanout;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode},
};
use http_body_util::BodyExt;
use tokio::sync::mpsc;
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

struct SummaryLlm;

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

#[async_trait::async_trait]
impl LlmProvider for SummaryLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::ContentDelta {
            delta: r#"<summary>
1. Primary Request and Intent:
   Compacted conversation summary

2. Key Technical Concepts:
   - compact

3. Files and Code Sections:
   - (none)

4. Errors and fixes:
   - (none)

5. Problem Solving:
   compacted

6. All user messages:
   - (none)

7. Pending Tasks:
   - (none)

8. Current Work:
   compact command

9. Optional Next Step:
   - (none)
</summary>"#
                .into(),
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

#[tokio::test]
async fn http_routes_require_bearer_token() {
    let runtime = runtime(Arc::new(ImmediateLlm));
    let event_tx = Arc::new(EventFanout::new(1024));
    let (app, token) = router(Arc::clone(&runtime), event_tx).unwrap();

    let unauthorized = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let authorized = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/sessions")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(authorized.status(), StatusCode::OK);
}

#[tokio::test]
async fn concurrent_prompt_accepts_one_and_queues_one() {
    let runtime = runtime(Arc::new(PendingLlm));
    let event_tx = Arc::new(EventFanout::new(1024));
    let (app, token) = router(Arc::clone(&runtime), event_tx).unwrap();
    let session_id = create_session(app.clone(), &token).await;
    let prompt_uri = format!("/api/sessions/{session_id}/prompt");

    let first = post_json(app.clone(), &prompt_uri, r#"{"text":"first"}"#, &token);
    let second = post_json(app, &prompt_uri, r#"{"text":"second"}"#, &token);

    let (first, second) = tokio::join!(first, second);
    let statuses = [first.status(), second.status()];

    // input queuing: one Accepted, one Handled (queued for next turn)
    assert!(statuses.contains(&StatusCode::OK));
    assert!(statuses.iter().all(|&s| s == StatusCode::OK));

    let first_body = to_bytes(first.into_body(), 4096).await.unwrap();
    let second_body = to_bytes(second.into_body(), 4096).await.unwrap();
    let bodies = [first_body, second_body];

    let kinds: Vec<&str> = bodies
        .iter()
        .map(|b| {
            let s = String::from_utf8_lossy(b);
            if s.contains("\"accepted\"") {
                "accepted"
            } else if s.contains("\"handled\"") {
                "handled"
            } else {
                "other"
            }
        })
        .collect();

    assert!(
        kinds.contains(&"accepted"),
        "expected one Accepted: {kinds:?}"
    );
    assert!(
        kinds.contains(&"handled"),
        "expected one Handled (queued): {kinds:?}"
    );
}


#[tokio::test]
async fn create_snapshot_then_stream_receives_live_prompt_delta() {
    let runtime = runtime(Arc::new(ImmediateLlm));
    let event_tx = Arc::new(EventFanout::new(1024));
    let (app, token) = router(Arc::clone(&runtime), event_tx).unwrap();
    let session_id = create_session(app.clone(), &token).await;

    let snapshot = get_json::<ConversationSnapshotResponseDto>(
        app.clone(),
        &format!("/api/sessions/{session_id}/conversation"),
        &token,
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
                .header("authorization", format!("Bearer {token}"))
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
        &token,
    )
    .await;
    assert_eq!(accepted.status(), StatusCode::OK);

    let body = read_sse_until(stream_response.into_body(), "finalizeBlock").await;
    assert!(body.contains("conversation"));
    assert!(body.contains("hello"));
    assert!(body.contains("hello from http"));
    assert!(body.contains(r#""status":"complete""#));

    let (after_app, after_token) = router(runtime, Arc::new(EventFanout::new(1024))).unwrap();
    let after = get_json::<ConversationSnapshotResponseDto>(
        after_app,
        &format!("/api/sessions/{session_id}/conversation"),
        &after_token,
    )
    .await;
    assert_eq!(after.blocks.len(), 2);
}

#[tokio::test]
async fn prompt_stream_returns_control_to_idle_when_turn_finishes() {
    let runtime = runtime(Arc::new(ImmediateLlm));
    let event_tx = Arc::new(EventFanout::new(1024));
    let (app, token) = router(Arc::clone(&runtime), event_tx).unwrap();
    let session_id = create_session(app.clone(), &token).await;

    let stream_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .header("authorization", format!("Bearer {token}"))
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
        &token,
    )
    .await;
    assert_eq!(accepted.status(), StatusCode::OK);

    let body = read_sse_until(stream_response.into_body(), r#""phase":"idle""#).await;
    assert!(body.contains(r#""canSubmitPrompt":true"#));
    assert!(body.contains(r#""canRequestCompact":true"#));
}

#[tokio::test]
async fn stream_preserves_global_updates_during_replay_drain() {
    let runtime = runtime(Arc::new(ImmediateLlm));
    let event_tx = Arc::new(EventFanout::new(1024));
    let (app, token) = router(Arc::clone(&runtime), Arc::clone(&event_tx)).unwrap();
    let session_id = create_session(app.clone(), &token).await;
    let sid = SessionId::from(session_id.clone());

    runtime
        .event_store()
        .append_event(Event::new(
            sid,
            None,
            EventPayload::UserMessage {
                message_id: "missed-message".into(),
                text: "missed while reconnecting".into(),
            },
        ))
        .await
        .unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .header("authorization", format!("Bearer {token}"))
                .uri(format!("/api/sessions/{session_id}/stream?cursor=1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    event_tx.send(ClientNotification::ExtensionRegistryChanged);

    let body = read_sse_until(response.into_body(), "extensionRegistryChanged").await;
    assert!(body.contains("missed while reconnecting"));
    assert!(body.contains("extensionRegistryChanged"));
}

#[tokio::test]
async fn stream_replays_events_after_snapshot_cursor() {
    let runtime = runtime(Arc::new(ImmediateLlm));
    let event_tx = Arc::new(EventFanout::new(1024));
    let (app, token) = router(Arc::clone(&runtime), Arc::clone(&event_tx)).unwrap();
    let session_id = create_session(app.clone(), &token).await;
    let sid = SessionId::from(session_id.clone());

    runtime
        .event_store()
        .append_event(Event::new(
            sid.clone(),
            None,
            EventPayload::UserMessage {
                message_id: "snapshot-message".into(),
                text: "already in snapshot".into(),
            },
        ))
        .await
        .unwrap();

    let snapshot = get_json::<ConversationSnapshotResponseDto>(
        app.clone(),
        &format!("/api/sessions/{session_id}/conversation"),
        &token,
    )
    .await;
    assert_eq!(snapshot.blocks.len(), 1);

    runtime
        .event_store()
        .append_event(Event::new(
            sid,
            None,
            EventPayload::UserMessage {
                message_id: "missed-message".into(),
                text: "missed while connecting stream".into(),
            },
        ))
        .await
        .unwrap();
    runtime
        .event_store()
        .append_event(Event::new(
            SessionId::from(session_id.clone()),
            None,
            EventPayload::AssistantMessageCompleted {
                message_id: "missed-assistant".into(),
                text: "completed response after snapshot".into(),
                reasoning_content: None,
            },
        ))
        .await
        .unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .header("authorization", format!("Bearer {token}"))
                .uri(format!(
                    "/api/sessions/{session_id}/stream?cursor={}",
                    snapshot.cursor.value
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = read_sse_until(response.into_body(), "completed response after snapshot").await;
    assert!(body.contains("missed-message"));
    assert!(body.contains("completed response after snapshot"));
    assert!(!body.contains("already in snapshot"));
}

#[tokio::test]
async fn stream_invalid_cursor_requests_rehydrate() {
    let runtime = runtime(Arc::new(ImmediateLlm));
    let event_tx = Arc::new(EventFanout::new(1024));
    let (app, token) = router(Arc::clone(&runtime), event_tx).unwrap();
    let session_id = create_session(app.clone(), &token).await;

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .header("authorization", format!("Bearer {token}"))
                .uri(format!("/api/sessions/{session_id}/stream?cursor=invalid"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = read_sse_until(response.into_body(), "rehydrateRequired").await;
    assert!(body.contains("rehydrateRequired"));
}

#[tokio::test]
async fn stream_projects_child_events_with_parent_cursor_without_child_text() {
    let runtime = runtime(Arc::new(ImmediateLlm));
    let event_tx = Arc::new(EventFanout::new(1024));
    let (app, token) = router(Arc::clone(&runtime), Arc::clone(&event_tx)).unwrap();
    let parent_id = create_session(app.clone(), &token).await;
    let parent_sid = SessionId::from(parent_id.clone());
    let child_sid = new_session_id();
    runtime
        .event_store()
        .create_session(&child_sid, ".", "mock-model", Some(&parent_sid), None, None)
        .await
        .unwrap();
    runtime
        .event_store()
        .append_event(Event::new(
            parent_sid.clone(),
            None,
            EventPayload::AgentSessionSpawned {
                child_session_id: child_sid.clone(),
                agent_name: "explorer".into(),
                task: "inspect code".into(),
                tool_policy: None,
                tool_call_id: "agent-call".into(),
            },
        ))
        .await
        .unwrap();
    let parent_cursor = runtime
        .event_store()
        .latest_cursor(&parent_sid)
        .await
        .unwrap()
        .unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .header("authorization", format!("Bearer {token}"))
                .uri(format!("/api/sessions/{parent_id}/stream"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let mut child_event = Event::new(
        child_sid.clone(),
        None,
        EventPayload::AssistantTextDelta {
            message_id: "child-assistant".into(),
            delta: "secret child text".into(),
        },
    );
    child_event.seq = Some(99);
    event_tx.send(ClientNotification::Event(child_event));

    let body = read_sse_until(response.into_body(), "agentSessionUpdated").await;
    assert!(body.contains(r#""kind":"agentSessionUpdated""#));
    assert!(body.contains(&format!(r#""childSessionId":"{child_sid}""#)));
    assert!(body.contains(r#""phase":"streaming""#));
    assert!(body.contains(&format!(r#""value":"{parent_cursor}""#)));
    assert!(!body.contains(r#""value":"99""#));
    assert!(!body.contains("agentName"));
    assert!(!body.contains("currentTool"));
    assert!(!body.contains("secret child text"));
    assert!(!body.contains("patchBlock"));
}

#[tokio::test]
async fn command_list_route_exposes_backend_slash_commands() {
    let runtime = runtime(Arc::new(ImmediateLlm));
    let event_tx = Arc::new(EventFanout::new(1024));
    let (app, token) = router(Arc::clone(&runtime), event_tx).unwrap();
    let session_id = create_session(app.clone(), &token).await;

    let body = get_json::<SlashCommandListResponseDto>(
        app,
        &format!("/api/sessions/{session_id}/commands"),
        &token,
    )
    .await;

    let compact = body
        .commands
        .iter()
        .find(|command| command.name == "compact")
        .expect("compact command");
    assert_eq!(compact.source, "builtin");
    assert!(!compact.needs_argument);
}

#[tokio::test]
async fn prompt_route_compact_returns_handled_and_streams_continuation() {
    let runtime = runtime(Arc::new(SummaryLlm));
    let event_tx = Arc::new(EventFanout::new(1024));
    let (app, token) = router(Arc::clone(&runtime), event_tx).unwrap();
    let session_id = create_session(app.clone(), &token).await;
    let sid = SessionId::from(session_id.clone());

    for text in ["one", "two", "three"] {
        runtime
            .event_store()
            .append_event(Event::new(
                sid.clone(),
                None,
                EventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: text.into(),
                },
            ))
            .await
            .unwrap();
        runtime
            .event_store()
            .append_event(Event::new(
                sid.clone(),
                None,
                EventPayload::AssistantMessageCompleted {
                    message_id: new_message_id(),
                    text: format!("answer {text}"),
                    reasoning_content: None,
                },
            ))
            .await
            .unwrap();
    }

    let stream_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .header("authorization", format!("Bearer {token}"))
                .uri(format!("/api/sessions/{session_id}/stream"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let response = post_json(
        app.clone(),
        &format!("/api/sessions/{session_id}/prompt"),
        r#"{"text":"/compact"}"#,
        &token,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: PromptSubmitResponse = serde_json::from_slice(&body_bytes(response).await).unwrap();
    assert!(matches!(body, PromptSubmitResponse::Handled { .. }));

    let sse = read_sse_until(stream_response.into_body(), "sessionContinued").await;
    assert!(sse.contains("sessionContinued"));
    assert!(
        !runtime
            .event_store()
            .session_read_model(&sid)
            .await
            .unwrap()
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .any(|content| matches!(content, LlmContent::Text { text } if text == "/compact"))
    );
}

#[tokio::test]
async fn compact_route_returns_same_session_and_hydrates_post_compact_context() {
    let runtime = runtime(Arc::new(SummaryLlm));
    let event_tx = Arc::new(EventFanout::new(1024));
    let (app, token) = router(Arc::clone(&runtime), event_tx).unwrap();
    let session_id = create_session(app.clone(), &token).await;
    let sid = SessionId::from(session_id.clone());
    let read_fixture = "target/post-compact-read-fixture.txt";
    fs::create_dir_all("target").unwrap();
    fs::write(read_fixture, "pub fn compact_restore_fixture() {}").unwrap();

    runtime
        .event_store()
        .append_event(Event::new(
            sid.clone(),
            None,
            EventPayload::ToolCallRequested {
                call_id: "read-call-1".into(),
                tool_name: "read".into(),
                arguments: serde_json::json!({ "path": read_fixture }),
            },
        ))
        .await
        .unwrap();
    runtime
        .event_store()
        .append_event(Event::new(
            sid.clone(),
            None,
            EventPayload::ToolCallCompleted {
                call_id: "read-call-1".into(),
                tool_name: "read".into(),
                result: ToolResult {
                    call_id: "read-call-1".into(),
                    content: "pub fn compact_restore_fixture() {}".into(),
                    is_error: false,
                    error: None,
                    metadata: BTreeMap::new(),
                    duration_ms: None,
                },
                arguments: String::new(),
                arguments_json: None,
            },
        ))
        .await
        .unwrap();

    for text in ["one", "two", "three"] {
        runtime
            .event_store()
            .append_event(Event::new(
                sid.clone(),
                None,
                EventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: text.into(),
                },
            ))
            .await
            .unwrap();
        runtime
            .event_store()
            .append_event(Event::new(
                sid.clone(),
                None,
                EventPayload::AssistantMessageCompleted {
                    message_id: new_message_id(),
                    text: format!("answer {text}"),
                    reasoning_content: None,
                },
            ))
            .await
            .unwrap();
    }

    let stream_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .header("authorization", format!("Bearer {token}"))
                .uri(format!("/api/sessions/{session_id}/stream"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let response = post_json(
        app.clone(),
        &format!("/api/sessions/{session_id}/compact"),
        r#"{}"#,
        &token,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: CompactSessionResponse = serde_json::from_slice(&body_bytes(response).await).unwrap();
    let returned_session_id = body
        .new_session_id
        .expect("compact should return session_id");
    assert_eq!(returned_session_id, session_id, "same-session compact");
    let sse = read_sse_until(stream_response.into_body(), "sessionContinued").await;
    assert!(sse.contains(&session_id));

    let state = runtime
        .event_store()
        .session_read_model(&sid)
        .await
        .unwrap();
    assert!(!state.context_messages.is_empty());
    let restored_context = state
        .context_messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            astrcode_core::llm::LlmContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(restored_context.contains("<post_compact_context>"));
    assert!(restored_context.contains(read_fixture));
    assert!(restored_context.contains("compact_restore_fixture"));

    let snapshot = get_json::<ConversationSnapshotResponseDto>(
        app,
        &format!("/api/sessions/{session_id}/conversation"),
        &token,
    )
    .await;
    assert_eq!(snapshot.session_id, session_id);
    assert_eq!(snapshot.cursor.value, state.cursor());
    let _ = fs::remove_file(read_fixture);
}

async fn create_session(app: Router, token: &str) -> String {
    let response = post_json(app, "/api/sessions", r#"{"workingDir":"."}"#, token).await;
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice::<CreateSessionResponseDto>(&body_bytes(response).await)
        .unwrap()
        .session_id
}

async fn post_json(
    app: Router,
    uri: &str,
    body: &'static str,
    token: &str,
) -> axum::response::Response {
    app.oneshot(
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body))
            .unwrap(),
    )
    .await
    .unwrap()
}

async fn get_json<T: serde::de::DeserializeOwned>(app: Router, uri: &str, token: &str) -> T {
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .header("authorization", format!("Bearer {token}"))
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
    let effective = EffectiveConfig {
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
            supports_prompt_cache_key: false,
            prompt_cache_retention: None,
            reasoning: false,
            reasoning_split: false,
        },
        small_llm: LlmSettings {
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
            supports_prompt_cache_key: false,
            prompt_cache_retention: None,
            reasoning: false,
            reasoning_split: false,
        },
        context: ContextSettings {
            auto_compact_enabled: true,
            predictive_compact_enabled: false,
            compact_threshold_percent: 83.5,
            compact_max_retry_attempts: 3,
            compact_max_output_tokens: 20_000,
            compact_keep_recent_turns: None,
            predictive_compact_baseline_growth_tokens: 15_000,
            compact_circuit_breaker_threshold: 3,
            compact_circuit_breaker_cooldown_secs: 60,
            post_compact_max_files: 5,
            post_compact_token_budget: 50_000,
            post_compact_max_tokens_per_file: 5_000,
        },
        agent: astrcode_core::config::AgentSettings::default(),
        extensions: ExtensionSettings::default(),
    };
    let event_store = Arc::new(InMemoryEventStore::new()) as Arc<dyn EventStore>;
    let extension_runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
    let context_assembler = Arc::new(LlmContextAssembler::new(ContextSettings::default()));
    let capabilities = Arc::new(astrcode_session::SessionRuntimeServices::new(
        llm_provider.clone(),
        llm_provider,
        Arc::clone(&extension_runner),
        Arc::clone(&context_assembler),
        effective,
    ));
    let config = Arc::new(astrcode_server::bootstrap::ConfigManager::new(
        Arc::new(astrcode_storage::config_store::FileConfigStore::new(
            std::path::PathBuf::from("target/test-config.json"),
        )),
        astrcode_core::config::Config::default(),
        Arc::clone(&capabilities),
    ));
    let session_manager = Arc::new(astrcode_server::session_manager::SessionManager::new(
        Arc::clone(&event_store),
        Arc::clone(&config),
        Arc::clone(&capabilities),
        vec![],
    ));
    Arc::new(ServerRuntime::assemble_for_test(
        event_store,
        config,
        context_assembler,
        session_manager,
        extension_runner,
        capabilities,
        std::env::temp_dir(),
    ))
}
