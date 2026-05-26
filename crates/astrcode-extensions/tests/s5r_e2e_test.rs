//! E2E：s5r 子进程扩展 — 覆盖 initialize / handler.invoke / host/invoke / ping / 全量 API。

use std::{
    fs,
    sync::{Arc, OnceLock},
    time::Duration,
};

use astrcode_core::{
    event::EventPayload,
    extension::{
        CommandContext, Extension, ExtensionCommandResult, ExtensionEvent, ExtensionHostServices,
        HookMode, LifecycleContext, PreToolUseContext, PreToolUseResult, Registrar, StopReason,
    },
    llm::{LlmEvent, LlmMessage, LlmProvider},
    tool::{ToolDefinition, ToolExecutionContext},
};
use astrcode_extension_sdk::config::ModelSelection;
use astrcode_extensions::{
    build_host_router, loader::ExtensionLoader, runner::ExtensionRunner, s5r_ext::S5rExtension,
};
use astrcode_storage::in_memory::InMemoryEventStore;
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
    let store: Arc<dyn astrcode_core::storage::EventStore> = Arc::new(InMemoryEventStore::new());
    build_host_router(Arc::new(ExtensionHostServices::new(store, None)), None)
}

struct MockLlm;

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
    let store: Arc<dyn astrcode_core::storage::EventStore> = Arc::new(InMemoryEventStore::new());
    build_host_router(
        Arc::new(ExtensionHostServices::new(store, Some(Arc::new(MockLlm)))),
        None,
    )
}

async fn load_s5r(router: Arc<astrcode_extensions::HostRouter>) -> Arc<S5rExtension> {
    let guest = ensure_guest_built();
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let ext_dir = std::env::temp_dir().join(format!("astrcode-s5r-e2e-{suffix}"));
    fs::create_dir_all(&ext_dir).unwrap();
    let manifest = serde_json::json!({
        "protocol": { "s5r": "1.0" },
        "command": [guest.to_string_lossy()]
    });
    fs::write(
        ext_dir.join("extension.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    S5rExtension::load(&ext_dir, &manifest, router, None)
        .await
        .expect("load s5r extension")
}

fn tool_ctx(working_dir: &str) -> ToolExecutionContext {
    ToolExecutionContext {
        session_id: "e2e-session".into(),
        working_dir: working_dir.into(),
        tool_call_id: None,
        event_tx: None,
        capabilities: Default::default(),
    }
}

fn pre_tool_use_ctx(tool_name: &str, tool_input: serde_json::Value) -> PreToolUseContext {
    PreToolUseContext {
        session_id: "e2e-session".into(),
        working_dir: "/tmp".into(),
        model: ModelSelection::simple("test"),
        tool_name: tool_name.into(),
        tool_input,
        available_tools: vec![],
        event_tx: None,
        extension_event_sink: None,
        session_store_dir: None,
    }
}

#[tokio::test]
async fn s5r_manifest_registers_tools_hooks_and_capabilities() {
    let ext = load_s5r(minimal_router()).await;
    assert_eq!(ext.id(), "s5r-guest-demo");
    assert!(
        ext.capabilities()
            .iter()
            .any(|c| { matches!(c, astrcode_core::extension::ExtensionCapability::SmallModel) })
    );

    let mut reg = Registrar::new();
    ext.register(&mut reg);
    assert!(reg.tools().iter().any(|(d, _)| d.name == "ping"));
    assert!(reg.tools().iter().any(|(d, _)| d.name == "greet"));
    assert_eq!(reg.pre_tool_use().len(), 1);
    assert_eq!(reg.pre_tool_use()[0].0, HookMode::Blocking);
    assert_eq!(reg.commands().len(), 1);
}

#[tokio::test]
async fn s5r_ping_health() {
    let ext = load_s5r(minimal_router()).await;
    ext.health().await.expect("extension/ping via health()");
}

#[tokio::test]
async fn s5r_ping_tool_returns_pong() {
    let ext = load_s5r(minimal_router()).await;
    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let (_, handler) = reg.tools().iter().find(|(d, _)| d.name == "ping").unwrap();
    let result = handler
        .execute("ping", serde_json::json!({}), "/tmp", &tool_ctx("/tmp"))
        .await
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(result.content, "pong");
}

#[tokio::test]
async fn s5r_greet_and_add_tools() {
    let ext = load_s5r(minimal_router()).await;
    let mut reg = Registrar::new();
    ext.register(&mut reg);

    let (_, greet) = reg.tools().iter().find(|(d, _)| d.name == "greet").unwrap();
    let r = greet
        .execute(
            "greet",
            serde_json::json!({ "name": "s5r" }),
            "/tmp",
            &tool_ctx("/tmp"),
        )
        .await
        .unwrap();
    assert_eq!(r.content, "hello, s5r!");

    let (_, add) = reg.tools().iter().find(|(d, _)| d.name == "add").unwrap();
    let r = add
        .execute(
            "add",
            serde_json::json!({ "a": 3, "b": 4 }),
            "/tmp",
            &tool_ctx("/tmp"),
        )
        .await
        .unwrap();
    assert_eq!(r.content, "3 + 4 = 7");
}

#[tokio::test]
async fn s5r_ask_llm_via_host_invoke() {
    let ext = load_s5r(mock_router()).await;
    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let (_, handler) = reg
        .tools()
        .iter()
        .find(|(d, _)| d.name == "ask_llm")
        .unwrap();
    let result = handler
        .execute(
            "ask_llm",
            serde_json::json!({ "prompt": "hello" }),
            "/tmp",
            &tool_ctx("/tmp"),
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
    let manifest = serde_json::json!({
        "protocol": { "s5r": "1.0" },
        "command": [guest.to_string_lossy()]
    });
    fs::write(
        ext_dir.join("extension.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let ext = S5rExtension::load(&ext_dir, &manifest, mock_router(), Some(wd_str.as_ref()))
        .await
        .expect("load");
    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let (_, handler) = reg
        .tools()
        .iter()
        .find(|(d, _)| d.name == "read_workspace")
        .unwrap();
    let result = handler
        .execute(
            "read_workspace",
            serde_json::json!({}),
            &wd_str,
            &tool_ctx(&wd_str),
        )
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
async fn s5r_pre_tool_use_blocks_and_emits_event() {
    let ext = load_s5r(mock_router()).await;
    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(5)));
    runner.register(ext).await.unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = PreToolUseContext {
        session_id: "e2e-session".into(),
        working_dir: "/tmp".into(),
        model: ModelSelection::simple("test"),
        tool_name: "emit_hook_probe".into(),
        tool_input: serde_json::json!({}),
        available_tools: vec![],
        event_tx: Some(tx),
        extension_event_sink: None,
        session_store_dir: None,
    };
    let result = runner.emit_pre_tool_use(ctx).await.unwrap();
    assert!(matches!(result, PreToolUseResult::Allow));

    let payload = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("timeout")
        .expect("channel closed");
    match payload {
        EventPayload::ExtensionEvent {
            extension_id,
            event_type,
            payload,
            ..
        } => {
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
    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let (_, handler) = reg
        .commands()
        .iter()
        .find(|(c, _)| c.name == "demo")
        .unwrap();
    let result = handler
        .execute(
            "demo",
            "",
            "/tmp",
            &CommandContext {
                session_id: "e2e-session".into(),
                working_dir: "/tmp".into(),
                model: ModelSelection::simple("test"),
                session_store_dir: None,
            },
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
            LifecycleContext {
                session_id: "e2e-session".into(),
                working_dir: "/tmp".into(),
                model: ModelSelection::simple("test"),
                event_tx: None,
                extension_event_sink: None,
                last_exchange: None,
            },
        )
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(800)).await;

    let tool = runner
        .collect_tool_adapters_typed("/tmp")
        .await
        .into_iter()
        .find(|t| t.definition().name == "pipeline_status")
        .expect("pipeline_status");

    let result = tool
        .execute(
            serde_json::json!({}),
            &ToolExecutionContext {
                session_id: "e2e-session".into(),
                working_dir: "/tmp".into(),
                tool_call_id: None,
                event_tx: None,
                capabilities: Default::default(),
            },
        )
        .await
        .unwrap();

    assert!(
        result.content.contains("steps=2"),
        "got: {}",
        result.content
    );
    assert!(
        result.content.contains("llm_ok=true"),
        "got: {}",
        result.content
    );
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
            "protocol": { "s5r": "1.0" },
            "command": [guest.to_string_lossy()]
        })
        .to_string(),
    )
    .unwrap();

    let (exts, errors) =
        ExtensionLoader::load_from_dir_for_test(&root, &Some(minimal_router()), None).await;
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(exts.len(), 1);
    assert_eq!(exts[0].id(), "s5r-guest-demo");
    let _ = fs::remove_dir_all(&root);
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
    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let handler = reg
        .tools()
        .iter()
        .find(|(d, _)| d.name == "slow")
        .map(|(_, handler)| Arc::clone(handler))
        .expect("slow tool");

    let slow_task = tokio::spawn(async move {
        handler
            .execute("slow", serde_json::json!({}), "/tmp", &tool_ctx("/tmp"))
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
