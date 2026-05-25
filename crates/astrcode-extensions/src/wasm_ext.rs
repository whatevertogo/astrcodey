//! WASM 扩展适配器（s6r 协议）。
//!
//! 加载 `.wasm` 文件，调用 `extension_manifest()` 获取声明式注册信息，
//! 再通过统一的 `extension_call()` 入口处理所有工具/命令/hook 调用。
//!
//! ## 内存所有权规则
//!
//! - 请求内存：宿主调用 guest 的 `alloc` 分配，写入后传给 `extension_call`，
//!   调用返回后由宿主调用 guest 的 `dealloc` 释放。
//! - 响应内存：guest 内部分配（`extension_call` / `extension_manifest` 返回 packed ptr），
//!   宿主读取 JSON 字符串后调用 `dealloc` 释放。
//!
//! ## 并发模型
//!
//! `WasmInner` 由 `parking_lot::Mutex` 保护。wasmtime `Store` 是 `!Send`，
//! 所有 guest 调用通过 `spawn_blocking` 在 blocking 线程池执行，
//! runtime worker 线程不被阻塞。

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use astrcode_extension_sdk::{
    extension::{
        CommandContext, CommandHandler, CompactContext, CompactContributions, CompactEvent,
        CompactHandler, CompactResult, EXTENSION_TOOL_OUTCOME_KEY, Extension, ExtensionCapability,
        ExtensionCommandResult, ExtensionError, ExtensionEvent, ExtensionToolOutcome, HookMode,
        HookResult, LifecycleContext, LifecycleHandler, PostToolUseContext, PostToolUseHandler,
        PostToolUseResult, PreToolUseContext, PreToolUseHandler, PreToolUseResult,
        PromptBuildContext, PromptBuildHandler, PromptContributions, ProviderContext,
        ProviderEvent, ProviderHandler, ProviderResult, Registrar, SlashCommand, ToolHandler,
    },
    s6r::{self, CallRequest, CallResponse, Manifest, event_to_name},
    tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult, tool_metadata},
};
use parking_lot::Mutex;
use serde_json::json;

use crate::wasm_api::{self, HostState};

pub use crate::wasm_api::HostInvoker;

// ─── 请求 ID 生成 ────────────────────────────────────────────────────────

fn new_req_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("req-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

// ─── WasmInner ──────────────────────────────────────────────────────────

/// 持有 wasmtime 运行时状态。
///
/// s6r 协议下只需要三个函数引用：内存分配/释放 + 统一调用入口。
struct WasmInner {
    store:      wasmtime::Store<HostState>,
    memory:     wasmtime::Memory,
    alloc_fn:   wasmtime::TypedFunc<i32, i32>,
    /// guest 导出的 `dealloc(ptr, len)`，宿主用于释放 guest 分配的内存。
    dealloc_fn: wasmtime::TypedFunc<(i32, i32), ()>,
    /// guest 导出的 `extension_call(req_ptr, req_len) -> i64`。
    call_fn:    wasmtime::TypedFunc<(i32, i32), i64>,
}

type SharedInner = Arc<Mutex<WasmInner>>;

// ─── guest 调用核心 ──────────────────────────────────────────────────────

/// 将 `CallRequest` 序列化后发送给 guest，返回解析好的 `CallResponse`。
///
/// 在 `spawn_blocking` 中执行，因为 wasmtime 是同步的。
async fn call_guest(
    inner: &SharedInner,
    request: CallRequest,
) -> Result<CallResponse, ExtensionError> {
    let inner = Arc::clone(inner);
    let json = serde_json::to_string(&request)
        .map_err(|e| ExtensionError::Internal(format!("serialize CallRequest: {e}")))?;
    tokio::task::spawn_blocking(move || call_guest_blocking(&inner, &json))
        .await
        .map_err(|e| ExtensionError::Internal(format!("wasm join error: {e}")))?
}

fn call_guest_blocking(
    inner: &Mutex<WasmInner>,
    request_json: &str,
) -> Result<CallResponse, ExtensionError> {
    let mut guard = inner.lock();

    // 每次调用独立重置 fuel，init 消耗不影响后续调用额度。
    let fuel_budget = guard.store.data().fuel_budget;
    guard
        .store
        .set_fuel(fuel_budget)
        .map_err(|e| ExtensionError::Internal(format!("set_fuel: {e}")))?;

    let memory     = guard.memory;
    let alloc_fn   = guard.alloc_fn.clone();
    let dealloc_fn = guard.dealloc_fn.clone();
    let call_fn    = guard.call_fn.clone();

    // 1. 写请求到 guest 内存（通过 guest 的 alloc 分配）
    let (req_ptr, req_len) =
        wasm_api::write_to_guest(&mut guard.store, &memory, &alloc_fn, request_json.as_bytes())
            .map_err(ExtensionError::Internal)?;

    // 2. 调用 extension_call，返回 packed (resp_ptr << 32 | resp_len)
    let packed = call_fn
        .call(&mut guard.store, (req_ptr as i32, req_len as i32))
        .map_err(|e| {
            if guard.store.get_fuel().is_ok_and(|f| f == 0) {
                ExtensionError::Timeout((fuel_budget / 1_000_000).max(1) * 1000)
            } else {
                ExtensionError::Internal(format!("wasm trap: {e}"))
            }
        })?;

    // 3. 释放请求内存（无论调用成功与否都要释放）
    let _ = dealloc_fn.call(&mut guard.store, (req_ptr as i32, req_len as i32));

    // 4. 解析响应 packed ptr
    if packed == 0 {
        return Err(ExtensionError::Internal("extension_call returned null ptr".into()));
    }
    let resp_ptr = ((packed >> 32) & 0xFFFF_FFFF) as u32;
    let resp_len = (packed & 0xFFFF_FFFF) as u32;

    // 5. 从 guest 内存读取响应 JSON（共享借用，owned String 产生后借用结束）
    let resp_json =
        wasm_api::read_str_from_memory(&guard.store, &memory, resp_ptr, resp_len)
            .map_err(ExtensionError::Internal)?;

    // 6. 释放响应内存（需要 &mut guard.store，上一步借用已结束）
    let _ = dealloc_fn.call(&mut guard.store, (resp_ptr as i32, resp_len as i32));

    // 7. 反序列化 CallResponse
    serde_json::from_str::<CallResponse>(&resp_json)
        .map_err(|e| ExtensionError::Internal(format!("parse CallResponse: {e}")))
}

/// 从 packed i64 读取 manifest JSON，并释放 guest 内存。
fn read_manifest(
    store: &mut wasmtime::Store<HostState>,
    memory: &wasmtime::Memory,
    dealloc_fn: &wasmtime::TypedFunc<(i32, i32), ()>,
    packed: i64,
) -> Result<Manifest, String> {
    if packed == 0 {
        return Err("extension_manifest returned null ptr".into());
    }
    let ptr = ((packed >> 32) & 0xFFFF_FFFF) as u32;
    let len = (packed & 0xFFFF_FFFF) as u32;

    // 读取（共享借用）
    let json = wasm_api::read_str_from_memory(store, memory, ptr, len)?;

    // 释放（可变借用，上面借用已结束）
    let _ = dealloc_fn.call(&mut *store, (ptr as i32, len as i32));

    serde_json::from_str::<Manifest>(&json)
        .map_err(|e| format!("parse Manifest JSON: {e}"))
}

// ─── WasmExtension ──────────────────────────────────────────────────────

pub struct WasmExtension {
    id:            String,
    capabilities:  Vec<ExtensionCapability>,
    inner:         SharedInner,
    tools:         Vec<ToolDefinition>,
    commands:      Vec<SlashCommand>,
    subscriptions: Vec<(ExtensionEvent, HookMode)>,
}

impl WasmExtension {
    /// 从 `.wasm` 文件加载扩展（s6r 协议）。
    ///
    /// `id` 和 `capabilities` 从 guest 的 `extension_manifest()` 返回值中读取，
    /// 不再由调用方传入。
    ///
    /// `invoker` 为 `host_invoke` 提供宿主能力调用接口。
    /// 传 `None` 则 guest 调用 `host_invoke` 时直接返回 0。
    pub fn load(
        path: &std::path::Path,
        fuel: u64,
        memory_bytes: usize,
        invoker: Option<HostInvoker>,
    ) -> Result<Arc<Self>, String> {
        // ── wasmtime 初始化 ──────────────────────────────────────────────
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine =
            wasmtime::Engine::new(&config).map_err(|e| format!("create wasm engine: {e}"))?;
        let module = wasmtime::Module::from_file(&engine, path)
            .map_err(|e| format!("compile wasm module: {e}"))?;

        let linker = wasm_api::create_linker(&engine)?;

        // manifest 阶段不注入 invoker，避免未声明能力在 manifest 内被调用。
        let host_state = HostState::new().with_limits(fuel, memory_bytes);
        let mut store = wasmtime::Store::new(&engine, host_state);
        store.limiter(|s: &mut HostState| -> &mut dyn wasmtime::ResourceLimiter { s });

        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| format!("instantiate wasm: {e}"))?;

        // ── 必须导出的函数 ───────────────────────────────────────────────
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or("wasm module must export 'memory'")?;

        let alloc_fn = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|e| format!("must export 'alloc': {e}"))?;

        let dealloc_fn = instance
            .get_typed_func::<(i32, i32), ()>(&mut store, "dealloc")
            .map_err(|e| format!("must export 'dealloc': {e}"))?;

        let call_fn = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "extension_call")
            .map_err(|e| format!("must export 'extension_call': {e}"))?;

        let manifest_fn = instance
            .get_typed_func::<(), i64>(&mut store, "extension_manifest")
            .map_err(|e| format!("must export 'extension_manifest': {e}"))?;

        // ── 调用 extension_manifest() 获取声明 ──────────────────────────
        store.set_fuel(fuel).map_err(|e| format!("set_fuel: {e}"))?;
        let packed = manifest_fn
            .call(&mut store, ())
            .map_err(|e| format!("extension_manifest trap: {e}"))?;

        let manifest = read_manifest(&mut store, &memory, &dealloc_fn, packed)?;

        // 校验 s6r 协议版本
        if manifest.s6r != s6r::S6R_VERSION {
            return Err(format!(
                "unsupported s6r version '{}', expected '{}'",
                manifest.s6r,
                s6r::S6R_VERSION,
            ));
        }

        let id = manifest.id.clone();
        if id.trim().is_empty() {
            return Err("s6r manifest id is empty".into());
        }

        let capabilities: Vec<ExtensionCapability> = manifest
            .capabilities
            .iter()
            .filter_map(|c| parse_capability(c))
            .collect();

        store
            .data_mut()
            .finish_manifest(capabilities.clone(), invoker);

        // ── 从 manifest 构建注册信息 ─────────────────────────────────────
        let tools: Vec<ToolDefinition> = manifest
            .tools
            .iter()
            .map(|t| ToolDefinition {
                name:           t.name.clone(),
                description:    t.description.clone(),
                parameters:     t.parameters.clone(),
                origin:         ToolOrigin::Extension,
                execution_mode: if t.mode == "parallel" {
                    ExecutionMode::Parallel
                } else {
                    ExecutionMode::Sequential
                },
            })
            .collect();

        let commands: Vec<SlashCommand> = manifest
            .commands
            .iter()
            .map(|c| SlashCommand {
                name:        c.name.clone(),
                description: c.description.clone(),
                args_schema: None,
            })
            .collect();

        let subscriptions: Vec<(ExtensionEvent, HookMode)> = manifest
            .hooks
            .iter()
            .filter_map(|h| {
                let event = s6r::event_from_name(&h.on)?;
                let mode  = s6r::mode_from_name(&h.mode)?;
                Some((event, mode))
            })
            .collect();

        let unknown_hooks: Vec<&str> = manifest
            .hooks
            .iter()
            .filter(|h| s6r::event_from_name(&h.on).is_none())
            .map(|h| h.on.as_str())
            .collect();
        if !unknown_hooks.is_empty() {
            tracing::warn!(
                extension_id = %id,
                ?unknown_hooks,
                "s6r manifest contains unknown hook event names, they will be ignored"
            );
        }

        Ok(Arc::new(Self {
            id,
            capabilities,
            inner: Arc::new(Mutex::new(WasmInner {
                store,
                memory,
                alloc_fn,
                dealloc_fn,
                call_fn,
            })),
            tools,
            commands,
            subscriptions,
        }))
    }
}

// ─── Extension trait ────────────────────────────────────────────────────

#[async_trait::async_trait]
impl Extension for WasmExtension {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> &[ExtensionCapability] {
        &self.capabilities
    }

    fn register(&self, reg: &mut Registrar) {
        for tool_def in &self.tools {
            reg.tool(
                tool_def.clone(),
                Arc::new(WasmToolHandler {
                    inner:        Arc::clone(&self.inner),
                    extension_id: self.id.clone(),
                }),
            );
        }

        for cmd in &self.commands {
            reg.command(
                cmd.clone(),
                Arc::new(WasmCommandHandler {
                            inner: Arc::clone(&self.inner),
                        }),
            );
        }

        for (event, mode) in &self.subscriptions {
            let inner = Arc::clone(&self.inner);
            match event {
                ExtensionEvent::PreToolUse => {
                    reg.on_pre_tool_use(*mode, 0, Arc::new(WasmPreToolUseHandler { inner }));
                },
                ExtensionEvent::PostToolUse => {
                    reg.on_post_tool_use(*mode, 0, Arc::new(WasmPostToolUseHandler { inner }));
                },
                ExtensionEvent::BeforeProviderRequest => {
                    reg.on_provider(
                        ProviderEvent::BeforeRequest,
                        *mode,
                        0,
                        Arc::new(WasmProviderHandler { inner, on: "before_provider_request".into() }),
                    );
                },
                ExtensionEvent::AfterProviderResponse => {
                    reg.on_provider(
                        ProviderEvent::AfterResponse,
                        *mode,
                        0,
                        Arc::new(WasmProviderHandler { inner, on: "after_provider_response".into() }),
                    );
                },
                ExtensionEvent::PromptBuild => {
                    reg.on_prompt_build(0, Arc::new(WasmPromptBuildHandler { inner }));
                },
                ExtensionEvent::PreCompact => {
                    reg.on_compact(
                        CompactEvent::PreCompact,
                        0,
                        Arc::new(WasmCompactHandler { inner, on: "pre_compact".into() }),
                    );
                },
                ExtensionEvent::PostCompact => {
                    reg.on_compact(
                        CompactEvent::PostCompact,
                        0,
                        Arc::new(WasmCompactHandler { inner, on: "post_compact".into() }),
                    );
                },
                other => {
                    let on = event_to_name(other).to_string();
                    reg.on_event(
                        other.clone(),
                        *mode,
                        0,
                        Arc::new(WasmLifecycleHandler { inner, on }),
                    );
                },
            }
        }
    }
}

// ─── Capability parser ──────────────────────────────────────────────────

fn parse_capability(name: &str) -> Option<ExtensionCapability> {
    match name {
        "session_state" => Some(ExtensionCapability::SessionState),
        "session_control" => Some(ExtensionCapability::SessionControl),
        "small_model" => Some(ExtensionCapability::SmallModel),
        "session_history" => Some(ExtensionCapability::SessionHistory),
        "emit_events" => Some(ExtensionCapability::EmitEvents),
        "workspace_read" => Some(ExtensionCapability::WorkspaceRead),
        "process_spawn" => Some(ExtensionCapability::ProcessSpawn),
        "network_client" => Some(ExtensionCapability::NetworkClient),
        _ => {
            tracing::warn!(capability = %name, "unknown s6r capability string, ignoring");
            None
        },
    }
}

// ─── context builders ────────────────────────────────────────────────────

fn build_pre_tool_use_context(ctx: &PreToolUseContext) -> serde_json::Value {
    json!({
        "session_id":      ctx.session_id,
        "working_dir":     ctx.working_dir,
        "model":           ctx.model,
        "tool_name":       ctx.tool_name,
        "tool_input":      ctx.tool_input,
        "available_tools": ctx.available_tools,
    })
}

fn build_post_tool_use_context(ctx: &PostToolUseContext) -> serde_json::Value {
    json!({
        "session_id":  ctx.session_id,
        "working_dir": ctx.working_dir,
        "model":       ctx.model,
        "tool_name":   ctx.tool_name,
        "tool_input":  ctx.tool_input,
        "tool_result": ctx.tool_result,
        "is_error":    ctx.is_error,
    })
}

fn build_provider_context(ctx: &ProviderContext) -> serde_json::Value {
    json!({
        "session_id":  ctx.session_id,
        "working_dir": ctx.working_dir,
        "model":       ctx.model,
        "messages":    ctx.messages,
    })
}

fn build_prompt_build_context(ctx: &PromptBuildContext) -> serde_json::Value {
    json!({
        "session_id":  ctx.session_id,
        "working_dir": ctx.working_dir,
        "model":       ctx.model,
    })
}

fn build_compact_context(ctx: &CompactContext) -> serde_json::Value {
    json!({
        "session_id":    ctx.session_id,
        "working_dir":   ctx.working_dir,
        "model":         ctx.model,
        "trigger":       ctx.trigger,
        "message_count": ctx.message_count,
        "pre_tokens":    ctx.pre_tokens,
        "post_tokens":   ctx.post_tokens,
        "summary":       ctx.summary,
    })
}

fn build_lifecycle_context(ctx: &LifecycleContext) -> serde_json::Value {
    json!({
        "session_id":  ctx.session_id,
        "working_dir": ctx.working_dir,
        "model":       ctx.model,
    })
}

// ─── result parsers ──────────────────────────────────────────────────────

fn parse_tool_result(
    resp: &CallResponse,
    _extension_id: &str,
) -> Result<ToolResult, ExtensionError> {
    if !resp.ok {
        let msg = resp.error.clone().unwrap_or_default();
        return Ok(ToolResult::text(msg, true, Default::default()));
    }
    match resp.effect() {
        "tool_outcome" => {
            let raw = resp
                .data_value("outcome")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let outcome: ExtensionToolOutcome = serde_json::from_value(raw)
                .map_err(|e| ExtensionError::Internal(format!("parse tool_outcome: {e}")))?;
            let outcome_json = serde_json::to_value(&outcome)
                .map_err(|e| ExtensionError::Internal(format!("serialize outcome: {e}")))?;
            Ok(ToolResult::text(
                String::new(),
                false,
                tool_metadata([(EXTENSION_TOOL_OUTCOME_KEY, outcome_json)]),
            ))
        },
        _ => {
            // effect = "ok" 或其他未知值：从 data.content 取文本，默认空串。
            let content = resp
                .data_value("content")
                .and_then(|v| v.as_str())
                .map(ToString::to_string)
                .unwrap_or_default();
            Ok(ToolResult::text(content, false, Default::default()))
        },
    }
}

fn parse_command_result(resp: &CallResponse) -> Result<ExtensionCommandResult, ExtensionError> {
    if !resp.ok {
        return Err(ExtensionError::Internal(
            resp.error.clone().unwrap_or_default(),
        ));
    }
    let data = resp.data.clone().unwrap_or(serde_json::json!({}));
    serde_json::from_value(data)
        .map_err(|e| ExtensionError::Internal(format!("parse command result: {e}")))
}

fn parse_pre_tool_use_result(resp: &CallResponse) -> Result<PreToolUseResult, ExtensionError> {
    if !resp.ok {
        return Ok(PreToolUseResult::Allow);
    }
    match resp.effect() {
        "block" => Ok(PreToolUseResult::Block {
            reason: resp.data_str("reason").to_string(),
        }),
        "modified_input" => {
            let tool_input = resp
                .data_value("tool_input")
                .cloned()
                .ok_or_else(|| {
                    ExtensionError::Internal(
                        "effect=modified_input but data.tool_input is missing".into(),
                    )
                })?;
            Ok(PreToolUseResult::ModifyInput { tool_input })
        },
        _ => Ok(PreToolUseResult::Allow),
    }
}

fn parse_post_tool_use_result(resp: &CallResponse) -> Result<PostToolUseResult, ExtensionError> {
    if !resp.ok {
        return Ok(PostToolUseResult::Allow);
    }
    match resp.effect() {
        "block" => Ok(PostToolUseResult::Block {
            reason: resp.data_str("reason").to_string(),
        }),
        "tool_outcome" => Ok(PostToolUseResult::ModifyResult {
            content: resp.data_str("content").to_string(),
        }),
        _ => Ok(PostToolUseResult::Allow),
    }
}

fn parse_provider_result(resp: &CallResponse) -> Result<ProviderResult, ExtensionError> {
    if !resp.ok {
        return Ok(ProviderResult::Allow);
    }
    match resp.effect() {
        "block" => Ok(ProviderResult::Block {
            reason: resp.data_str("reason").to_string(),
        }),
        "replace_messages" => {
            let messages_val = resp.data_value("messages").cloned().ok_or_else(|| {
                ExtensionError::Internal(
                    "effect=replace_messages but data.messages is missing".into(),
                )
            })?;
            Ok(ProviderResult::ReplaceMessages {
                messages: serde_json::from_value(messages_val)
                    .map_err(|e| ExtensionError::Internal(format!("parse messages: {e}")))?,
            })
        },
        "append_messages" => {
            let messages_val = resp.data_value("messages").cloned().ok_or_else(|| {
                ExtensionError::Internal(
                    "effect=append_messages but data.messages is missing".into(),
                )
            })?;
            Ok(ProviderResult::AppendMessages {
                messages: serde_json::from_value(messages_val)
                    .map_err(|e| ExtensionError::Internal(format!("parse messages: {e}")))?,
            })
        },
        _ => Ok(ProviderResult::Allow),
    }
}

fn parse_prompt_build_result(resp: &CallResponse) -> Result<PromptContributions, ExtensionError> {
    if !resp.ok || resp.effect() != "prompt_contributions" {
        return Ok(PromptContributions::default());
    }
    let data = resp.data.clone().unwrap_or_default();
    serde_json::from_value(data)
        .map_err(|e| ExtensionError::Internal(format!("parse PromptContributions: {e}")))
}

fn parse_compact_result(resp: &CallResponse) -> Result<CompactResult, ExtensionError> {
    if !resp.ok || resp.effect() != "compact_contributions" {
        return Ok(CompactResult::Allow);
    }
    let data = resp.data.clone().unwrap_or_default();
    let contributions: CompactContributions = serde_json::from_value(data)
        .map_err(|e| ExtensionError::Internal(format!("parse CompactContributions: {e}")))?;
    Ok(CompactResult::Contributions(contributions))
}

fn parse_lifecycle_result(resp: &CallResponse) -> Result<HookResult, ExtensionError> {
    if resp.ok {
        Ok(HookResult::Allow)
    } else {
        Ok(HookResult::Block {
            reason: resp.error.clone().unwrap_or_default(),
        })
    }
}

// ─── Tool handler ────────────────────────────────────────────────────────

struct WasmToolHandler {
    inner:        SharedInner,
    extension_id: String,
}

#[async_trait::async_trait]
impl ToolHandler for WasmToolHandler {
    async fn execute(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        working_dir: &str,
        ctx: &astrcode_extension_sdk::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        let req = CallRequest::Tool {
            id:    new_req_id(),
            name:  tool_name.to_string(),
            input: json!({
                "tool_name":    tool_name,
                "arguments":    arguments,
                "working_dir":  working_dir,
                "session_id":   ctx.session_id,
                "tool_call_id": ctx.tool_call_id,
            }),
        };
        let resp = call_guest(&self.inner, req).await?;
        parse_tool_result(&resp, &self.extension_id)
    }
}

// ─── Command handler ─────────────────────────────────────────────────────

struct WasmCommandHandler {
    inner: SharedInner,
}

#[async_trait::async_trait]
impl CommandHandler for WasmCommandHandler {
    async fn execute(
        &self,
        command_name: &str,
        arguments: &str,
        working_dir: &str,
        ctx: &CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        let req = CallRequest::Command {
            id:    new_req_id(),
            name:  command_name.to_string(),
            input: json!({
                "command_name": command_name,
                "arguments":    arguments,
                "working_dir":  working_dir,
                "session_id":   ctx.session_id,
                "model":        ctx.model,
            }),
        };
        let resp = call_guest(&self.inner, req).await?;
        parse_command_result(&resp)
    }
}

// ─── Hook handlers ───────────────────────────────────────────────────────

struct WasmPreToolUseHandler { inner: SharedInner }

#[async_trait::async_trait]
impl PreToolUseHandler for WasmPreToolUseHandler {
    async fn handle(&self, ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError> {
        let req = CallRequest::Hook {
            id:    new_req_id(),
            on:    "pre_tool_use".into(),
            input: build_pre_tool_use_context(&ctx),
        };
        let resp = call_guest(&self.inner, req).await?;
        parse_pre_tool_use_result(&resp)
    }
}

struct WasmPostToolUseHandler { inner: SharedInner }

#[async_trait::async_trait]
impl PostToolUseHandler for WasmPostToolUseHandler {
    async fn handle(&self, ctx: PostToolUseContext) -> Result<PostToolUseResult, ExtensionError> {
        let req = CallRequest::Hook {
            id:    new_req_id(),
            on:    "post_tool_use".into(),
            input: build_post_tool_use_context(&ctx),
        };
        let resp = call_guest(&self.inner, req).await?;
        parse_post_tool_use_result(&resp)
    }
}

struct WasmProviderHandler { inner: SharedInner, on: String }

#[async_trait::async_trait]
impl ProviderHandler for WasmProviderHandler {
    async fn handle(&self, ctx: ProviderContext) -> Result<ProviderResult, ExtensionError> {
        let req = CallRequest::Hook {
            id:    new_req_id(),
            on:    self.on.clone(),
            input: build_provider_context(&ctx),
        };
        let resp = call_guest(&self.inner, req).await?;
        parse_provider_result(&resp)
    }
}

struct WasmPromptBuildHandler { inner: SharedInner }

#[async_trait::async_trait]
impl PromptBuildHandler for WasmPromptBuildHandler {
    async fn handle(&self, ctx: PromptBuildContext) -> Result<PromptContributions, ExtensionError> {
        let req = CallRequest::Hook {
            id:    new_req_id(),
            on:    "prompt_build".into(),
            input: build_prompt_build_context(&ctx),
        };
        let resp = call_guest(&self.inner, req).await?;
        parse_prompt_build_result(&resp)
    }
}

struct WasmCompactHandler { inner: SharedInner, on: String }

#[async_trait::async_trait]
impl CompactHandler for WasmCompactHandler {
    async fn handle(&self, ctx: CompactContext) -> Result<CompactResult, ExtensionError> {
        let req = CallRequest::Hook {
            id:    new_req_id(),
            on:    self.on.clone(),
            input: build_compact_context(&ctx),
        };
        let resp = call_guest(&self.inner, req).await?;
        parse_compact_result(&resp)
    }
}

struct WasmLifecycleHandler { inner: SharedInner, on: String }

#[async_trait::async_trait]
impl LifecycleHandler for WasmLifecycleHandler {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        let req = CallRequest::Hook {
            id:    new_req_id(),
            on:    self.on.clone(),
            input: build_lifecycle_context(&ctx),
        };
        let resp = call_guest(&self.inner, req).await?;
        parse_lifecycle_result(&resp)
    }
}
