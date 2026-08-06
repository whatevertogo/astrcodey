use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use astrcode_context::{ContextSettings, context_assembler::LlmContextAssembler};
use astrcode_core::{
    config::{
        EffectiveConfig, ExtensionSettings, LlmSettings, ProviderAuthScheme, ProviderWireFormat,
    },
    event::{
        DurableEvent, DurableEventPayload, ExtensionEventData, LiveEvent, LiveEventPayload,
        StoredEvent,
    },
    llm::{LlmContent, LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
    tool::{SessionToolSelection, ToolDefinition, ToolResult, ToolResultArtifactSlice},
    types::{Cursor, SessionId, new_message_id},
};
use astrcode_extension_sdk::{
    builder::manifest,
    extension::{
        ExtensionCapability, ExtensionError, ExtensionHttpHandler, ExtensionHttpMethod,
        ExtensionHttpResponse, ExtensionHttpRoute, ExtensionManifest, HttpContext,
        MAX_EXTENSION_HTTP_BODY_BYTES, Registrar,
    },
};
use astrcode_extensions::{Extension, runner::ExtensionRunner};
use astrcode_protocol::{
    events::ClientNotification,
    http::{
        ApplyProviderPresetResponseDto, CommandCompletionResponse, CommandInvokeResponse,
        CompactSessionResponse, ConfigureSessionToolsResponse, ConversationBlockDto,
        ConversationErrorEnvelopeDto, ConversationSnapshotResponseDto, CreateSessionResponseDto,
        PromptSubmitResponse, ProviderCatalogResponseDto, SlashCommandListResponseDto,
        ToolSelectionDto,
    },
    wire::{CommandSourceDto, ProviderAuthSchemeDto, ProviderWireFormatDto},
};
use astrcode_server::{
    bootstrap::{ServerApp, ServerRuntime},
    http::{router as app_router, router_with_event_publisher},
    test_support::{
        ChildSessionCoordinator, ConfigManager, MAX_PROMPT_TEXT_BYTES, SessionManager,
        TurnRegistry, TurnScheduler,
    },
};
use astrcode_session_projection::{SessionReadModel, SessionSummary};
use astrcode_storage::{
    EventReader, SessionEventJournal, SessionPathResolver, SessionReader, SessionStore,
    StorageError, ToolResultArtifactInput, ToolResultArtifactRef, ToolResultArtifactStore,
    in_memory::InMemoryEventStore,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use tokio::sync::mpsc;
use tower::ServiceExt;

fn router(
    runtime: Arc<ServerRuntime>,
) -> Result<(Router, String), astrcode_server::http::HttpServerError> {
    app_router(ServerApp::new(runtime))
}

struct ImmediateLlm;

struct HttpRoutesExtension;

struct HttpRoutesHandler;

#[async_trait::async_trait]
impl ExtensionHttpHandler for HttpRoutesHandler {
    async fn handle(&self, ctx: HttpContext) -> Result<ExtensionHttpResponse, ExtensionError> {
        let request = ctx.request();
        Ok(ExtensionHttpResponse::json(
            201,
            serde_json::json!({
                "pathParams": request.path_params,
                "query": request.query,
                "body": request.body,
            }),
        ))
    }
}

#[async_trait::async_trait]
impl Extension for HttpRoutesExtension {
    fn manifest(&self) -> ExtensionManifest {
        manifest("http-routes-test")
            .version("test")
            .description("Server HTTP route test extension")
            .capability(ExtensionCapability::PublicHttp)
            .capability(ExtensionCapability::AuthenticatedHttp)
            .build()
    }

    fn register(&self, registrar: &mut Registrar) {
        registrar.http_route(
            ExtensionHttpRoute::public(ExtensionHttpMethod::Post, "/plugin-probe/{id}"),
            Arc::new(HttpRoutesHandler),
        );
        registrar.http_route(
            ExtensionHttpRoute::authenticated(ExtensionHttpMethod::Post, "/protected-probe/{id}"),
            Arc::new(HttpRoutesHandler),
        );
    }
}

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
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();

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
async fn cors_allows_supported_tauri_origins_only() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, _token) = router(runtime).unwrap();

    for origin in [
        "tauri://localhost",
        "http://tauri.localhost",
        "https://tauri.localhost",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/api/sessions/session-1/stream")
                    .header(header::ORIGIN, origin)
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                    .header(
                        header::ACCESS_CONTROL_REQUEST_HEADERS,
                        "authorization,cache-control",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&origin.parse().unwrap())
        );
    }

    let untrusted = app
        .oneshot(
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/api/sessions/session-1/stream")
                .header(header::ORIGIN, "https://example.com")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                .header(
                    header::ACCESS_CONTROL_REQUEST_HEADERS,
                    "authorization,cache-control",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(untrusted.status(), StatusCode::OK);
    assert!(
        untrusted
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none()
    );
}

#[tokio::test]
async fn session_tools_are_applied_at_creation_reconfigured_and_validated() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let created = post_json_owned(
        app.clone(),
        "/api/sessions",
        r#"{"workingDir":".","toolSelection":{"mode":"all","except":[" shell ","shell"]}}"#.into(),
        &token,
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    let session_id = serde_json::from_slice::<CreateSessionResponseDto>(&body_bytes(created).await)
        .unwrap()
        .session_id;
    let initial_model = runtime
        .event_store()
        .session_read_model(&SessionId::new(&session_id))
        .await
        .unwrap();
    assert_eq!(
        initial_model.identity.tool_selection,
        SessionToolSelection::All {
            except: vec!["shell".into()]
        }
    );

    let uri = format!("/api/sessions/{session_id}/tools");

    let configured = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(&uri)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    r#"{"selection":{"mode":"only","names":["write"," read ","write"]}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let configured_status = configured.status();
    let configured_body = body_bytes(configured).await;
    assert_eq!(
        configured_status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&configured_body)
    );
    let configured =
        serde_json::from_slice::<ConfigureSessionToolsResponse>(&configured_body).unwrap();
    assert_eq!(
        configured.selection,
        ToolSelectionDto::Only {
            names: vec!["read".into(), "write".into()]
        }
    );

    let read_model = runtime
        .event_store()
        .session_read_model(&SessionId::new(&session_id))
        .await
        .unwrap();
    assert_eq!(
        read_model.identity.tool_selection,
        SessionToolSelection::Only {
            names: vec!["read".into(), "write".into()]
        }
    );

    let invalid = app
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(uri)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(r#"{"selection":{"mode":"all","except":[" "]}}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn extension_http_routes_allow_only_declared_public_routes() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    runtime
        .extension_runner()
        .register(Arc::new(HttpRoutesExtension))
        .await
        .unwrap();
    let (app, token) = router(Arc::clone(&runtime)).unwrap();

    let public = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/plugin-probe/7?mode=public")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"source":"public"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(public.status(), StatusCode::CREATED);

    let protected_path = "/api/extensions/http-routes-test/protected-probe/8";
    let unauthorized = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(protected_path)
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let authorized = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(protected_path)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"source":"authenticated"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(authorized.status(), StatusCode::CREATED);

    let invalid_json = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/plugin-probe/7")
                .body(Body::from("{"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(invalid_json.status(), StatusCode::BAD_REQUEST);

    let oversized = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/plugin-probe/7")
                .body(Body::from(vec![0; MAX_EXTENSION_HTTP_BODY_BYTES + 1]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let unknown = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/not-a-plugin-route")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn provider_catalog_route_returns_endpoint_presets() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();

    let catalog =
        get_json::<ProviderCatalogResponseDto>(app, "/api/config/provider-catalog", &token).await;

    let qwen = catalog
        .providers
        .iter()
        .find(|provider| provider.id == "qwen")
        .expect("qwen preset exists");
    assert_eq!(qwen.provider_kind, "qwen");
    assert_eq!(
        qwen.wire_format,
        ProviderWireFormatDto::OpenAiChatCompletions
    );
    assert!(
        qwen.endpoints
            .iter()
            .any(|endpoint| endpoint.base_url.as_deref()
                == Some("https://dashscope.aliyuncs.com/compatible-mode/v1"))
    );
    assert!(!qwen.capabilities.strict_tool_use);

    for provider_id in ["openai", "anthropic"] {
        let provider = catalog
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .expect("strict provider preset exists");
        assert!(provider.capabilities.strict_tool_use);
    }

    let ark = catalog
        .providers
        .iter()
        .find(|provider| provider.id == "ark")
        .expect("ark preset exists");
    assert_eq!(ark.auth_scheme, ProviderAuthSchemeDto::Bearer);
    assert!(
        ark.endpoints
            .iter()
            .any(|endpoint| endpoint.base_url.as_deref()
                == Some("https://ark.cn-beijing.volces.com/api/v3"))
    );
}

#[tokio::test]
async fn active_selection_rejects_unknown_approval_mode_with_structured_error() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(runtime).unwrap();

    let response = post_json(
        app,
        "/api/config/active-selection",
        r#"{"activeProfile":"test","activeModel":"test","approvalMode":"future"}"#,
        &token,
    )
    .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: ConversationErrorEnvelopeDto =
        serde_json::from_slice(&body_bytes(response).await).unwrap();
    assert_eq!(error.code, "invalid_approval_mode");
}

#[tokio::test]
async fn provider_preset_apply_persists_profile_from_catalog() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let body = serde_json::json!({
        "providerId": "qwen",
        "endpointId": "dashscope-compatible",
        "profileName": "qwen-test",
        "activate": false
    })
    .to_string();

    let response = post_json_owned(
        app.clone(),
        "/api/config/provider-preset/apply",
        body,
        &token,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let applied: ApplyProviderPresetResponseDto =
        serde_json::from_slice(&body_bytes(response).await).unwrap();
    assert_eq!(applied.profile_name, "qwen-test");
    assert_eq!(applied.model_id, "qwen3-coder-plus");
    assert!(!applied.activated);

    let config = runtime.config_manager().raw_config_snapshot();
    let profile = config
        .profiles
        .iter()
        .find(|profile| profile.name == "qwen-test")
        .expect("qwen profile was persisted");
    assert_eq!(profile.provider_kind, "qwen");
    assert_eq!(
        profile.wire_format,
        ProviderWireFormat::OpenAiChatCompletions
    );
    assert_eq!(
        profile.base_url,
        "https://dashscope.aliyuncs.com/compatible-mode/v1"
    );
    assert_eq!(profile.api_key.as_deref(), Some("env:DASHSCOPE_API_KEY"));
}

#[tokio::test]
async fn concurrent_config_updates_preserve_both_profiles() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let first = post_json_owned(
        app.clone(),
        "/api/config/provider-preset/apply",
        serde_json::json!({
            "providerId": "qwen",
            "endpointId": "dashscope-compatible",
            "profileName": "concurrent-qwen",
            "activate": false
        })
        .to_string(),
        &token,
    );
    let second = post_json_owned(
        app,
        "/api/config/provider-preset/apply",
        serde_json::json!({
            "providerId": "openai-compatible",
            "baseUrl": "https://concurrent.example.test/v1",
            "profileName": "concurrent-compatible",
            "modelId": "test-model",
            "activate": false
        })
        .to_string(),
        &token,
    );

    let (first, second) = tokio::join!(first, second);
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);

    let config = runtime.config_manager().raw_config_snapshot();
    for expected in ["concurrent-qwen", "concurrent-compatible"] {
        assert!(
            config
                .profiles
                .iter()
                .any(|profile| profile.name == expected),
            "missing profile {expected}"
        );
    }
}

#[tokio::test]
async fn provider_preset_apply_uses_submitted_api_key() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let body = serde_json::json!({
        "providerId": "openai-compatible",
        "baseUrl": "https://api.example.test/v1",
        "apiKey": "test-key-from-settings",
        "modelId": "custom-model",
        "activate": true
    })
    .to_string();

    let response = post_json_owned(
        app.clone(),
        "/api/config/provider-preset/apply",
        body,
        &token,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let applied: ApplyProviderPresetResponseDto =
        serde_json::from_slice(&body_bytes(response).await).unwrap();
    assert_eq!(applied.profile_name, "openai-compatible");
    assert_eq!(applied.model_id, "custom-model");
    assert!(applied.activated);

    let config = runtime.config_manager().raw_config_snapshot();
    assert_eq!(config.active_profile, "openai-compatible");
    assert_eq!(config.active_model, "custom-model");
    let profile = config
        .profiles
        .iter()
        .find(|profile| profile.name == "openai-compatible")
        .expect("custom provider profile was persisted");
    assert_eq!(profile.base_url, "https://api.example.test/v1");
    assert_eq!(profile.api_key.as_deref(), Some("test-key-from-settings"));

    let body = serde_json::json!({
        "providerId": "openai-compatible",
        "baseUrl": "https://api.changed.example.test/v1",
        "modelId": "custom-model",
        "activate": true
    })
    .to_string();
    let response = post_json_owned(
        app.clone(),
        "/api/config/provider-preset/apply",
        body,
        &token,
    )
    .await;

    assert_eq!(response.status(), StatusCode::OK);
    let config = runtime.config_manager().raw_config_snapshot();
    let profile = config
        .profiles
        .iter()
        .find(|profile| profile.name == "openai-compatible")
        .expect("custom provider profile was persisted");
    assert_eq!(profile.base_url, "https://api.changed.example.test/v1");
    assert_eq!(profile.api_key.as_deref(), Some("test-key-from-settings"));

    let body = serde_json::json!({
        "profileName": "openai-compatible"
    })
    .to_string();
    let response = post_json_owned(app, "/api/config/provider-preset/remove", body, &token).await;

    assert_eq!(response.status(), StatusCode::OK);
    let config = runtime.config_manager().raw_config_snapshot();
    assert!(config.active_profile.is_empty());
    assert!(config.active_model.is_empty());
    assert!(
        config
            .profiles
            .iter()
            .all(|profile| profile.name != "openai-compatible")
    );
}

#[tokio::test]
async fn model_options_rejects_unknown_profile() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let body = serde_json::json!({
        "profileName": "nonexistent",
        "modelId": "test",
        "thinking": { "enabled": true, "effort": "high" }
    })
    .to_string();

    let response = post_json_owned(app, "/api/config/model-options", body, &token).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = body_bytes(response).await;
    let err: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(err["code"], "unknown_profile");
}

#[tokio::test]
async fn model_options_rejects_unknown_model() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();

    // First create a profile via provider preset
    let body = serde_json::json!({
        "providerId": "qwen",
        "endpointId": "dashscope-compatible",
        "profileName": "test-profile",
        "activate": false
    })
    .to_string();
    let response = post_json_owned(
        app.clone(),
        "/api/config/provider-preset/apply",
        body,
        &token,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    // Now try updating a non-existent model
    let body = serde_json::json!({
        "profileName": "test-profile",
        "modelId": "no-such-model",
        "thinking": { "enabled": true }
    })
    .to_string();
    let response = post_json_owned(app, "/api/config/model-options", body, &token).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = body_bytes(response).await;
    let err: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(err["code"], "unknown_model");
}

#[tokio::test]
async fn model_options_rejects_thinking_without_capability() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();

    // openai-compatible has no built-in thinking capability
    let body = serde_json::json!({
        "providerId": "openai-compatible",
        "profileName": "nocap-profile",
        "baseUrl": "https://api.example.com/v1",
        "activate": false
    })
    .to_string();
    let response = post_json_owned(
        app.clone(),
        "/api/config/provider-preset/apply",
        body,
        &token,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    // Try enabling thinking (should fail since no capability exists)
    let body = serde_json::json!({
        "profileName": "nocap-profile",
        "modelId": "gpt-4.1",
        "thinking": { "enabled": true, "effort": "high" }
    })
    .to_string();
    let response = post_json_owned(app, "/api/config/model-options", body, &token).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = body_bytes(response).await;
    let err: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(err["code"], "no_thinking_capability");
}

#[tokio::test]
async fn model_options_persists_thinking() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();

    // deepseek has built-in thinking capability (OpenAiChat, toggle-only)
    let body = serde_json::json!({
        "providerId": "deepseek",
        "endpointId": "official",
        "profileName": "thinking-test",
        "activate": false
    })
    .to_string();
    let response = post_json_owned(
        app.clone(),
        "/api/config/provider-preset/apply",
        body,
        &token,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let deepseek_default_model = "deepseek-v4-flash";

    // Persist thinking config (toggle-only, no effort needed)
    let body = serde_json::json!({
        "profileName": "thinking-test",
        "modelId": deepseek_default_model,
        "thinking": { "enabled": true }
    })
    .to_string();
    let response = post_json_owned(app.clone(), "/api/config/model-options", body, &token).await;
    assert_eq!(response.status(), StatusCode::OK);
    let resp: serde_json::Value = serde_json::from_slice(&body_bytes(response).await).unwrap();
    assert_eq!(resp["success"], true);

    // Verify persisted config
    let config = runtime.config_manager().raw_config_snapshot();
    let profile = config
        .profiles
        .iter()
        .find(|p| p.name == "thinking-test")
        .unwrap();
    let model = profile
        .models
        .iter()
        .find(|m| m.id == deepseek_default_model)
        .unwrap();
    let opts = model
        .model_options
        .as_ref()
        .expect("model_options should exist");
    assert_eq!(opts.thinking.as_ref().map(|t| t.enabled), Some(true));
}

#[tokio::test]
async fn model_options_can_disable_thinking() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();

    // Use deepseek which has a built-in thinking capability
    let body = serde_json::json!({
        "providerId": "deepseek",
        "endpointId": "official",
        "profileName": "disable-test",
        "activate": false
    })
    .to_string();
    let response = post_json_owned(
        app.clone(),
        "/api/config/provider-preset/apply",
        body,
        &token,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let deepseek_default_model = "deepseek-v4-flash";

    // Set enabled first
    let body = serde_json::json!({
        "profileName": "disable-test",
        "modelId": deepseek_default_model,
        "thinking": { "enabled": true }
    })
    .to_string();
    let response = post_json_owned(app.clone(), "/api/config/model-options", body, &token).await;
    assert_eq!(response.status(), StatusCode::OK);

    // Now disable by sending enabled:false
    let body = serde_json::json!({
        "profileName": "disable-test",
        "modelId": deepseek_default_model,
        "thinking": { "enabled": false }
    })
    .to_string();
    let response = post_json_owned(app.clone(), "/api/config/model-options", body, &token).await;
    assert_eq!(response.status(), StatusCode::OK);

    // Verify disabled
    let config = runtime.config_manager().raw_config_snapshot();
    let model = config
        .profiles
        .iter()
        .find(|p| p.name == "disable-test")
        .unwrap()
        .models
        .iter()
        .find(|m| m.id == deepseek_default_model)
        .unwrap();
    let opts = model
        .model_options
        .as_ref()
        .expect("model_options should exist");
    assert_eq!(
        opts.thinking.as_ref().map(|thinking| thinking.enabled),
        Some(false)
    );

    let body = serde_json::json!({
        "providerId": "openai",
        "endpointId": "official",
        "profileName": "cannot-disable-test",
        "modelId": "o3-mini",
        "activate": false
    })
    .to_string();
    let response = post_json_owned(
        app.clone(),
        "/api/config/provider-preset/apply",
        body,
        &token,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let body = serde_json::json!({
        "profileName": "cannot-disable-test",
        "modelId": "o3-mini",
        "thinking": { "enabled": false }
    })
    .to_string();
    let response = post_json_owned(app, "/api/config/model-options", body, &token).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = body_bytes(response).await;
    let error: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(error["code"], "invalid_thinking_config");
}

#[tokio::test]
async fn model_options_null_thinking_restores_model_default() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();

    let body = serde_json::json!({
        "providerId": "deepseek",
        "endpointId": "official",
        "profileName": "null-test",
        "activate": false
    })
    .to_string();
    let response = post_json_owned(
        app.clone(),
        "/api/config/provider-preset/apply",
        body,
        &token,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let deepseek_default_model = "deepseek-v4-flash";

    // Send request with thinking: null (explicit null)
    let body = serde_json::json!({
        "profileName": "null-test",
        "modelId": deepseek_default_model,
        "thinking": null
    })
    .to_string();
    let response = post_json_owned(app, "/api/config/model-options", body, &token).await;
    assert_eq!(response.status(), StatusCode::OK);

    let config = runtime.config_manager().raw_config_snapshot();
    let model = config
        .profiles
        .iter()
        .find(|p| p.name == "null-test")
        .unwrap()
        .models
        .iter()
        .find(|m| m.id == deepseek_default_model)
        .unwrap();
    assert!(model.model_options.is_none());
}

#[tokio::test]
async fn get_config_exposes_thinking_and_capability() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();

    // deepseek has built-in thinking capability (OpenAiChat mapping)
    let body = serde_json::json!({
        "providerId": "deepseek",
        "endpointId": "official",
        "profileName": "config-test",
        "activate": false
    })
    .to_string();
    let response = post_json_owned(
        app.clone(),
        "/api/config/provider-preset/apply",
        body,
        &token,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let deepseek_default_model = "deepseek-v4-flash";

    // Set thinking
    let body = serde_json::json!({
        "profileName": "config-test",
        "modelId": deepseek_default_model,
        "thinking": { "enabled": true }
    })
    .to_string();
    let _ = post_json_owned(app.clone(), "/api/config/model-options", body, &token).await;

    // GET /api/config should show thinking and capability
    let config_resp = get_json::<serde_json::Value>(app, "/api/config", &token).await;
    let profiles = config_resp["profiles"].as_array().unwrap();
    let test_profile = profiles
        .iter()
        .find(|p| p["name"] == "config-test")
        .unwrap();
    let model = &test_profile["models"][0];
    assert_eq!(model["id"], deepseek_default_model);

    // model_options should include thinking
    let opts = &model["modelOptions"];
    assert_eq!(opts["thinking"]["enabled"], true);

    // top-level thinking should mirror model_options.thinking
    assert_eq!(model["thinking"]["enabled"], true);

    // thinking_capability should be present (deepseek has built-in capability)
    let cap = &model["thinkingCapability"];
    assert_eq!(cap["allowedEffort"], serde_json::json!([]));
    assert_eq!(cap["canDisable"], true);
    assert!(
        cap.get("wireMapping").is_none(),
        "provider wire encoding must stay out of the UI contract"
    );
}

#[tokio::test]
async fn concurrent_prompt_accepts_one_and_queues_one() {
    let runtime = runtime(Arc::new(PendingLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
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
async fn prompt_route_accepts_valid_attachments_and_rejects_oversized_text() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let attachment_session_id = create_session(app.clone(), &token).await;
    let attachment_prompt_uri = format!("/api/sessions/{attachment_session_id}/prompt");
    let valid_attachment_body = serde_json::json!({
        "text": "",
        "attachments": [{
            "filename": "large.txt",
            "content": "x".repeat(MAX_EXTENSION_HTTP_BODY_BYTES + 1),
            "mediaType": "text/plain"
        }]
    })
    .to_string();

    let accepted = post_json_owned(
        app.clone(),
        &attachment_prompt_uri,
        valid_attachment_body,
        &token,
    )
    .await;
    assert_eq!(accepted.status(), StatusCode::OK);

    let oversized_session_id = create_session(app.clone(), &token).await;
    let oversized_prompt_uri = format!("/api/sessions/{oversized_session_id}/prompt");
    let body = serde_json::json!({
        "text": "x".repeat(MAX_PROMPT_TEXT_BYTES + 1)
    })
    .to_string();

    let response = post_json_owned(app, &oversized_prompt_uri, body, &token).await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn inject_route_writes_mid_turn_user_message() {
    let runtime = runtime(Arc::new(PendingLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let session_id = create_session(app.clone(), &token).await;

    let prompt_uri = format!("/api/sessions/{session_id}/prompt");
    let inject_uri = format!("/api/sessions/{session_id}/inject");
    let _first = post_json(app.clone(), &prompt_uri, r#"{"text":"first"}"#, &token).await;

    let inject = post_json(app, &inject_uri, r#"{"text":"steer me"}"#, &token).await;
    assert_eq!(inject.status(), StatusCode::OK);
    let body: PromptSubmitResponse = serde_json::from_slice(&body_bytes(inject).await).unwrap();
    assert!(matches!(
        body,
        PromptSubmitResponse::Handled { message, .. }
            if message == "injected into active turn"
    ));
}

#[tokio::test]
async fn inject_route_without_active_turn_returns_client_error() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let session_id = create_session(app.clone(), &token).await;
    let inject_uri = format!("/api/sessions/{session_id}/inject");

    let response = post_json(app, &inject_uri, r#"{"text":"too early"}"#, &token).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_snapshot_then_stream_receives_live_prompt_delta() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let session_id = create_session(app.clone(), &token).await;

    let snapshot = get_json::<ConversationSnapshotResponseDto>(
        app.clone(),
        &format!("/api/sessions/{session_id}/conversation"),
        &token,
    )
    .await;
    assert_eq!(snapshot.session_id, session_id);
    assert_eq!(snapshot.cursor.value, "0");
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

    let mut stream_body = stream_response.into_body();
    let connected = tokio::time::timeout(Duration::from_secs(1), stream_body.frame())
        .await
        .expect("SSE connection comment should be immediate")
        .expect("SSE body should stay open")
        .unwrap();
    assert_eq!(
        connected.data_ref().map(|data| data.as_ref()),
        Some(&b": connected\n\n"[..])
    );

    let accepted = post_json(
        app,
        &format!("/api/sessions/{session_id}/prompt"),
        r#"{"text":"hello"}"#,
        &token,
    )
    .await;
    assert_eq!(accepted.status(), StatusCode::OK);

    let body = read_sse_until(stream_body, "finalizeBlock").await;
    assert!(body.contains("conversation"));
    assert!(body.contains("hello"));
    assert!(body.contains("hello from http"));
    assert!(body.contains(r#""status":"complete""#));

    let (after_app, after_token) = router(runtime).unwrap();
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
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
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
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let session_id = create_session(app.clone(), &token).await;
    let sid = SessionId::from(session_id.clone());

    runtime
        .event_store()
        .append_event(DurableEvent::new(
            sid,
            None,
            DurableEventPayload::UserMessage {
                message_id: "missed-message".into(),
                text: "missed while reconnecting".into(),
                attachments: vec![],
                accepted_seq: None,
            },
        ))
        .await
        .unwrap();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .header("authorization", format!("Bearer {token}"))
                .uri(format!("/api/sessions/{session_id}/stream?cursor=0"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let reload = post_json(app, "/api/extensions/reload", "{}", &token).await;
    assert_eq!(reload.status(), StatusCode::OK);

    let body = read_sse_until(response.into_body(), "extensionRegistryChanged").await;
    assert!(body.contains("missed while reconnecting"));
    assert!(body.contains("extensionRegistryChanged"));
}

#[tokio::test]
async fn stream_preserves_ask_user_events_during_replay_drain() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token, events) =
        router_with_event_publisher(ServerApp::new(Arc::clone(&runtime))).unwrap();
    let session_id = create_session(app.clone(), &token).await;
    let sid = SessionId::from(session_id.clone());

    runtime
        .event_store()
        .append_event(DurableEvent::new(
            sid,
            None,
            DurableEventPayload::UserMessage {
                message_id: "missed-message".into(),
                text: "missed while reconnecting".into(),
                attachments: vec![],
                accepted_seq: None,
            },
        ))
        .await
        .unwrap();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .header("authorization", format!("Bearer {token}"))
                .uri(format!("/api/sessions/{session_id}/stream?cursor=0"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // 其他会话的 ask-user 问题在 replay 期间降临：跨会话广播经全局通知通道送达，
    // drain 必须保留而不是丢弃。
    events.send_notification(ClientNotification::Event(
        LiveEvent::new(
            SessionId::from("other-session"),
            None,
            LiveEventPayload::ExtensionEvent(ExtensionEventData {
                extension_id: "astrcode-ask-user".into(),
                event_type: "ask_user.pending".into(),
                schema_version: 1,
                payload: serde_json::json!({ "callId": "call-1" }),
            }),
        )
        .into(),
    ));

    let body = read_sse_until(response.into_body(), "ask_user.pending").await;
    assert!(body.contains("missed while reconnecting"));
    assert!(body.contains("ask_user.pending"));
    assert!(body.contains("call-1"));
}

#[tokio::test]
async fn stream_suppresses_current_session_ask_user_global_copy() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token, events) =
        router_with_event_publisher(ServerApp::new(Arc::clone(&runtime))).unwrap();
    let session_id = create_session(app.clone(), &token).await;

    let response = app
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
    let mut body = response.into_body();
    tokio::time::timeout(Duration::from_secs(1), body.frame())
        .await
        .expect("SSE connection comment should be immediate")
        .expect("SSE body should stay open")
        .unwrap();

    events.send_notification(ClientNotification::Event(
        LiveEvent::new(
            SessionId::from(session_id),
            None,
            LiveEventPayload::ExtensionEvent(ExtensionEventData {
                extension_id: "astrcode-ask-user".into(),
                event_type: "ask_user.pending".into(),
                schema_version: 1,
                payload: serde_json::json!({ "callId": "current-session-call" }),
            }),
        )
        .into(),
    ));

    let first = tokio::time::timeout(Duration::from_secs(1), body.frame())
        .await
        .expect("current-session event should be delivered")
        .expect("SSE body should stay open")
        .unwrap();
    let first = std::str::from_utf8(first.data_ref().unwrap()).unwrap();
    assert_eq!(first.matches("current-session-call").count(), 1);

    let duplicate = tokio::time::timeout(Duration::from_millis(250), body.frame()).await;
    assert!(duplicate.is_err(), "global copy should not be delivered");
}

#[tokio::test]
async fn stream_replays_events_after_snapshot_cursor() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let session_id = create_session(app.clone(), &token).await;
    let sid = SessionId::from(session_id.clone());

    runtime
        .event_store()
        .append_event(DurableEvent::new(
            sid.clone(),
            None,
            DurableEventPayload::UserMessage {
                message_id: "snapshot-message".into(),
                text: "already in snapshot".into(),
                attachments: vec![],
                accepted_seq: None,
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
        .append_event(DurableEvent::new(
            sid,
            None,
            DurableEventPayload::UserMessage {
                message_id: "missed-message".into(),
                text: "missed while connecting stream".into(),
                attachments: vec![],
                accepted_seq: None,
            },
        ))
        .await
        .unwrap();
    runtime
        .event_store()
        .append_event(DurableEvent::new(
            SessionId::from(session_id.clone()),
            None,
            DurableEventPayload::AssistantMessageCompleted {
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
async fn snapshot_and_replay_preserve_durable_errors() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let session_id = create_session(app.clone(), &token).await;
    let sid = SessionId::from(session_id.clone());

    runtime
        .event_store()
        .append_event(DurableEvent::new(
            sid.clone(),
            None,
            DurableEventPayload::UserMessage {
                message_id: "before-failure".into(),
                text: "before failure".into(),
                attachments: vec![],
                accepted_seq: None,
            },
        ))
        .await
        .unwrap();
    let before = get_json::<ConversationSnapshotResponseDto>(
        app.clone(),
        &format!("/api/sessions/{session_id}/conversation"),
        &token,
    )
    .await;

    runtime
        .event_store()
        .append_event(DurableEvent::new(
            sid.clone(),
            None,
            DurableEventPayload::ErrorOccurred {
                code: -32603,
                message: "provider rejected the selected model".into(),
                recoverable: false,
            },
        ))
        .await
        .unwrap();
    runtime
        .event_store()
        .append_event(DurableEvent::new(
            sid.clone(),
            None,
            DurableEventPayload::UserMessage {
                message_id: "after-failure".into(),
                text: "retry after failure".into(),
                attachments: vec![],
                accepted_seq: None,
            },
        ))
        .await
        .unwrap();
    runtime
        .event_store()
        .append_event(DurableEvent::new(
            sid,
            None,
            DurableEventPayload::TurnCompleted {
                finish_reason: "error".into(),
            },
        ))
        .await
        .unwrap();

    let replay = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .header("authorization", format!("Bearer {token}"))
                .uri(format!(
                    "/api/sessions/{session_id}/stream?cursor={}",
                    before.cursor.value
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let replay_body = read_sse_until(replay.into_body(), "retry after failure").await;
    assert!(replay_body.contains(r#""kind":"error""#));
    assert!(
        replay_body.find("provider rejected").unwrap()
            < replay_body.find("retry after failure").unwrap()
    );

    let latest = get_json::<ConversationSnapshotResponseDto>(
        app,
        &format!("/api/sessions/{session_id}/conversation"),
        &token,
    )
    .await;
    assert!(
        latest.cursor.value.parse::<u64>().unwrap() > before.cursor.value.parse::<u64>().unwrap()
    );
    assert!(matches!(
        latest.blocks.as_slice(),
        [
            ConversationBlockDto::User { text: before, .. },
            ConversationBlockDto::Error { message, .. },
            ConversationBlockDto::User { text: after, .. }
        ] if before == "before failure"
            && message == "provider rejected the selected model"
            && after == "retry after failure"
    ));
}

#[tokio::test]
async fn stream_invalid_cursors_request_rehydrate_and_close() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let session_id = create_session(app.clone(), &token).await;

    for cursor in ["invalid", "999999"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .header("authorization", format!("Bearer {token}"))
                    .uri(format!("/api/sessions/{session_id}/stream?cursor={cursor}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = tokio::time::timeout(
            Duration::from_secs(1),
            to_bytes(response.into_body(), 64 * 1024),
        )
        .await
        .expect("rehydrate stream should close")
        .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("rehydrateRequired"));
    }
}

#[tokio::test]
async fn stream_replay_over_limit_requests_rehydrate_and_closes() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let session_id = create_session(app.clone(), &token).await;
    let sid = SessionId::from(session_id.clone());

    for _ in 0..=1_000 {
        runtime
            .event_store()
            .append_event(DurableEvent::new(
                sid.clone(),
                None,
                DurableEventPayload::TurnStarted,
            ))
            .await
            .unwrap();
    }

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .header("authorization", format!("Bearer {token}"))
                .uri(format!("/api/sessions/{session_id}/stream?cursor=0"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = tokio::time::timeout(
        Duration::from_secs(1),
        to_bytes(response.into_body(), 64 * 1024),
    )
    .await
    .expect("over-limit replay stream should close")
    .unwrap();
    let body = std::str::from_utf8(&body).unwrap();
    assert!(body.contains("rehydrateRequired"));
}

#[tokio::test]
async fn stream_ignores_events_from_other_sessions() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let session_a = create_session(app.clone(), &token).await;
    let session_b = create_session(app.clone(), &token).await;

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .header("authorization", format!("Bearer {token}"))
                .uri(format!("/api/sessions/{session_a}/stream"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let session_b_prompt = post_json(
        app.clone(),
        &format!("/api/sessions/{session_b}/prompt"),
        r#"{"text":"from session b"}"#,
        &token,
    )
    .await;
    assert_eq!(session_b_prompt.status(), StatusCode::OK);

    let session_a_prompt = post_json(
        app,
        &format!("/api/sessions/{session_a}/prompt"),
        r#"{"text":"from session a"}"#,
        &token,
    )
    .await;
    assert_eq!(session_a_prompt.status(), StatusCode::OK);

    let body = read_sse_until(response.into_body(), "from session a").await;
    assert!(body.contains("from session a"));
    assert!(!body.contains("from session b"));
}

#[tokio::test]
async fn stream_projects_tracked_child_events_to_parent_stream() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token, events) =
        router_with_event_publisher(ServerApp::new(Arc::clone(&runtime))).unwrap();
    let session_id = create_session(app.clone(), &token).await;
    let parent_sid = SessionId::from(session_id.clone());
    let child_sid = SessionId::from(format!("{session_id}-child"));
    let child_id = child_sid.to_string();

    runtime
        .event_store()
        .append_event(DurableEvent::new(
            parent_sid,
            None,
            DurableEventPayload::AgentSessionSpawned {
                child_session_id: child_sid.clone(),
                agent_name: "worker".into(),
                task: "check fanout routing".into(),
                tool_selection: None,
                tool_call_id: "child-call".into(),
            },
        ))
        .await
        .unwrap();

    let response = app
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

    events.send_notification(ClientNotification::Event(
        LiveEvent::new(
            child_sid.clone(),
            None,
            LiveEventPayload::AssistantMessageStarted {
                message_id: "child-message".into(),
            },
        )
        .into(),
    ));
    events.send_notification(ClientNotification::Event(
        LiveEvent::new(
            child_sid,
            None,
            LiveEventPayload::AssistantTextDelta {
                message_id: "child-message".into(),
                delta: "child live text must not leak".into(),
            },
        )
        .into(),
    ));

    let body = read_sse_until(response.into_body(), "agentSessionUpdated").await;
    assert!(body.contains("agentSessionUpdated"));
    assert!(body.contains(&child_id));
    assert!(!body.contains("child live text must not leak"));
}

#[tokio::test]
async fn command_list_route_exposes_backend_slash_commands() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
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
    assert_eq!(compact.source, CommandSourceDto::Builtin);
    assert!(!compact.needs_argument);
    assert!(compact.requires_idle);
    assert!(!compact.argument_completions);

    let mode_cmd = body
        .commands
        .iter()
        .find(|command| command.name == "mode")
        .expect("mode extension command");
    assert_eq!(mode_cmd.source, CommandSourceDto::Extension);

    let shift_tab = body
        .keybindings
        .iter()
        .find(|kb| kb.command == "mode")
        .expect("shift+tab mode keybinding");
    assert_eq!(shift_tab.key, "shift+tab");
}

#[tokio::test]
async fn invoke_command_route_toggles_mode() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let session_id = create_session(app.clone(), &token).await;

    let http_response = post_json(
        app,
        &format!("/api/sessions/{session_id}/commands/mode"),
        r#"{"arguments":""}"#,
        &token,
    )
    .await;
    assert_eq!(http_response.status(), StatusCode::OK);
    let response: CommandInvokeResponse =
        serde_json::from_slice(&body_bytes(http_response).await).unwrap();

    match response {
        CommandInvokeResponse::Display { content, .. } => {
            assert!(content.contains("plan") || content.contains("Switched"));
        },
        other => panic!("expected display mode toggle, got {other:?}"),
    }
}

#[tokio::test]
async fn command_completion_route_returns_empty_for_commands_without_completion() {
    let runtime = runtime(Arc::new(ImmediateLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let session_id = create_session(app.clone(), &token).await;

    let http_response = post_json(
        app,
        &format!("/api/sessions/{session_id}/commands/mode/complete"),
        r#"{"argument":"","cursor":0}"#,
        &token,
    )
    .await;
    assert_eq!(http_response.status(), StatusCode::OK);
    let response: CommandCompletionResponse =
        serde_json::from_slice(&body_bytes(http_response).await).unwrap();

    assert!(response.items.is_empty());
    assert!(!response.truncated);
}

#[tokio::test]
async fn prompt_route_compact_returns_handled_and_rewrites_transcript() {
    let runtime = runtime(Arc::new(SummaryLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let session_id = create_session(app.clone(), &token).await;
    let sid = SessionId::from(session_id.clone());

    for text in ["one", "two", "three"] {
        runtime
            .event_store()
            .append_event(DurableEvent::new(
                sid.clone(),
                None,
                DurableEventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: text.into(),
                    attachments: vec![],
                    accepted_seq: None,
                },
            ))
            .await
            .unwrap();
        runtime
            .event_store()
            .append_event(DurableEvent::new(
                sid.clone(),
                None,
                DurableEventPayload::AssistantMessageCompleted {
                    message_id: new_message_id(),
                    text: format!("answer {text}"),
                    reasoning_content: None,
                },
            ))
            .await
            .unwrap();
    }

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

    let state = runtime
        .event_store()
        .session_read_model(&sid)
        .await
        .unwrap();
    assert!(state.compactions.iter().any(|compaction| {
        compaction.trigger == "manual_command" && !compaction.summary.is_empty()
    }));
    assert!(!state.transcript.messages.iter().any(|message| {
        message
            .message
            .content
            .iter()
            .any(|content| matches!(content, LlmContent::Text { text } if text == "/compact"))
    }));
}

#[tokio::test]
async fn compact_route_returns_same_session_and_hydrates_post_compact_context() {
    let runtime = runtime(Arc::new(SummaryLlm)).await;
    let (app, token) = router(Arc::clone(&runtime)).unwrap();
    let session_id = create_session(app.clone(), &token).await;
    let sid = SessionId::from(session_id.clone());
    let read_fixture = "target/post-compact-read-fixture.txt";
    fs::create_dir_all("target").unwrap();
    fs::write(read_fixture, "pub fn compact_restore_fixture() {}").unwrap();

    runtime
        .event_store()
        .append_event(DurableEvent::new(
            sid.clone(),
            None,
            DurableEventPayload::ToolCallRequested {
                call_id: "read-call-1".into(),
                tool_name: "read".into(),
                arguments: serde_json::json!({ "path": read_fixture }),
                raw_arguments: None,
            },
        ))
        .await
        .unwrap();
    runtime
        .event_store()
        .append_event(DurableEvent::new(
            sid.clone(),
            None,
            DurableEventPayload::ToolCallCompleted {
                call_id: "read-call-1".into(),
                tool_name: "read".into(),
                result: ToolResult {
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
            .append_event(DurableEvent::new(
                sid.clone(),
                None,
                DurableEventPayload::UserMessage {
                    message_id: new_message_id(),
                    text: text.into(),
                    attachments: vec![],
                    accepted_seq: None,
                },
            ))
            .await
            .unwrap();
        runtime
            .event_store()
            .append_event(DurableEvent::new(
                sid.clone(),
                None,
                DurableEventPayload::AssistantMessageCompleted {
                    message_id: new_message_id(),
                    text: format!("answer {text}"),
                    reasoning_content: None,
                },
            ))
            .await
            .unwrap();
    }

    let response = post_json(
        app.clone(),
        &format!("/api/sessions/{session_id}/compact"),
        r#"{}"#,
        &token,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: CompactSessionResponse = serde_json::from_slice(&body_bytes(response).await).unwrap();
    let returned_session_id = body.session_id.expect("compact should return session_id");
    assert_eq!(returned_session_id, session_id, "same-session compact");

    let state = runtime
        .event_store()
        .session_read_model(&sid)
        .await
        .unwrap();
    let restored_context = astrcode_core::llm::LlmContent::join_text(
        state
            .transcript
            .messages
            .iter()
            .flat_map(|message| &message.message.content),
        "\n",
    );
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

async fn post_json_owned(
    app: Router,
    uri: &str,
    body: String,
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

/// Thin wrapper around [`InMemoryEventStore`] that returns a temp directory
/// for `session_store_dir`, enabling extensions (like mode) that need a real
/// filesystem path for state persistence.
struct TestEventStore {
    inner: InMemoryEventStore,
    temp_dir: PathBuf,
}

impl TestEventStore {
    fn new() -> Self {
        Self {
            inner: InMemoryEventStore::new(),
            temp_dir: std::env::temp_dir(),
        }
    }
}

#[async_trait::async_trait]
impl EventReader for TestEventStore {
    async fn replay_events(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        self.inner.replay_events(session_id).await
    }

    async fn latest_cursor(&self, session_id: &SessionId) -> Result<Option<Cursor>, StorageError> {
        self.inner.latest_cursor(session_id).await
    }

    async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        self.inner.replay_from(session_id, cursor).await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError> {
        self.inner.list_sessions().await
    }
}

#[async_trait::async_trait]
impl SessionReader for TestEventStore {
    async fn session_read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<SessionReadModel>, StorageError> {
        self.inner.session_read_model(session_id).await
    }

    async fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError> {
        self.inner.list_session_summaries().await
    }
}

#[async_trait::async_trait]
impl ToolResultArtifactStore for TestEventStore {
    async fn read_tool_result_artifact_by_path(
        &self,
        session_id: &SessionId,
        path: &str,
        char_offset: usize,
        max_chars: usize,
    ) -> Result<ToolResultArtifactSlice, StorageError> {
        self.inner
            .read_tool_result_artifact_by_path(session_id, path, char_offset, max_chars)
            .await
    }

    async fn write_tool_result_artifact(
        &self,
        session_id: &SessionId,
        artifact: ToolResultArtifactInput,
    ) -> Result<ToolResultArtifactRef, StorageError> {
        self.inner
            .write_tool_result_artifact(session_id, artifact)
            .await
    }
}

#[async_trait::async_trait]
impl SessionPathResolver for TestEventStore {
    async fn session_store_dir(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<PathBuf>, StorageError> {
        // Verify the session exists, then return a subdirectory in temp.
        self.inner.session_read_model(session_id).await?;
        Ok(Some(self.temp_dir.join(session_id.as_str())))
    }

    async fn planned_session_store_dir(
        &self,
        session_id: &SessionId,
        _working_dir: &str,
        _parent_session_id: Option<&SessionId>,
        _source_extension: Option<&str>,
    ) -> Result<Option<PathBuf>, StorageError> {
        Ok(Some(self.temp_dir.join(session_id.as_str())))
    }
}

#[async_trait::async_trait]
impl SessionEventJournal for TestEventStore {
    async fn create_session(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
        self.inner.create_session(event).await
    }

    async fn append_event(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
        self.inner.append_event(event).await
    }
}

#[async_trait::async_trait]
impl SessionStore for TestEventStore {
    async fn checkpoint(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<(), StorageError> {
        self.inner.checkpoint(session_id, cursor).await
    }
    async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        self.inner.delete_session(session_id).await
    }
}

async fn runtime(llm_provider: Arc<dyn LlmProvider>) -> Arc<ServerRuntime> {
    static NEXT_CONFIG_ID: AtomicU64 = AtomicU64::new(0);

    let effective = EffectiveConfig {
        llm: LlmSettings {
            provider_kind: "mock".into(),
            base_url: String::new(),
            api_key: String::new(),
            wire_format: ProviderWireFormat::OpenAiChatCompletions,
            auth_scheme: ProviderAuthScheme::Bearer,
            model_id: "mock-model".into(),
            max_tokens: 1024,
            context_limit: 1024,
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            max_retries: 0,
            retry_base_delay_ms: 0,
            supports_prompt_cache_key: false,
            supports_stream_usage: false,
            supports_strict_tool_use: false,
            prompt_cache_retention: None,
            thinking: Default::default(),
            thinking_capability: None,
            thinking_configured: false,
        },
        small_llm: LlmSettings {
            provider_kind: "mock".into(),
            base_url: String::new(),
            api_key: String::new(),
            wire_format: ProviderWireFormat::OpenAiChatCompletions,
            auth_scheme: ProviderAuthScheme::Bearer,
            model_id: "mock-model".into(),
            max_tokens: 1024,
            context_limit: 1024,
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            max_retries: 0,
            retry_base_delay_ms: 0,
            supports_prompt_cache_key: false,
            supports_stream_usage: false,
            supports_strict_tool_use: false,
            prompt_cache_retention: None,
            thinking: Default::default(),
            thinking_capability: None,
            thinking_configured: false,
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
        permissions: Default::default(),
        extensions: ExtensionSettings::default(),
    };
    let event_store = Arc::new(TestEventStore::new()) as Arc<dyn SessionStore>;
    let extension_runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
    extension_runner
        .register(astrcode_extension_mode::extension())
        .await
        .unwrap();
    let context_assembler = Arc::new(LlmContextAssembler::new(ContextSettings::default()));
    let shell_timeout_secs = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
    let capabilities = astrcode_server::test_support::assemble_session_runtime_services_for_test(
        llm_provider.clone(),
        llm_provider,
        effective,
        extension_runner.clone(),
        context_assembler.clone(),
        std::sync::Arc::clone(&shell_timeout_secs),
    );
    let config = Arc::new(ConfigManager::new(
        Arc::new(astrcode_storage::config_store::FileConfigStore::new(
            std::path::PathBuf::from(format!(
                "target/test-config-{}.toml",
                NEXT_CONFIG_ID.fetch_add(1, Ordering::Relaxed)
            )),
        )),
        astrcode_core::config::Config::default(),
        Arc::clone(&extension_runner),
        shell_timeout_secs,
        Arc::clone(&capabilities),
    ));
    let session_manager = Arc::new(SessionManager::new(
        Arc::clone(&event_store),
        Arc::clone(&capabilities),
        vec![],
    ));
    let child_sessions = Arc::new(ChildSessionCoordinator::new(Arc::clone(&session_manager)));
    let scheduler = Arc::new(TurnScheduler::new(
        Arc::clone(&session_manager),
        Arc::new(TurnRegistry::new()),
        Arc::clone(&child_sessions),
    ));
    child_sessions.spawn_completion_watcher(Arc::clone(&scheduler));
    Arc::new(ServerRuntime::assemble_for_test(
        event_store,
        config,
        session_manager,
        scheduler,
        extension_runner,
        capabilities,
        std::env::temp_dir(),
    ))
}
