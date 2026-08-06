//! E2E：s5r 子进程扩展 — 覆盖 initialize / handler.invoke / host/invoke / ping / 全量 API。

use std::{
    fs,
    sync::{Arc, OnceLock},
    time::Duration,
};

use astrcode_core::{
    event::{DurableEventPayload, EventPayload, ExtensionEventData},
    llm::{LlmEvent, LlmMessage, LlmProvider},
    tool::{ExecutionMode, ToolDefinition, ToolExecutionContext},
};
use astrcode_extension_sdk::{
    builder::manifest,
    config::ModelSelection,
    extension::{
        Extension, ExtensionCapability, ExtensionCommandResult, ExtensionError, ExtensionEvent,
        ExtensionHttpHandler, ExtensionHttpMethod, ExtensionHttpRequest, ExtensionHttpResponse,
        ExtensionHttpRoute, ExtensionManifest, ExtensionPackageManifest, ExtensionRegistrations,
        HookMode, HttpContext, PreToolUseResult, Registrar, RuntimeHookCallContext,
        RuntimeLifecycleContext, RuntimePreToolUseContext, StopReason,
    },
    testing::{CommandContextBuilder, ToolContextBuilder},
};
use astrcode_extensions::{
    HostBackends, build_host_router, build_host_router_with_public_http_dispatcher,
    loader::ExtensionLoader, runner::ExtensionRunner, s5r_ext::S5rExtension,
};
use astrcode_storage::{EventReader, SessionReader, in_memory::InMemoryEventStore};
use async_trait::async_trait;

fn guest_binary_path() -> std::path::PathBuf {
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("s5r-guest")
        .join("target")
        .join("release");
    #[cfg(windows)]
    let path = base.join("s5r_guest_demo.exe");
    #[cfg(not(windows))]
    let path = base.join("s5r_guest_demo");
    path
}

static GUEST_BINARY: OnceLock<std::path::PathBuf> = OnceLock::new();

fn ensure_guest_built() -> std::path::PathBuf {
    GUEST_BINARY
        .get_or_init(|| {
            let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("s5r-guest")
                .join("Cargo.toml");
            eprintln!(
                "s5r E2E: cargo build --release --manifest-path {}",
                manifest.display()
            );
            let output = std::process::Command::new("cargo")
                .arg("build")
                .arg("--release")
                .arg("--manifest-path")
                .arg(&manifest)
                .output()
                .expect("spawn cargo build s5r-guest");
            assert!(
                output.status.success(),
                "s5r-guest build failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                guest_binary_path().exists(),
                "s5r guest binary missing after build"
            );
            guest_binary_path()
        })
        .clone()
}

fn minimal_router() -> Arc<astrcode_extensions::HostRouter> {
    let store = Arc::new(InMemoryEventStore::new());
    let event_reader: Arc<dyn EventReader> = store.clone();
    let session_reader: Arc<dyn SessionReader> = store;
    build_host_router(HostBackends {
        event_reader: Some(event_reader),
        session_reader: Some(session_reader),
        ..HostBackends::default()
    })
}

struct MockLlm;

struct DispatchTargetExtension;

struct DispatchTargetHandler;

#[async_trait]
impl ExtensionHttpHandler for DispatchTargetHandler {
    async fn handle(&self, ctx: HttpContext) -> Result<ExtensionHttpResponse, ExtensionError> {
        let request = ctx.request();
        Ok(ExtensionHttpResponse::json(
            200,
            serde_json::json!({
                "id": request.path_params.get("id"),
                "query": request.query,
                "body": request.body,
            }),
        ))
    }
}

#[async_trait]
impl Extension for DispatchTargetExtension {
    fn manifest(&self) -> ExtensionManifest {
        manifest("dispatch-target")
            .version("test")
            .description("S5R public HTTP dispatch test target")
            .capability(ExtensionCapability::PublicHttp)
            .build()
    }

    fn register(&self, registrar: &mut Registrar) {
        registrar.http_route(
            ExtensionHttpRoute::public(ExtensionHttpMethod::Post, "/dispatch-target/{id}"),
            Arc::new(DispatchTargetHandler),
        );
    }
}

#[async_trait]
impl LlmProvider for MockLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<LlmEvent>, astrcode_core::llm::LlmError> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(LlmEvent::ContentDelta {
            delta: "mock-llm-response".into(),
        })
        .ok();
        tx.send(LlmEvent::Done {
            finish_reason: "stop".into(),
        })
        .ok();
        Ok(rx)
    }

    fn model_limits(&self) -> astrcode_core::llm::ModelLimits {
        astrcode_core::llm::ModelLimits {
            max_input_tokens: 8192,
            max_output_tokens: 1024,
        }
    }
}

fn mock_router() -> Arc<astrcode_extensions::HostRouter> {
    let store = Arc::new(InMemoryEventStore::new());
    let event_reader: Arc<dyn EventReader> = store.clone();
    let session_reader: Arc<dyn SessionReader> = store;
    build_host_router(HostBackends {
        main_llm: Some(Arc::new(MockLlm)),
        small_llm: Some(Arc::new(MockLlm)),
        event_reader: Some(event_reader),
        session_reader: Some(session_reader),
        ..HostBackends::default()
    })
}

async fn load_s5r(router: Arc<astrcode_extensions::HostRouter>) -> Arc<S5rExtension> {
    let guest = ensure_guest_built();
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ext_dir = std::env::temp_dir().join(format!("astrcode-s5r-e2e-{suffix}"));
    fs::create_dir_all(&ext_dir).unwrap();
    let manifest: ExtensionPackageManifest = serde_json::from_value(serde_json::json!({
        "extension_id": "s5r-guest-demo",
        "protocol": { "s5r": "2.0" },
        "command": [guest.to_string_lossy()]
    }))
    .unwrap();
    fs::write(
        ext_dir.join("extension.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    S5rExtension::load(&ext_dir, &manifest, router, None)
        .await
        .expect("load s5r extension")
}

fn registrations_for(extension: &S5rExtension) -> ExtensionRegistrations {
    let manifest = extension.manifest();
    let mut registrar = Registrar::new();
    extension.register(&mut registrar);
    registrar
        .finish(manifest)
        .map(|(_, registrations)| registrations)
        .expect("valid s5r extension registrations")
}

fn extension_tool_ctx(
    tool_name: &str,
    arguments: serde_json::Value,
    working_dir: &str,
) -> astrcode_extension_sdk::extension::ToolContext {
    ToolContextBuilder::new("s5r-guest-demo", tool_name)
        .session("e2e-session", working_dir, None)
        .arguments(arguments)
        .build()
}

fn core_tool_ctx(working_dir: &str) -> ToolExecutionContext {
    ToolExecutionContext::new(
        "e2e-session".into(),
        working_dir,
        None,
        None,
        Default::default(),
    )
}

fn runtime_hook_call() -> RuntimeHookCallContext {
    RuntimeHookCallContext::new("e2e-session", "/tmp", ModelSelection::simple("test"), None)
}

fn pre_tool_use_ctx(tool_name: &str, tool_input: serde_json::Value) -> RuntimePreToolUseContext {
    RuntimePreToolUseContext::new(
        runtime_hook_call(),
        "call-1".into(),
        tool_name,
        tool_input,
        astrcode_core::permission::ApprovalMode::Manual,
        Vec::new(),
    )
}

#[tokio::test]
async fn s5r_manifest_registers_tools_hooks_and_capabilities() {
    let ext = load_s5r(minimal_router()).await;
    let manifest = ext.manifest();
    assert_eq!(manifest.id(), "s5r-guest-demo");
    assert!(manifest.capabilities().iter().any(|c| {
        matches!(
            c,
            astrcode_extension_sdk::extension::ExtensionCapability::SmallModel
        )
    }));
    assert!(manifest.capabilities().iter().any(|capability| matches!(
        capability,
        astrcode_extension_sdk::extension::ExtensionCapability::SessionInspect
    )));

    let mut registrar = Registrar::new();
    ext.register(&mut registrar);
    let (_, registrations) = registrar
        .finish(manifest)
        .expect("valid s5r extension registrations");
    assert!(
        registrations
            .tools()
            .iter()
            .any(|tool| tool.definition().name == "ping")
    );
    assert!(
        registrations
            .tools()
            .iter()
            .any(|tool| tool.definition().name == "greet")
    );
    assert_eq!(registrations.pre_tool_use().len(), 1);
    assert_eq!(registrations.pre_tool_use()[0].mode, HookMode::Blocking);
    assert!(matches!(
        registrations.pre_tool_use()[0].target,
        astrcode_extension_sdk::extension::ToolHookTarget::All
    ));
    assert_eq!(registrations.commands().len(), 1);
    assert_eq!(registrations.http_routes().len(), 1);
    assert_eq!(registrations.http_routes()[0].route.path, "/s5r-probe/{id}");
}

#[tokio::test]
async fn s5r_http_route_dispatches_through_worker_handler() {
    let ext = load_s5r(minimal_router()).await;
    let runner = ExtensionRunner::new(Duration::from_secs(5));
    runner.register(ext).await.unwrap();

    let result = runner
        .dispatch_public_http_route(
            ExtensionHttpRequest {
                method: ExtensionHttpMethod::Post,
                path: "/s5r-probe/99".into(),
                path_params: Default::default(),
                query: Some("source=e2e".into()),
                body: serde_json::Value::Null,
            },
            br#"{"hello":"worker"}"#,
        )
        .await
        .unwrap();

    let astrcode_extensions::runner::ExtensionHttpDispatchResult::Response(response) = result
    else {
        panic!("expected HTTP response");
    };
    assert_eq!(response.status, 202);
    assert_eq!(response.body["id"], "99");
    assert_eq!(response.body["query"], "source=e2e");
    assert_eq!(response.body["body"]["hello"], "worker");
}

#[tokio::test]
async fn s5r_host_client_dispatches_to_another_extensions_public_route() {
    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(5)));
    runner
        .register(Arc::new(DispatchTargetExtension))
        .await
        .unwrap();
    let store = Arc::new(InMemoryEventStore::new());
    let event_reader: Arc<dyn EventReader> = store.clone();
    let session_reader: Arc<dyn SessionReader> = store;
    let router = build_host_router_with_public_http_dispatcher(
        HostBackends {
            event_reader: Some(event_reader),
            session_reader: Some(session_reader),
            ..HostBackends::default()
        },
        runner.clone(),
    );
    let ext = load_s5r(router).await;
    let registrations = registrations_for(ext.as_ref());
    let handler = registrations
        .tools()
        .iter()
        .find(|tool| tool.definition().name == "dispatch_public_http")
        .expect("dispatch_public_http tool")
        .handler();

    let result = handler
        .execute(extension_tool_ctx(
            "dispatch_public_http",
            serde_json::json!({}),
            "/tmp",
        ))
        .await
        .unwrap();

    assert!(!result.is_error);
    let body: serde_json::Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(body["id"], "42");
    assert_eq!(body["query"], "source=s5r");
    assert_eq!(body["body"]["from"], "guest");
}

#[tokio::test]
async fn s5r_ping_health() {
    let ext = load_s5r(minimal_router()).await;
    ext.health().await.expect("extension/ping via health()");
}

#[tokio::test]
async fn s5r_ping_tool_returns_pong() {
    let ext = load_s5r(minimal_router()).await;
    let registrations = registrations_for(ext.as_ref());
    let handler = registrations
        .tools()
        .iter()
        .find(|tool| tool.definition().name == "ping")
        .unwrap()
        .handler();
    let result = handler
        .execute(extension_tool_ctx("ping", serde_json::json!({}), "/tmp"))
        .await
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(result.content, "pong");
}

#[tokio::test]
async fn s5r_greet_and_add_tools() {
    let ext = load_s5r(minimal_router()).await;
    let registrations = registrations_for(ext.as_ref());

    let greet = registrations
        .tools()
        .iter()
        .find(|tool| tool.definition().name == "greet")
        .unwrap()
        .handler();
    let r = greet
        .execute(extension_tool_ctx(
            "greet",
            serde_json::json!({ "name": "s5r" }),
            "/tmp",
        ))
        .await
        .unwrap();
    assert_eq!(r.content, "hello, s5r!");

    let add = registrations
        .tools()
        .iter()
        .find(|tool| tool.definition().name == "add")
        .unwrap()
        .handler();
    let r = add
        .execute(extension_tool_ctx(
            "add",
            serde_json::json!({ "a": 3, "b": 4 }),
            "/tmp",
        ))
        .await
        .unwrap();
    assert_eq!(r.content, "3 + 4 = 7");
}

#[tokio::test]
async fn s5r_ask_llm_via_host_invoke() {
    let ext = load_s5r(mock_router()).await;
    let registrations = registrations_for(ext.as_ref());
    let handler = registrations
        .tools()
        .iter()
        .find(|tool| tool.definition().name == "ask_llm")
        .unwrap()
        .handler();
    let result = handler
        .execute(extension_tool_ctx(
            "ask_llm",
            serde_json::json!({ "prompt": "hello" }),
            "/tmp",
        ))
        .await
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(result.content, "mock-llm-response");
}

#[tokio::test]
async fn s5r_workspace_read_via_host_invoke() {
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let wd = std::env::temp_dir().join(format!("astrcode-s5r-ws-{suffix}"));
    fs::create_dir_all(&wd).unwrap();
    fs::write(wd.join("probe.txt"), "workspace-ok").unwrap();
    let wd_str = wd.to_string_lossy();

    let guest = ensure_guest_built();
    let ext_dir = wd.join("ext");
    fs::create_dir_all(&ext_dir).unwrap();
    let manifest: ExtensionPackageManifest = serde_json::from_value(serde_json::json!({
        "extension_id": "s5r-guest-demo",
        "protocol": { "s5r": "2.0" },
        "command": [guest.to_string_lossy()]
    }))
    .unwrap();
    fs::write(
        ext_dir.join("extension.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let ext = S5rExtension::load(&ext_dir, &manifest, mock_router(), Some(wd_str.as_ref()))
        .await
        .expect("load");
    let registrations = registrations_for(ext.as_ref());
    let handler = registrations
        .tools()
        .iter()
        .find(|tool| tool.definition().name == "read_workspace")
        .unwrap()
        .handler();
    let result = handler
        .execute(extension_tool_ctx(
            "read_workspace",
            serde_json::json!({}),
            &wd_str,
        ))
        .await
        .unwrap();
    assert!(
        result.content.contains("workspace-ok"),
        "got: {}",
        result.content
    );
    let _ = fs::remove_dir_all(&wd);
}

#[tokio::test]
async fn s5r_parallel_tools_keep_request_scoped_workspace_contexts() {
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("astrcode-s5r-parallel-{suffix}"));
    let workspace_a = root.join("a");
    let workspace_b = root.join("b");
    fs::create_dir_all(&workspace_a).unwrap();
    fs::create_dir_all(&workspace_b).unwrap();
    fs::write(workspace_a.join("probe.txt"), "workspace-a").unwrap();
    fs::write(workspace_b.join("probe.txt"), "workspace-b").unwrap();

    let ext = load_s5r(mock_router()).await;
    let registrations = registrations_for(ext.as_ref());
    let tool = registrations
        .tools()
        .iter()
        .find(|tool| tool.definition().name == "parallel_read_workspace")
        .expect("parallel_read_workspace tool");
    assert_eq!(tool.definition().execution_mode, ExecutionMode::Parallel);

    let workspace_a = workspace_a.to_string_lossy().into_owned();
    let workspace_b = workspace_b.to_string_lossy().into_owned();
    let context_a = extension_tool_ctx(
        "parallel_read_workspace",
        serde_json::json!({ "delay_ms": 150 }),
        &workspace_a,
    );
    let context_b = extension_tool_ctx(
        "parallel_read_workspace",
        serde_json::json!({ "delay_ms": 150 }),
        &workspace_b,
    );
    let call_a = tool.handler().execute(context_a);
    let call_b = tool.handler().execute(context_b);
    let (result_a, result_b) = tokio::join!(call_a, call_b);
    let result_a = result_a.expect("workspace A invocation");
    let result_b = result_b.expect("workspace B invocation");

    assert_eq!(result_a.content, "content=workspace-a peak=2");
    assert_eq!(result_b.content, "content=workspace-b peak=2");

    ext.stop(StopReason::Disabled).await.expect("stop");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn s5r_session_inspect_via_typed_host_client() {
    let ext = load_s5r(minimal_router()).await;
    let registrations = registrations_for(ext.as_ref());
    let handler = registrations
        .tools()
        .iter()
        .find(|tool| tool.definition().name == "inspect_sessions")
        .expect("inspect_sessions tool")
        .handler();

    let result = handler
        .execute(extension_tool_ctx(
            "inspect_sessions",
            serde_json::json!({}),
            "/tmp",
        ))
        .await
        .expect("invoke inspect_sessions");

    assert_eq!(result.content, "session_count=0");
}

#[tokio::test]
async fn s5r_pre_tool_use_blocks_and_emits_event() {
    let ext = load_s5r(mock_router()).await;
    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(5)));
    runner.register(ext).await.unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = RuntimePreToolUseContext::new(
        runtime_hook_call().with_event_tx(Some(tx.into())),
        "call-1".into(),
        "emit_hook_probe",
        serde_json::json!({}),
        astrcode_core::permission::ApprovalMode::Manual,
        Vec::new(),
    );
    let result = runner.emit_pre_tool_use(ctx).await.unwrap();
    assert!(matches!(result, PreToolUseResult::Allow));

    let payload = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed");
    match payload {
        EventPayload::Durable(DurableEventPayload::ExtensionEvent(ExtensionEventData {
            extension_id,
            event_type,
            payload,
            ..
        })) => {
            assert_eq!(extension_id, "s5r-guest-demo");
            assert_eq!(event_type, "s5r_guest.probe");
            assert_eq!(payload["from"], "pre_tool_use");
        },
        other => panic!("unexpected: {other:?}"),
    }

    let block_ctx = pre_tool_use_ctx(
        "bash",
        serde_json::json!({ "command": "rm -rf /important/data" }),
    );
    match runner.emit_pre_tool_use(block_ctx).await.unwrap() {
        PreToolUseResult::Block { reason } => assert!(reason.contains("rm -rf")),
        other => panic!("expected Block, got {other:?}"),
    }
}

#[tokio::test]
async fn s5r_demo_command() {
    let ext = load_s5r(minimal_router()).await;
    let registrations = registrations_for(ext.as_ref());
    let (_, handler) = registrations
        .commands()
        .iter()
        .find(|(c, _)| c.name == "demo")
        .unwrap();
    let result = handler
        .execute(
            CommandContextBuilder::new("s5r-guest-demo", "demo")
                .session("e2e-session", "/tmp", None)
                .model(ModelSelection::simple("test"))
                .build(),
        )
        .await
        .unwrap();
    match result {
        ExtensionCommandResult::Display {
            content,
            is_error,
            status_update: _,
        } => {
            assert!(!is_error);
            assert!(content.contains("s5r guest demo"));
        },
        other => panic!("unexpected command result: {other:?}"),
    }
}

#[tokio::test]
async fn s5r_turn_end_continuations_and_pipeline() {
    let ext = load_s5r(mock_router()).await;
    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(30)));
    runner.register(ext).await.unwrap();

    runner
        .emit_lifecycle(
            ExtensionEvent::TurnEnd,
            RuntimeLifecycleContext::new(runtime_hook_call(), None),
        )
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(800)).await;

    let tool = runner
        .tool_catalog_snapshot_typed("/tmp")
        .await
        .tools
        .into_iter()
        .find(|t| t.definition().name == "pipeline_status")
        .expect("pipeline_status");

    let result = tool
        .execute(serde_json::json!({}), &core_tool_ctx("/tmp"))
        .await
        .unwrap();

    assert_eq!(result.content, "step_1_calls=1 step_2_calls=1 llm_ok=true");
}

#[tokio::test]
async fn s5r_loader_discovers_manifest() {
    let guest = ensure_guest_built();
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("astrcode-s5r-loader-{suffix}"));
    let ext_dir = root.join("demo");
    fs::create_dir_all(&ext_dir).unwrap();
    fs::write(
        ext_dir.join("extension.json"),
        serde_json::json!({
            "extension_id": "s5r-guest-demo",
            "protocol": { "s5r": "2.0" },
            "command": [guest.to_string_lossy()]
        })
        .to_string(),
    )
    .unwrap();

    let (exts, errors) =
        ExtensionLoader::load_from_dir_for_test(&root, &Some(minimal_router()), None).await;
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(exts.len(), 1);
    assert_eq!(exts[0].manifest().id(), "s5r-guest-demo");
    let _ = fs::remove_dir_all(&root);
}

#[tokio::test]
async fn s5r_load_rejects_package_and_handshake_id_mismatch() {
    let guest = ensure_guest_built();
    let root = tempfile::tempdir().unwrap();
    let manifest: ExtensionPackageManifest = serde_json::from_value(serde_json::json!({
        "extension_id": "different-extension",
        "protocol": { "s5r": "2.0" },
        "command": [guest.to_string_lossy()]
    }))
    .unwrap();

    let error = match S5rExtension::load(root.path(), &manifest, minimal_router(), None).await {
        Ok(_) => panic!("mismatched package and handshake ids must be rejected"),
        Err(error) => error,
    };

    assert!(error.contains("extension id mismatch"), "{error}");
    assert!(error.contains("different-extension"), "{error}");
    assert!(error.contains("s5r-guest-demo"), "{error}");
}

#[tokio::test]
async fn s5r_stop_shuts_down_process() {
    let ext = load_s5r(minimal_router()).await;
    ext.stop(StopReason::Disabled).await.expect("stop");
    ext.health().await.expect_err("process should be gone");
}

#[tokio::test]
async fn s5r_cancel_on_stop_during_slow_tool() {
    let ext = load_s5r(minimal_router()).await;
    let registrations = registrations_for(ext.as_ref());
    let handler = registrations
        .tools()
        .iter()
        .find(|tool| tool.definition().name == "slow")
        .map(|tool| Arc::clone(tool.handler()))
        .expect("slow tool");

    let slow_task = tokio::spawn(async move {
        handler
            .execute(extension_tool_ctx("slow", serde_json::json!({}), "/tmp"))
            .await
    });

    tokio::time::sleep(Duration::from_millis(300)).await;
    ext.stop(StopReason::Disabled).await.expect("stop");

    let result = tokio::time::timeout(Duration::from_secs(5), slow_task)
        .await
        .expect("slow tool task timed out")
        .expect("slow tool join");

    match result {
        Ok(tool_result) => {
            assert!(
                tool_result.is_error,
                "expected cancelled tool error, got: {}",
                tool_result.content
            );
            assert!(
                tool_result.content.contains("cancel"),
                "unexpected content: {}",
                tool_result.content
            );
        },
        Err(err) => {
            let msg = err.to_string().to_lowercase();
            assert!(
                msg.contains("cancel") || msg.contains("closed"),
                "unexpected error: {err}"
            );
        },
    }
}
