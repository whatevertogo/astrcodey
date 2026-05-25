//! E2E 测试：从真实 Rust 源码编译 s6r WASM 插件，通过完整管道加载和执行。
//!
//! 测试流程：
//! 1. 编译 tests/s6r-guest/ 到 wasm32-wasip1
//! 2. 通过 WasmExtension::load() 加载（调用 extension_manifest）
//! 3. 通过 Extension trait 注册工具/hooks/commands
//! 4. 执行各个工具和 hook，验证返回值

use std::sync::Arc;

use astrcode_extension_sdk::{
    config::ModelSelection,
    extension::{Extension, HookMode, PreToolUseContext, PreToolUseResult, Registrar},
    tool::ToolExecutionContext,
};
use astrcode_extensions::wasm_ext::WasmExtension;

/// guest 插件的 WASM 文件路径（由 cargo build --target wasm32-wasip1 生成）。
fn guest_wasm_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("s6r-guest")
        .join("target")
        .join("wasm32-wasip1")
        .join("release")
        .join("s6r_guest_demo.wasm")
}

fn load_guest() -> Arc<WasmExtension> {
    let wasm_path = guest_wasm_path();
    assert!(wasm_path.exists(), "guest WASM not found at {:?}", wasm_path);
    WasmExtension::load(&wasm_path, 10_000_000, 64 * 1024 * 1024).unwrap()
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

// ─── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn e2e_manifest_registers_tools_and_hooks() {
    let ext = load_guest();
    assert_eq!(ext.id(), "s6r-guest-demo");

    let mut reg = Registrar::new();
    ext.register(&mut reg);

    let tools = reg.tools();
    assert_eq!(tools.len(), 2);
    let names: Vec<_> = tools.iter().map(|(d, _)| d.name.as_str()).collect();
    assert!(names.contains(&"greet"), "greet tool not found: {names:?}");
    assert!(names.contains(&"add"), "add tool not found: {names:?}");

    let hooks = reg.pre_tool_use();
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].0, HookMode::Blocking);

    let cmds = reg.commands();
    assert_eq!(cmds.len(), 1);
    assert_eq!(cmds[0].0.name, "demo");
}

#[tokio::test]
async fn e2e_greet_tool_returns_hello() {
    let ext = load_guest();
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
async fn e2e_add_tool_computes_sum() {
    let ext = load_guest();
    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let (_, handler) = reg.tools().iter().find(|(d, _)| d.name == "add").unwrap();

    let result = handler
        .execute("add", serde_json::json!({ "a": 3, "b": 7 }), "/tmp", &tool_ctx())
        .await
        .unwrap();

    assert!(!result.is_error);
    assert_eq!(result.content, "3 + 7 = 10");
}

#[tokio::test]
async fn e2e_unknown_tool_returns_error() {
    let ext = load_guest();
    let mut reg = Registrar::new();
    ext.register(&mut reg);
    // greet 的 handler 对未知 tool_name 不会报错——因为 extension_call 把 name 传给 guest，
    // guest 会返回 unknown tool 错误
    let (_, handler) = reg.tools().iter().find(|(d, _)| d.name == "greet").unwrap();

    let result = handler
        .execute("nonexistent", serde_json::json!({}), "/tmp", &tool_ctx())
        .await
        .unwrap();

    assert!(result.is_error);
    assert!(result.content.contains("unknown tool"));
}

#[tokio::test]
async fn e2e_pre_tool_use_blocks_dangerous_command() {
    let ext = load_guest();
    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let (_, _, handler) = &reg.pre_tool_use()[0];

    let ctx = pre_tool_use_ctx(
        "bash",
        serde_json::json!({ "command": "rm -rf /important/data" }),
    );
    let result = handler.handle(ctx).await.unwrap();

    match result {
        PreToolUseResult::Block { reason } => {
            assert!(reason.contains("rm -rf"));
        }
        other => panic!("expected Block, got {other:?}"),
    }
}

#[tokio::test]
async fn e2e_pre_tool_use_allows_safe_command() {
    let ext = load_guest();
    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let (_, _, handler) = &reg.pre_tool_use()[0];

    let ctx = pre_tool_use_ctx("read_file", serde_json::json!({ "path": "/tmp/test.txt" }));
    let result = handler.handle(ctx).await.unwrap();
    assert!(matches!(result, PreToolUseResult::Allow));
}

#[tokio::test]
async fn e2e_pre_tool_use_allows_bash_without_rm_rf() {
    let ext = load_guest();
    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let (_, _, handler) = &reg.pre_tool_use()[0];

    let ctx = pre_tool_use_ctx("bash", serde_json::json!({ "command": "ls -la" }));
    let result = handler.handle(ctx).await.unwrap();
    assert!(matches!(result, PreToolUseResult::Allow));
}

#[tokio::test]
async fn e2e_command_demo_returns_display() {
    let ext = load_guest();
    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let (_, handler) = &reg.commands()[0];

    use astrcode_extension_sdk::extension::CommandContext;

    let ctx = CommandContext {
        session_id: "e2e-session".into(),
        working_dir: "/tmp".into(),
        model: ModelSelection::simple("test"),
        session_store_dir: None,
    };
    let result = handler.execute("demo", "", "/tmp", &ctx).await.unwrap();
    // guest 返回的 JSON data 会被反序列化为 ExtensionCommandResult
    // guest 的 handle_command 对 "demo" 返回 ok + data
    // parse_command_result 会从 data 反序列化
    // 由于 guest data 里包含 kind/content/is_error，应被解析为 Display
    match result {
        astrcode_extension_sdk::extension::ExtensionCommandResult::Display { content, .. } => {
            assert_eq!(content, "s6r guest demo works!");
        }
        other => panic!("expected Display, got {other:?}"),
    }
}
