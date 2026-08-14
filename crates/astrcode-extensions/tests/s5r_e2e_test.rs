//! E2E：s5r 子进程扩展 — 覆盖 initialize / handler.invoke / host/invoke / ping / 全量 API。

use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, OnceLock},
    time::Duration,
};

use astrcode_core::{
    event::{CustomEventData, DurableEventPayload, EventPayload},
    llm::{LlmEvent, LlmProvider},
    tool::{
        ExecutionMode, Tool, ToolCapabilities, ToolExecutionContext,
        access::{FileOperation, HostResource, ResourceAccess, ResourceLease, ToolPlan},
    },
    types::TurnId,
};
use astrcode_extension_sdk::{
    builder::manifest,
    config::ModelSelection,
    extension::{
        CommandAvailability, CommandExecution, Extension, ExtensionCapability,
        ExtensionCommandResult, ExtensionError, ExtensionHttpHandler, ExtensionHttpMethod,
        ExtensionHttpRequest, ExtensionHttpResponse, ExtensionHttpRoute, ExtensionManifest,
        ExtensionPackageManifest, HttpContext, LifecycleEvent, PreToolUseAdmission, Registrar,
        StopReason,
        internal::{
            RuntimeHookCallContext, RuntimePreToolUseContext, extension_stop_context,
            runtime_lifecycle_context, runtime_pre_tool_use_context,
        },
    },
};
use astrcode_extensions::{
    HostBackends, build_host_router, build_host_router_with_public_http_dispatcher,
    loader::{
        DiskExtensionSource, ExtensionLoadContext, ExtensionSource, prepare_extension_generation,
    },
    runner::ExtensionRunner,
    s5r_ext::S5rExtension,
};
use astrcode_storage::{EventReader, SessionReader, in_memory::InMemoryEventStore};
use async_trait::async_trait;

async fn sync_extension_sources(
    runner: &Arc<ExtensionRunner>,
    ctx: &ExtensionLoadContext,
    sources: &[&dyn ExtensionSource],
) -> Vec<String> {
    match prepare_extension_generation(runner, ctx, sources, &BTreeMap::new()).await {
        Ok(candidate) => {
            candidate.commit_with(|_| {}).await;
            Vec::new()
        },
        Err(errors) => errors,
    }
}

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
    async fn generate_request(
        &self,
        _request: astrcode_core::llm::LlmRequest,
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
        "protocol": { "s5r": "3.0" },
        "command": [guest.to_string_lossy()]
    }))
    .unwrap();
    fs::write(
        ext_dir.join("extension.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    S5rExtension::load(&ext_dir, &manifest, router)
        .await
        .expect("load s5r extension")
}

async fn register_s5r(
    runner: &Arc<ExtensionRunner>,
    router: Arc<astrcode_extensions::HostRouter>,
) -> Arc<S5rExtension> {
    runner.bind_host_router(Arc::clone(&router));
    let extension = load_s5r(router).await;
    runner.register(extension.clone()).await.unwrap();
    extension
}

async fn runner_with_s5r(router: Arc<astrcode_extensions::HostRouter>) -> Arc<ExtensionRunner> {
    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(5)));
    register_s5r(&runner, router).await;
    runner
}

async fn runner_tool(
    runner: &ExtensionRunner,
    tool_name: &str,
    working_dir: &str,
) -> Arc<dyn Tool> {
    runner
        .tool_catalog_snapshot_typed(working_dir)
        .await
        .tools
        .into_iter()
        .find(|tool| tool.definition().name == tool_name)
        .unwrap_or_else(|| panic!("missing {tool_name} tool"))
}

fn core_tool_ctx(working_dir: &str) -> ToolExecutionContext {
    ToolExecutionContext::new(
        "e2e-session".into(),
        working_dir,
        None,
        None,
        Default::default(),
    )
    .with_resource_lease(full_host_test_lease(working_dir))
}

fn attributed_tool_ctx(working_dir: &str) -> ToolExecutionContext {
    ToolExecutionContext::new(
        "e2e-session".into(),
        working_dir,
        Some("call-e2e".into()),
        None,
        Default::default(),
    )
    .with_resource_lease(full_host_test_lease(working_dir))
    .with_turn_id(TurnId::new("turn-e2e"))
}

fn full_host_test_lease(working_dir: &str) -> ResourceLease {
    let mut resources = vec![
        ResourceAccess::File {
            operation: FileOperation::ReadWrite,
            path: working_dir.into(),
            recursive: true,
        },
        ResourceAccess::search_file(working_dir, true),
    ];
    resources.extend(
        [
            HostResource::Process,
            HostResource::ToolResultArtifact,
            HostResource::Session,
            HostResource::Model,
            HostResource::Network,
            HostResource::Event,
            HostResource::ExtensionHttp,
        ]
        .map(ResourceAccess::host),
    );
    ResourceLease::from_plan(&ToolPlan::new(resources))
}

fn runtime_hook_call() -> RuntimeHookCallContext {
    RuntimeHookCallContext::new("e2e-session", "/tmp", ModelSelection::simple("test"), None)
}

fn pre_tool_use_ctx(tool_name: &str, tool_input: serde_json::Value) -> RuntimePreToolUseContext {
    runtime_pre_tool_use_context(
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
    let runner = runner_with_s5r(minimal_router()).await;

    let result = runner
        .dispatch_public_http_route(
            ExtensionHttpRequest {
                method: ExtensionHttpMethod::Post,
                path: "/s5r-probe/99".into(),
                path_params: BTreeMap::from([("id".into(), "forged".into())]),
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

    let error = runner
        .dispatch_public_http_route(
            ExtensionHttpRequest::new(ExtensionHttpMethod::Post, "/s5r-probe/invalid-status"),
            &[],
        )
        .await
        .expect_err("S5R responses must enforce the same HTTP status bounds");
    assert!(error.to_string().contains("status"), "{error}");
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
    register_s5r(&runner, router).await;
    let tool = runner_tool(&runner, "dispatch_public_http", "/tmp").await;
    let result = tool
        .execute(serde_json::json!({}), &core_tool_ctx("/tmp"))
        .await
        .unwrap();

    assert!(!result.is_error);
    let body: serde_json::Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(body["id"], "42");
    assert_eq!(body["query"], "source=s5r");
    assert_eq!(body["body"]["from"], "guest");
}

#[tokio::test]
async fn s5r_health_is_unavailable_until_runner_activation() {
    let router = minimal_router();
    let ext = load_s5r(Arc::clone(&router)).await;
    assert!(
        ext.health().await.is_err(),
        "initialized worker must not become callable before registration"
    );

    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(5)));
    runner.bind_host_router(router);
    runner.register(ext.clone()).await.unwrap();
    ext.health().await.expect("extension/ping via health()");
    runner.shutdown().await;
}

#[tokio::test]
async fn s5r_ping_tool_returns_pong() {
    let runner = runner_with_s5r(minimal_router()).await;
    let tool = runner_tool(&runner, "ping", "/tmp").await;
    let result = tool
        .execute(serde_json::json!({}), &core_tool_ctx("/tmp"))
        .await
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(result.content, "pong");

    let context_tool = runner_tool(&runner, "call_context", "/tmp").await;
    let attributed = context_tool
        .execute(serde_json::json!({}), &attributed_tool_ctx("/tmp"))
        .await
        .unwrap();
    let context: serde_json::Value = serde_json::from_str(&attributed.content).unwrap();
    assert_eq!(context["extension_id"], "s5r-guest-demo");
    assert_eq!(context["session_id"], "e2e-session");
    assert_eq!(context["turn_id"], "turn-e2e");
    assert_eq!(context["tool_call_id"], "call-e2e");
    assert_eq!(context["working_dir"], "/tmp");
}

#[tokio::test]
async fn s5r_greet_and_add_tools() {
    let runner = runner_with_s5r(minimal_router()).await;
    let greet = runner_tool(&runner, "greet", "/tmp").await;
    let r = greet
        .execute(serde_json::json!({ "name": "s5r" }), &core_tool_ctx("/tmp"))
        .await
        .unwrap();
    assert_eq!(r.content, "hello, s5r!");

    let add = runner_tool(&runner, "add", "/tmp").await;
    let r = add
        .execute(
            serde_json::json!({ "a": 3, "b": 4 }),
            &core_tool_ctx("/tmp"),
        )
        .await
        .unwrap();
    assert_eq!(r.content, "3 + 4 = 7");
}

#[tokio::test]
async fn s5r_ask_llm_via_host_invoke() {
    let runner = runner_with_s5r(mock_router()).await;
    let tool = runner_tool(&runner, "ask_llm", "/tmp").await;
    let result = tool
        .execute(
            serde_json::json!({ "prompt": "hello" }),
            &core_tool_ctx("/tmp"),
        )
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
        "protocol": { "s5r": "3.0" },
        "command": [guest.to_string_lossy()]
    }))
    .unwrap();
    fs::write(
        ext_dir.join("extension.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let router = mock_router();
    let ext = S5rExtension::load(&ext_dir, &manifest, Arc::clone(&router))
        .await
        .expect("load");
    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(5)));
    runner.bind_host_router(router);
    runner.register(ext).await.unwrap();
    let tool = runner_tool(&runner, "read_workspace", &wd_str).await;
    let result = tool
        .execute(serde_json::json!({}), &core_tool_ctx(&wd_str))
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

    let runner = runner_with_s5r(mock_router()).await;

    let workspace_a = workspace_a.to_string_lossy().into_owned();
    let workspace_b = workspace_b.to_string_lossy().into_owned();
    let tool_a = runner_tool(&runner, "parallel_read_workspace", &workspace_a).await;
    let tool_b = runner_tool(&runner, "parallel_read_workspace", &workspace_b).await;
    assert_eq!(tool_a.execution_mode(), ExecutionMode::Parallel);
    let context_a = core_tool_ctx(&workspace_a);
    let context_b = core_tool_ctx(&workspace_b);
    let call_a = tool_a.execute(serde_json::json!({ "delay_ms": 150 }), &context_a);
    let call_b = tool_b.execute(serde_json::json!({ "delay_ms": 150 }), &context_b);
    let (result_a, result_b) = tokio::join!(call_a, call_b);
    let result_a = result_a.expect("workspace A invocation");
    let result_b = result_b.expect("workspace B invocation");

    assert_eq!(result_a.content, "content=workspace-a peak=2");
    assert_eq!(result_b.content, "content=workspace-b peak=2");

    assert!(runner.shutdown().await.is_empty());
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn s5r_session_inspect_via_typed_host_client() {
    let runner = runner_with_s5r(minimal_router()).await;
    let tool = runner_tool(&runner, "inspect_sessions", "/tmp").await;
    let result = tool
        .execute(serde_json::json!({}), &core_tool_ctx("/tmp"))
        .await
        .expect("invoke inspect_sessions");

    assert_eq!(result.content, "session_count=0");
}

#[tokio::test]
async fn s5r_session_state_roundtrips_via_typed_host_client() {
    let runner = runner_with_s5r(minimal_router()).await;
    let tool = runner_tool(&runner, "session_state_roundtrip", "/tmp").await;
    let store = tempfile::tempdir().unwrap();
    let mut capabilities = ToolCapabilities::default();
    capabilities.paths.store_dir = Some(store.path().to_path_buf());
    let ctx = ToolExecutionContext::new(
        "e2e-session".into(),
        "/tmp",
        Some("call-state".into()),
        None,
        capabilities,
    )
    .with_resource_lease(full_host_test_lease("/tmp"));

    let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();

    assert!(!result.is_error);
    assert_eq!(result.content, "state-roundtrip-ok");
}

#[tokio::test]
async fn s5r_pre_tool_use_blocks_and_emits_event() {
    let runner = runner_with_s5r(mock_router()).await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = runtime_pre_tool_use_context(
        runtime_hook_call().with_event_tx(Some(tx.into())),
        "call-1".into(),
        "emit_hook_probe",
        serde_json::json!({}),
        astrcode_core::permission::ApprovalMode::Manual,
        Vec::new(),
    );
    let result = runner.emit_pre_tool_use(ctx).await.unwrap();
    assert!(matches!(result, PreToolUseAdmission::Allow));

    let payload = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed");
    match payload {
        EventPayload::Durable(DurableEventPayload::CustomEvent(CustomEventData {
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
        PreToolUseAdmission::Block { reason } => assert!(reason.contains("rm -rf")),
        other => panic!("expected Block, got {other:?}"),
    }
}

#[tokio::test]
async fn s5r_demo_command() {
    let runner = runner_with_s5r(minimal_router()).await;
    let runtime =
        RuntimeHookCallContext::new("e2e-session", "/tmp", ModelSelection::simple("test"), None);
    let resolved = runner
        .resolve_commands_for_typed(&runtime.working_dir().to_string_lossy())
        .await
        .into_iter()
        .find(|resolved| resolved.command.name == "demo")
        .expect("demo command");
    assert_eq!(
        resolved.command.args_schema,
        Some(serde_json::json!({ "type": "string" }))
    );
    assert!(resolved.command.requires_idle);
    assert!(resolved.command.argument_completions);
    assert_eq!(resolved.command.priority, 17);
    assert_eq!(
        resolved.command.availability,
        CommandAvailability::InteractiveOnly
    );
    assert_eq!(resolved.command.execution, CommandExecution::Extension);
    let result = runner
        .invoke_resolved_command_typed(&resolved, "", &runtime)
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
    let completions = runner
        .complete_resolved_command_typed(&resolved, "demo", 4, &runtime)
        .await
        .unwrap();
    assert_eq!(completions.items.len(), 1);
    assert_eq!(completions.items[0].label, "demo");
    assert_eq!(completions.items[0].insert_text, "demo-value");
    assert_eq!(completions.items[0].detail.as_deref(), Some("4"));
}

#[tokio::test]
async fn s5r_turn_end_continuations_and_pipeline() {
    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(30)));
    register_s5r(&runner, mock_router()).await;

    runner
        .emit_lifecycle(
            LifecycleEvent::TurnEnd,
            runtime_lifecycle_context(runtime_hook_call(), None, 0),
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

    assert_eq!(
        result.content,
        "step_1_calls=1 step_2_calls=1 tool_calls=1 llm_ok=true"
    );
}

#[tokio::test]
async fn s5r_loader_discovers_manifest() {
    let guest = ensure_guest_built();
    let root = tempfile::tempdir().unwrap();
    let ext_dir = root.path().join(".astrcode/extensions/demo");
    fs::create_dir_all(&ext_dir).unwrap();
    fs::write(
        ext_dir.join("extension.json"),
        serde_json::json!({
            "extension_id": "s5r-guest-demo",
            "protocol": { "s5r": "3.0" },
            "command": [guest.to_string_lossy()]
        })
        .to_string(),
    )
    .unwrap();

    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
    let source = DiskExtensionSource::new(BTreeMap::new());
    let errors = sync_extension_sources(
        &runner,
        &ExtensionLoadContext {
            working_dir: Some(root.path().to_string_lossy().into_owned()),
            host_router: Some(minimal_router()),
            transport_profile: Default::default(),
        },
        &[&source],
    )
    .await;
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(runner.registered_extension_ids().await, ["s5r-guest-demo"]);
    assert!(runner.shutdown().await.is_empty());
}

#[tokio::test]
async fn s5r_load_rejects_package_and_handshake_id_mismatch() {
    let guest = ensure_guest_built();
    let root = tempfile::tempdir().unwrap();
    let manifest: ExtensionPackageManifest = serde_json::from_value(serde_json::json!({
        "extension_id": "different-extension",
        "protocol": { "s5r": "3.0" },
        "command": [guest.to_string_lossy()]
    }))
    .unwrap();

    let error = match S5rExtension::load(root.path(), &manifest, minimal_router()).await {
        Ok(_) => panic!("mismatched package and handshake ids must be rejected"),
        Err(error) => error,
    };

    assert!(error.contains("host expected extension"), "{error}");
    assert!(error.contains("different-extension"), "{error}");
    assert!(error.contains("s5r-guest-demo"), "{error}");
}

#[tokio::test]
async fn s5r_stop_shuts_down_process() {
    let ext = load_s5r(minimal_router()).await;
    ext.stop(extension_stop_context(StopReason::Disabled))
        .await
        .expect("stop");
    ext.health().await.expect_err("process should be gone");
}

#[tokio::test]
async fn s5r_cancel_on_stop_during_slow_tool() {
    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(5)));
    let ext = register_s5r(&runner, minimal_router()).await;
    let tool = runner_tool(&runner, "slow", "/tmp").await;

    let slow_task = tokio::spawn(async move {
        tool.execute(serde_json::json!({}), &core_tool_ctx("/tmp"))
            .await
    });

    tokio::time::sleep(Duration::from_millis(300)).await;
    ext.stop(extension_stop_context(StopReason::Disabled))
        .await
        .expect("stop");

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
                tool_result.content.contains("cancel") || tool_result.content.contains("closed"),
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
