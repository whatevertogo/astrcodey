//! 集成测试：s6r 协议 WASM 扩展适配器。
//!
//! 使用 WAT（WebAssembly Text Format）动态构建测试模块，验证 s6r ABI 的端到端行为：
//! `extension_manifest()` 声明式注册 + `extension_call()` 统一调用入口。

use std::sync::Arc;

use astrcode_extension_sdk::{
    config::ModelSelection,
    extension::{Extension, PreToolUseContext, PreToolUseResult, Registrar},
    s6r::S6R_VERSION,
    tool::ToolExecutionContext,
};
use astrcode_extensions::wasm_ext::WasmExtension;
use tempfile::NamedTempFile;

// ─── WAT 模块构建器 ──────────────────────────────────────────────────────────

/// 构建一个最小化的 s6r 兼容 WASM 模块。
///
/// 模块内存布局：
/// - 偏移 512：manifest JSON 数据
/// - 偏移 512 + manifest_len + 8（对齐）：response JSON 数据
/// - 偏移 bump_start：bump 分配器起始（host 写入请求的缓冲区）
struct S6rModuleBuilder {
    manifest: serde_json::Value,
    response: serde_json::Value,
}

impl S6rModuleBuilder {
    fn new() -> Self {
        Self {
            manifest: serde_json::json!({
                "s6r": S6R_VERSION,
                "id": "test-ext",
                "version": "0.1.0",
                "tools": [],
                "commands": [],
                "hooks": []
            }),
            response: serde_json::json!({ "id": "req-0", "ok": true, "effect": "ok" }),
        }
    }

    /// 覆盖整个 manifest JSON。
    fn manifest(mut self, manifest: serde_json::Value) -> Self {
        self.manifest = manifest;
        self
    }

    /// 覆盖 `extension_call` 固定返回的 response JSON。
    fn response(mut self, response: serde_json::Value) -> Self {
        self.response = response;
        self
    }

    /// 构建 WASM 二进制。
    fn build(&self) -> Vec<u8> {
        let manifest_json = serde_json::to_string(&self.manifest).unwrap();
        let response_json = serde_json::to_string(&self.response).unwrap();

        let manifest_bytes = manifest_json.as_bytes();
        let manifest_len = manifest_bytes.len();
        let manifest_escaped = escape_wat_bytes(manifest_bytes);

        let response_bytes = response_json.as_bytes();
        let response_len = response_bytes.len();
        let response_offset = 512 + manifest_len + 8; // 8 字节对齐填充
        let response_escaped = escape_wat_bytes(response_bytes);

        let bump_start = response_offset + response_len + 64;

        let wat = format!(
            r#"(module
  (import "env" "host_log" (func (param i32 i32 i32)))
  (import "env" "host_emit" (func (param i32 i32) (result i64)))
  (memory (export "memory") 2)
  (global $bump (mut i32) (i32.const {bump_start}))
  (data (i32.const 512) "{manifest_escaped}")
  (data (i32.const {response_offset}) "{response_escaped}")
  (func (export "alloc") (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $n)))
    (local.get $p))
  (func (export "dealloc") (param i32) (param i32))
  (func (export "extension_manifest") (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (i32.const 512)) (i64.const 32))
      (i64.extend_i32_u (i32.const {manifest_len}))))
  (func (export "extension_call") (param i32) (param i32) (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (i32.const {response_offset})) (i64.const 32))
      (i64.extend_i32_u (i32.const {response_len})))))"#,
        );

        wat::parse_str(&wat).expect("valid WAT")
    }
}

/// WAT 字符串字面量转义：每个字节 → `\xx`（两位十六进制）。
fn escape_wat_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("\\{b:02x}")).collect()
}

// ─── 测试辅助 ────────────────────────────────────────────────────────────────

fn load_wasm(bytes: &[u8]) -> Arc<WasmExtension> {
    let mut tmp = NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut tmp, bytes).unwrap();
    WasmExtension::load(tmp.path(), 10_000_000, 64 * 1024 * 1024, None).unwrap()
}

fn tool_execution_ctx() -> ToolExecutionContext {
    ToolExecutionContext {
        session_id: "test-session".into(),
        working_dir: String::new(),
        tool_call_id: None,
        event_tx: None,
        capabilities: Default::default(),
    }
}

fn pre_tool_use_ctx() -> PreToolUseContext {
    PreToolUseContext {
        session_id: "test-session".into(),
        working_dir: "/tmp".into(),
        model: ModelSelection::simple("test-model"),
        tool_name: "shell".into(),
        tool_input: serde_json::json!({}),
        available_tools: vec![],
        extension_event_sink: None,
        session_store_dir: None,
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn s6r_manifest_loads_tools() {
    let bytes = S6rModuleBuilder::new()
        .manifest(serde_json::json!({
            "s6r": S6R_VERSION, "id": "test-ext", "version": "0.1.0",
            "tools": [{
                "name": "testTool",
                "description": "A test tool",
                "parameters": { "type": "object", "properties": { "text": { "type": "string" } } },
                "mode": "sequential"
            }],
            "commands": [], "hooks": []
        }))
        .build();

    let ext = load_wasm(&bytes);
    let mut reg = Registrar::new();
    ext.register(&mut reg);

    let tools = reg.tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].0.name, "testTool");
    assert_eq!(tools[0].0.description, "A test tool");
}

#[tokio::test]
async fn s6r_tool_handler_returns_ok() {
    let bytes = S6rModuleBuilder::new()
        .manifest(serde_json::json!({
            "s6r": S6R_VERSION, "id": "test-ext", "version": "0.1.0",
            "tools": [{ "name": "echoTool", "description": "echo", "parameters": {} }],
            "commands": [], "hooks": []
        }))
        .response(serde_json::json!({
            "id": "req-0", "ok": true, "effect": "ok",
            "data": { "content": "hello from wasm" }
        }))
        .build();

    let ext = load_wasm(&bytes);
    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let tools = reg.tools();
    let (_, handler) = &tools[0];

    let result = handler
        .execute(
            "echoTool",
            serde_json::json!({}),
            "/tmp",
            &tool_execution_ctx(),
        )
        .await
        .unwrap();

    assert_eq!(result.content, "hello from wasm");
    assert!(!result.is_error);
}

#[tokio::test]
async fn s6r_tool_handler_returns_error() {
    let bytes = S6rModuleBuilder::new()
        .manifest(serde_json::json!({
            "s6r": S6R_VERSION, "id": "test-ext", "version": "0.1.0",
            "tools": [{ "name": "failTool", "description": "fail", "parameters": {} }],
            "commands": [], "hooks": []
        }))
        .response(serde_json::json!({
            "id": "req-0", "ok": false,
            "error": "something went wrong"
        }))
        .build();

    let ext = load_wasm(&bytes);
    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let (_, handler) = &reg.tools()[0];

    let result = handler
        .execute(
            "failTool",
            serde_json::json!({}),
            "/tmp",
            &tool_execution_ctx(),
        )
        .await
        .unwrap();

    assert!(result.is_error);
    assert!(result.content.contains("something went wrong"));
}

#[tokio::test]
async fn s6r_pre_tool_use_hook_blocks() {
    use astrcode_extension_sdk::extension::HookMode;

    let bytes = S6rModuleBuilder::new()
        .manifest(serde_json::json!({
            "s6r": S6R_VERSION, "id": "test-ext", "version": "0.1.0",
            "tools": [], "commands": [],
            "hooks": [{ "on": "pre_tool_use", "mode": "blocking" }]
        }))
        .response(serde_json::json!({
            "id": "req-0", "ok": true,
            "effect": "block",
            "data": { "reason": "blocked by wasm" }
        }))
        .build();

    let ext = load_wasm(&bytes);
    let mut reg = Registrar::new();
    ext.register(&mut reg);

    let pre_handlers = reg.pre_tool_use();
    assert_eq!(pre_handlers.len(), 1);
    assert_eq!(pre_handlers[0].0, HookMode::Blocking);

    let result = pre_handlers[0].2.handle(pre_tool_use_ctx()).await.unwrap();
    match result {
        PreToolUseResult::Block { reason } => assert_eq!(reason, "blocked by wasm"),
        other => panic!("expected Block, got {other:?}"),
    }
}

#[tokio::test]
async fn s6r_pre_tool_use_hook_allows() {
    let bytes = S6rModuleBuilder::new()
        .manifest(serde_json::json!({
            "s6r": S6R_VERSION, "id": "test-ext", "version": "0.1.0",
            "tools": [], "commands": [],
            "hooks": [{ "on": "pre_tool_use", "mode": "non_blocking" }]
        }))
        // default response is ok → Allow
        .build();

    let ext = load_wasm(&bytes);
    let mut reg = Registrar::new();
    ext.register(&mut reg);

    let pre_handlers = reg.pre_tool_use();
    let result = pre_handlers[0].2.handle(pre_tool_use_ctx()).await.unwrap();
    assert!(matches!(result, PreToolUseResult::Allow));
}

#[tokio::test]
async fn s6r_minimal_module_loads_with_empty_capabilities() {
    let bytes = S6rModuleBuilder::new().build();
    let ext = load_wasm(&bytes);

    let mut reg = Registrar::new();
    ext.register(&mut reg);
    assert!(reg.tools().is_empty());
    assert!(reg.pre_tool_use().is_empty());
}

#[tokio::test]
async fn s6r_multiple_tools_registered() {
    let bytes = S6rModuleBuilder::new()
        .manifest(serde_json::json!({
            "s6r": S6R_VERSION, "id": "test-ext", "version": "0.1.0",
            "tools": [
                { "name": "toolA", "description": "A", "parameters": {} },
                { "name": "toolB", "description": "B", "parameters": {} },
                { "name": "toolC", "description": "C", "parameters": {} }
            ],
            "commands": [], "hooks": []
        }))
        .build();

    let ext = load_wasm(&bytes);
    let mut reg = Registrar::new();
    ext.register(&mut reg);

    let tools = reg.tools();
    assert_eq!(tools.len(), 3);
    let names: Vec<_> = tools.iter().map(|(d, _)| d.name.as_str()).collect();
    assert!(names.contains(&"toolA"));
    assert!(names.contains(&"toolB"));
    assert!(names.contains(&"toolC"));
}

#[tokio::test]
async fn s6r_parallel_execution_mode() {
    use astrcode_extension_sdk::tool::ExecutionMode;

    let bytes = S6rModuleBuilder::new()
        .manifest(serde_json::json!({
            "s6r": S6R_VERSION, "id": "test-ext", "version": "0.1.0",
            "tools": [{ "name": "parallelTool", "description": "P", "parameters": {}, "mode": "parallel" }],
            "commands": [], "hooks": []
        }))
        .build();

    let ext = load_wasm(&bytes);
    let mut reg = Registrar::new();
    ext.register(&mut reg);

    let tools = reg.tools();
    assert_eq!(tools[0].0.execution_mode, ExecutionMode::Parallel);
}

#[tokio::test]
async fn s6r_unknown_mode_defaults_to_sequential() {
    use astrcode_extension_sdk::tool::ExecutionMode;

    let bytes = S6rModuleBuilder::new()
        .manifest(serde_json::json!({
            "s6r": S6R_VERSION, "id": "test-ext", "version": "0.1.0",
            "tools": [{ "name": "unknownTool", "description": "U", "parameters": {}, "mode": "unknown_mode" }],
            "commands": [], "hooks": []
        }))
        .build();

    let ext = load_wasm(&bytes);
    let mut reg = Registrar::new();
    ext.register(&mut reg);

    // 未知的 mode 字符串不等于 "parallel"，默认 Sequential
    assert_eq!(reg.tools()[0].0.execution_mode, ExecutionMode::Sequential);
}

#[tokio::test]
async fn s6r_wrong_protocol_version_rejected() {
    let manifest = serde_json::json!({
        "s6r": "999",  // 不兼容的版本
        "id": "test-ext", "version": "0.1.0",
        "tools": [], "commands": [], "hooks": []
    });
    // 使用一个合法的 S6R_VERSION manifest 作为基础，手动替换
    let bytes = S6rModuleBuilder::new().manifest(manifest).build();

    let mut tmp = NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut tmp, &bytes).unwrap();
    let result = WasmExtension::load(tmp.path(), 10_000_000, 64 * 1024 * 1024, None);
    let err = result.err().expect("should reject unknown s6r version");
    assert!(err.contains("unsupported s6r version"), "error was: {err}");
}

#[tokio::test]
async fn s6r_unknown_hook_names_are_warned_and_ignored() {
    // 包含未知的 hook 名称，应产生 warning 但不失败
    let bytes = S6rModuleBuilder::new()
        .manifest(serde_json::json!({
            "s6r": S6R_VERSION, "id": "test-ext", "version": "0.1.0",
            "tools": [], "commands": [],
            "hooks": [
                { "on": "pre_tool_use", "mode": "blocking" },
                { "on": "totally_unknown_hook_xyz", "mode": "blocking" }
            ]
        }))
        .build();

    let ext = load_wasm(&bytes);
    let mut reg = Registrar::new();
    ext.register(&mut reg);

    // 未知 hook 被忽略，只有 pre_tool_use 被注册
    assert_eq!(reg.pre_tool_use().len(), 1);
}
