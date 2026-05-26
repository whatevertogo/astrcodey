//! E2E：s5r-guest WASM 插件经 WasmExtension 加载与执行。

use std::{sync::Arc, time::Duration};

use astrcode_core::{
    event::EventPayload,
    extension::{
        Extension, ExtensionEvent, ExtensionHostServices, HookMode, LifecycleContext,
        PreToolUseContext, PreToolUseResult, Registrar,
    },
    llm::{LlmEvent, LlmMessage, LlmProvider},
    tool::{ToolDefinition, ToolExecutionContext},
};
use astrcode_extension_sdk::config::ModelSelection;
use astrcode_extensions::{build_host_router, runner::ExtensionRunner, wasm_ext::WasmExtension};
use astrcode_storage::in_memory::InMemoryEventStore;
use async_trait::async_trait;

fn guest_wasm_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("s5r-guest")
        .join("target")
        .join("wasm32-wasip1")
        .join("release")
        .join("s5r_guest_demo.wasm")
}

fn guest_manifest_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("s5r-guest")
        .join("Cargo.toml")
}

/// CI / 本地测试前确保 guest WASM 已编译（`wasm32-wasip1` release）。
fn ensure_guest_wasm_built() -> std::path::PathBuf {
    let wasm_path = guest_wasm_path();
    if wasm_path.exists() {
        return wasm_path;
    }

    let manifest = guest_manifest_path();
    eprintln!(
        "s5r E2E: building guest WASM via `cargo build --manifest-path {} --target wasm32-wasip1 \
         --release`",
        manifest.display()
    );

    let output = std::process::Command::new("cargo")
        .arg("build")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target")
        .arg("wasm32-wasip1")
        .arg("--release")
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn cargo build for s5r-guest: {e}"));

    if !output.status.success() {
        panic!(
            "cargo build s5r-guest failed (status={:?})\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    assert!(
        wasm_path.exists(),
        "guest WASM still missing at {} after build",
        wasm_path.display()
    );
    wasm_path
}

fn minimal_router() -> Arc<astrcode_extensions::HostRouter> {
    let store: Arc<dyn astrcode_core::storage::EventStore> = Arc::new(InMemoryEventStore::new());
    build_host_router(Arc::new(ExtensionHostServices::new(store, None)), None)
}

fn load_guest(router: Arc<astrcode_extensions::HostRouter>) -> Arc<WasmExtension> {
    let wasm_path = ensure_guest_wasm_built();
    WasmExtension::load(&wasm_path, 10_000_000, 64 * 1024 * 1024, router).unwrap()
}

fn tool_ctx() -> ToolExecutionContext {
    ToolExecutionContext {
        session_id: "e2e-session".into(),
        working_dir: "/tmp".into(),
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

#[tokio::test]
async fn e2e_manifest_registers_tools_and_hooks() {
    let ext = load_guest(minimal_router());
    assert_eq!(ext.id(), "s5r-guest-demo");

    let mut reg = Registrar::new();
    ext.register(&mut reg);

    assert_eq!(reg.tools().len(), 4);
    assert_eq!(reg.pre_tool_use().len(), 1);
    assert_eq!(reg.pre_tool_use()[0].0, HookMode::Blocking);
    assert_eq!(reg.commands().len(), 1);
}

#[tokio::test]
async fn e2e_greet_tool_returns_hello() {
    let ext = load_guest(minimal_router());
    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let (_, handler) = reg.tools().iter().find(|(d, _)| d.name == "greet").unwrap();

    let result = handler
        .execute(
            "greet",
            serde_json::json!({ "name": "AstrCode" }),
            "/tmp",
            &tool_ctx(),
        )
        .await
        .unwrap();

    assert!(!result.is_error);
    assert_eq!(result.content, "hello, AstrCode!");
}

#[tokio::test]
async fn e2e_pre_tool_use_blocks_dangerous_command() {
    let ext = load_guest(minimal_router());
    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let (_, _, handler) = &reg.pre_tool_use()[0];

    let result = handler
        .handle(pre_tool_use_ctx(
            "bash",
            serde_json::json!({ "command": "rm -rf /important/data" }),
        ))
        .await
        .unwrap();

    match result {
        PreToolUseResult::Block { reason } => assert!(reason.contains("rm -rf")),
        other => panic!("expected Block, got {other:?}"),
    }
}

#[tokio::test]
async fn e2e_pre_tool_use_hook_emit_via_host_router() {
    let ext = load_guest(mock_router());
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

    let payload = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for extension event")
        .expect("event channel closed");
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
        other => panic!("unexpected payload: {other:?}"),
    }
}

#[tokio::test]
async fn e2e_ask_llm_calls_host_router() {
    let ext = load_guest(mock_router());
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
            &tool_ctx(),
        )
        .await
        .unwrap();

    assert!(!result.is_error);
    assert_eq!(result.content, "mock-llm-response");
}

#[tokio::test]
async fn e2e_turn_end_continuations_invoke_small_llm() {
    let ext = load_guest(mock_router());
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
        .expect("pipeline_status tool");

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
