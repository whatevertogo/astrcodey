//! E2E：s5r-guest WASM 插件经 WasmExtension 加载与执行。

use std::{sync::Arc, time::Duration};

use astrcode_core::{
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

fn minimal_router() -> Arc<astrcode_extensions::HostRouter> {
    let store: Arc<dyn astrcode_core::storage::EventStore> =
        Arc::new(InMemoryEventStore::new());
    build_host_router(
        Arc::new(ExtensionHostServices::new(store, None)),
        None,
    )
}

fn load_guest(router: Arc<astrcode_extensions::HostRouter>) -> Arc<WasmExtension> {
    let wasm_path = guest_wasm_path();
    assert!(wasm_path.exists(), "guest WASM not found at {wasm_path:?}");
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
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<LlmEvent>, astrcode_core::llm::LlmError>
    {
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
    let store: Arc<dyn astrcode_core::storage::EventStore> =
        Arc::new(InMemoryEventStore::new());
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
async fn e2e_ask_llm_calls_host_router() {
    let ext = load_guest(mock_router());
    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let (_, handler) = reg.tools().iter().find(|(d, _)| d.name == "ask_llm").unwrap();

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
        .emit_lifecycle(ExtensionEvent::TurnEnd, LifecycleContext {
            session_id: "e2e-session".into(),
            working_dir: "/tmp".into(),
            model: ModelSelection::simple("test"),
            extension_event_sink: None,
            last_exchange: None,
        })
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

    assert!(result.content.contains("steps=2"), "got: {}", result.content);
    assert!(result.content.contains("llm_ok=true"), "got: {}", result.content);
}
