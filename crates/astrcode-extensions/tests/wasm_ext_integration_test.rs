//! WASM 外置扩展端到端集成测试。
//!
//! 用 wasm-encoder 动态构建 WASM 模块，通过 WasmExtension::load() 加载，
//! 验证宿主-guest 协议（注册、工具执行、事件处理）正确运行。

use std::sync::Arc;

use astrcode_core::{
    extension::{Extension, HookMode, PreToolUseContext, PreToolUseResult, Registrar},
    tool::{ExecutionMode, ToolExecutionContext},
};
use astrcode_extensions::wasm_ext::WasmExtension;
use tempfile::NamedTempFile;
use wasm_encoder::*;

// ─── Host import function indices (type indices) ────────────────────────────

const TYPE_HOST_REGISTER_TOOL: u32 = 0;
const TYPE_HOST_REGISTER_COMMAND: u32 = 1;
const TYPE_HOST_SUBSCRIBE: u32 = 2;
const TYPE_HOST_SET_RESPONSE: u32 = 3;
const TYPE_HOST_LOG: u32 = 4;
const TYPE_EXTENSION_INIT: u32 = 5;
const TYPE_HANDLER: u32 = 6;
const TYPE_ALLOC: u32 = 7;

const NUM_HOST_IMPORTS: u32 = 5;

// ─── WasmModuleBuilder ──────────────────────────────────────────────────────

enum InitCall {
    RegisterTool {
        name_off: u32,
        name_len: u32,
        desc_off: u32,
        desc_len: u32,
        schema_off: u32,
        schema_len: u32,
        execution_mode: u32,
    },
    Subscribe {
        event_disc: u32,
        mode_disc: u32,
    },
}

struct HandlerConfig {
    response_off: u32,
    response_len: u32,
    effect_code: i32,
}

struct WasmModuleBuilder {
    strings: Vec<u8>,
    init_calls: Vec<InitCall>,
    tool_handler: Option<HandlerConfig>,
    event_handler: Option<HandlerConfig>,
}

impl WasmModuleBuilder {
    fn new() -> Self {
        Self {
            strings: Vec::new(),
            init_calls: Vec::new(),
            tool_handler: None,
            event_handler: None,
        }
    }

    fn intern(&mut self, s: &str) -> (u32, u32) {
        let off = self.strings.len() as u32;
        self.strings.extend_from_slice(s.as_bytes());
        (off, s.len() as u32)
    }

    fn register_tool(&mut self, name: &str, desc: &str, schema: &str) {
        self.register_tool_with_execution_mode(name, desc, schema, 0);
    }

    fn register_tool_with_execution_mode(
        &mut self,
        name: &str,
        desc: &str,
        schema: &str,
        execution_mode: u32,
    ) {
        let (n_off, n_len) = self.intern(name);
        let (d_off, d_len) = self.intern(desc);
        let (s_off, s_len) = self.intern(schema);
        self.init_calls.push(InitCall::RegisterTool {
            name_off: n_off,
            name_len: n_len,
            desc_off: d_off,
            desc_len: d_len,
            schema_off: s_off,
            schema_len: s_len,
            execution_mode,
        });
    }

    fn subscribe(&mut self, event_disc: u8, mode_disc: u8) {
        self.init_calls.push(InitCall::Subscribe {
            event_disc: event_disc as u32,
            mode_disc: mode_disc as u32,
        });
    }

    fn set_tool_response(&mut self, response: &str, effect_code: i32) {
        let (off, len) = self.intern(response);
        self.tool_handler = Some(HandlerConfig {
            response_off: off,
            response_len: len,
            effect_code,
        });
    }

    fn set_event_response(&mut self, response: &str, effect_code: i32) {
        let (off, len) = self.intern(response);
        self.event_handler = Some(HandlerConfig {
            response_off: off,
            response_len: len,
            effect_code,
        });
    }

    fn build(self) -> Vec<u8> {
        let data_end = align_up(self.strings.len() as u32, 8);
        let mut next_func = NUM_HOST_IMPORTS;

        // ── Type section ──
        let mut types = TypeSection::new();
        // host_register_tool: 6 string-buffer i32s + 1 execution-mode discriminant
        types.ty().function([ValType::I32; 7], []);
        types.ty().function([ValType::I32; 4], []);
        types.ty().function([ValType::I32; 2], []);
        types.ty().function([ValType::I32; 2], []);
        types
            .ty()
            .function([ValType::I32, ValType::I32, ValType::I32], []);
        types.ty().function([], []);
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I32]);
        types.ty().function([ValType::I32], [ValType::I32]);

        // ── Import section ──
        let mut imports = ImportSection::new();
        imports.import(
            "env",
            "host_register_tool",
            EntityType::Function(TYPE_HOST_REGISTER_TOOL),
        );
        imports.import(
            "env",
            "host_register_command",
            EntityType::Function(TYPE_HOST_REGISTER_COMMAND),
        );
        imports.import(
            "env",
            "host_subscribe",
            EntityType::Function(TYPE_HOST_SUBSCRIBE),
        );
        imports.import(
            "env",
            "host_set_response",
            EntityType::Function(TYPE_HOST_SET_RESPONSE),
        );
        imports.import("env", "host_log", EntityType::Function(TYPE_HOST_LOG));

        // ── Function section — dynamic function indices ──
        let mut funcs = FunctionSection::new();
        let mut local_funcs = Vec::new(); // (name, kind, func_idx)

        // extension_init (always present but may be empty)
        let init_idx = next_func;
        funcs.function(TYPE_EXTENSION_INIT);
        local_funcs.push(("extension_init", init_idx));
        next_func += 1;

        let mut tool_handler_idx = None;
        if self.tool_handler.is_some() {
            let idx = next_func;
            funcs.function(TYPE_HANDLER);
            tool_handler_idx = Some(idx);
            next_func += 1;
        }

        let mut event_handler_idx = None;
        if self.event_handler.is_some() {
            let idx = next_func;
            funcs.function(TYPE_HANDLER);
            event_handler_idx = Some(idx);
            next_func += 1;
        }

        // alloc
        let alloc_idx = next_func;
        funcs.function(TYPE_ALLOC);

        // ── Global section (heap pointer) ──
        let mut globals = GlobalSection::new();
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(data_end as i32),
        );

        // ── Memory section ──
        let mut memories = MemorySection::new();
        memories.memory(MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });

        // ── Export section ──
        let mut exports = ExportSection::new();
        exports.export("memory", ExportKind::Memory, 0);
        exports.export("alloc", ExportKind::Func, alloc_idx);
        exports.export("extension_init", ExportKind::Func, init_idx);
        if let Some(idx) = tool_handler_idx {
            exports.export("handle_tool", ExportKind::Func, idx);
        }
        if let Some(idx) = event_handler_idx {
            exports.export("handle_event", ExportKind::Func, idx);
        }

        // ── Data section ──
        let mut data_section = DataSection::new();
        if !self.strings.is_empty() {
            data_section.active(0, &ConstExpr::i32_const(0), self.strings.iter().copied());
        }

        // ── Code section ──
        let mut code = CodeSection::new();

        // extension_init body
        {
            let mut f = Function::new([]);
            let mut insns = f.instructions();
            for call in &self.init_calls {
                match call {
                    InitCall::RegisterTool {
                        name_off,
                        name_len,
                        desc_off,
                        desc_len,
                        schema_off,
                        schema_len,
                        execution_mode,
                    } => {
                        insns.i32_const(*name_off as i32);
                        insns.i32_const(*name_len as i32);
                        insns.i32_const(*desc_off as i32);
                        insns.i32_const(*desc_len as i32);
                        insns.i32_const(*schema_off as i32);
                        insns.i32_const(*schema_len as i32);
                        insns.i32_const(*execution_mode as i32);
                        // host_register_tool is import index 0
                        insns.call(0);
                    },
                    InitCall::Subscribe {
                        event_disc,
                        mode_disc,
                    } => {
                        insns.i32_const(*event_disc as i32);
                        insns.i32_const(*mode_disc as i32);
                        // host_subscribe is import index 2
                        insns.call(2);
                    },
                }
            }
            insns.end();
            code.function(&f);
        }

        // handle_tool body (optional)
        if let Some(cfg) = &self.tool_handler {
            let mut f = Function::new([]);
            let mut insns = f.instructions();
            insns.i32_const(cfg.response_off as i32);
            insns.i32_const(cfg.response_len as i32);
            insns.call(3);
            insns.i32_const(cfg.effect_code);
            insns.end();
            code.function(&f);
        }

        // handle_event body (optional)
        if let Some(cfg) = &self.event_handler {
            let mut f = Function::new([]);
            let mut insns = f.instructions();
            insns.i32_const(cfg.response_off as i32);
            insns.i32_const(cfg.response_len as i32);
            insns.call(3);
            insns.i32_const(cfg.effect_code);
            insns.end();
            code.function(&f);
        }

        // alloc body: bump allocator using global 0
        {
            let mut f = Function::new([(1, ValType::I32)]);
            let mut insns = f.instructions();
            // local 0 = size (param), local 1 = old_ptr (local)
            insns.global_get(0); // push heap_ptr
            insns.local_set(1); // old_ptr = heap_ptr
            insns.global_get(0); // push heap_ptr
            insns.local_get(0); // push size
            insns.i32_add();
            insns.global_set(0); // heap_ptr += size
            insns.local_get(1); // return old_ptr
            insns.end();
            code.function(&f);
        }

        // ── Assemble module ──
        let mut module = Module::new();
        module.section(&types);
        module.section(&imports);
        module.section(&funcs);
        module.section(&memories);
        module.section(&globals);
        module.section(&exports);
        module.section(&code);
        if !self.strings.is_empty() {
            module.section(&data_section);
        }
        module.finish()
    }
}

fn align_up(v: u32, align: u32) -> u32 {
    (v + align - 1) & !(align - 1)
}

fn load_wasm(bytes: &[u8]) -> Arc<WasmExtension> {
    let mut tmp = NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut tmp, bytes).unwrap();
    WasmExtension::load(tmp.path(), "test-ext".into(), 10_000_000, 64 * 1024 * 1024).unwrap()
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
        model: astrcode_core::config::ModelSelection::simple("test-model"),
        tool_name: "shell".into(),
        tool_input: serde_json::json!({}),
        available_tools: vec![],
        extension_event_sink: None,
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn wasm_loads_and_registers_tools() {
    let mut b = WasmModuleBuilder::new();
    b.register_tool(
        "testTool",
        "A test tool",
        r#"{"type":"object","properties":{"text":{"type":"string"}}}"#,
    );
    b.set_tool_response("hello from wasm", 0);

    let ext = load_wasm(&b.build());

    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let tools = reg.tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].0.name, "testTool");
    assert_eq!(tools[0].0.description, "A test tool");
}

#[tokio::test]
async fn wasm_tool_handler_returns_ok() {
    let mut b = WasmModuleBuilder::new();
    b.register_tool("echoTool", "echo", r#"{"type":"object","properties":{}}"#);
    b.set_tool_response("hello from wasm", 0);

    let ext = load_wasm(&b.build());

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
async fn wasm_tool_handler_returns_error() {
    let mut b = WasmModuleBuilder::new();
    b.register_tool("failTool", "fail", r#"{"type":"object","properties":{}}"#);
    b.set_tool_response("something went wrong", 1); // GUEST_EFFECT_ERROR

    let ext = load_wasm(&b.build());

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
async fn wasm_event_handler_round_trips_response() {
    let mut b = WasmModuleBuilder::new();
    // Subscribe to PreToolUse (discriminant 4), blocking mode (discriminant 0)
    b.subscribe(4, 0);
    // handle_event returns ERROR (effect=1) with reason
    b.set_event_response(
        "blocked by wasm",
        1, // GUEST_EFFECT_ERROR → PreToolUseResult::Block
    );

    let ext = load_wasm(&b.build());

    let mut reg = Registrar::new();
    ext.register(&mut reg);

    let pre_handlers = reg.pre_tool_use();
    assert_eq!(pre_handlers.len(), 1);
    assert_eq!(pre_handlers[0].0, HookMode::Blocking);

    // Call the handler and verify Block result
    let handler = &pre_handlers[0].2;
    let result = handler.handle(pre_tool_use_ctx()).await.unwrap();
    match result {
        PreToolUseResult::Block { reason } => {
            assert_eq!(reason, "blocked by wasm");
        },
        other => panic!("Expected Block, got {other:?}"),
    }
}

#[tokio::test]
async fn wasm_minimal_module_loads_with_empty_capabilities() {
    let b = WasmModuleBuilder::new();
    // No tools, no subscriptions, no handlers
    let bytes = b.build();
    let ext = load_wasm(&bytes);

    let mut reg = Registrar::new();
    ext.register(&mut reg);
    assert!(reg.tools().is_empty());
    assert!(reg.commands().is_empty());
    assert!(reg.pre_tool_use().is_empty());
}

#[tokio::test]
async fn wasm_multiple_tools_registered() {
    let mut b = WasmModuleBuilder::new();
    b.register_tool(
        "tool1",
        "First tool",
        r#"{"type":"object","properties":{}}"#,
    );
    b.register_tool(
        "tool2",
        "Second tool",
        r#"{"type":"object","properties":{}}"#,
    );
    b.set_tool_response("ok", 0);

    let ext = load_wasm(&b.build());

    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let tools = reg.tools();
    assert_eq!(tools.len(), 2);

    let names: Vec<&str> = tools.iter().map(|(def, _)| def.name.as_str()).collect();
    assert!(names.contains(&"tool1"));
    assert!(names.contains(&"tool2"));
}

#[tokio::test]
async fn wasm_register_tool_parallel_execution_mode() {
    let mut b = WasmModuleBuilder::new();
    b.register_tool_with_execution_mode(
        "parallelTool",
        "parallel",
        r#"{"type":"object","properties":{}}"#,
        1,
    );
    b.set_tool_response("ok", 0);

    let ext = load_wasm(&b.build());

    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let tools = reg.tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].0.execution_mode, ExecutionMode::Parallel);
}

#[tokio::test]
async fn wasm_register_tool_unknown_execution_mode_defaults_to_sequential() {
    let mut b = WasmModuleBuilder::new();
    b.register_tool_with_execution_mode(
        "unknownTool",
        "unknown mode",
        r#"{"type":"object","properties":{}}"#,
        42,
    );
    b.set_tool_response("ok", 0);

    let ext = load_wasm(&b.build());

    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let tools = reg.tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].0.execution_mode, ExecutionMode::Sequential);
}

#[tokio::test]
async fn wasm_register_tool_overflow_execution_mode_defaults_to_sequential() {
    let mut b = WasmModuleBuilder::new();
    // 257 as i32 would wrap to 1 if cast to u8, incorrectly becoming Parallel
    b.register_tool_with_execution_mode(
        "overflowTool",
        "overflow mode",
        r#"{"type":"object","properties":{}}"#,
        257,
    );
    b.set_tool_response("ok", 0);

    let ext = load_wasm(&b.build());

    let mut reg = Registrar::new();
    ext.register(&mut reg);
    let tools = reg.tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].0.execution_mode, ExecutionMode::Sequential);
}
