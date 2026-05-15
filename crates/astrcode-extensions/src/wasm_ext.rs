//! WASM 扩展适配器 — 将 wasmtime 实例包装为 Extension trait 实现。
//!
//! 加载 `.wasm` 文件，调用 `extension_init()` 让插件注册能力，
//! 然后通过 handler adapter 将宿主的 handler trait 调用翻译为
//! WASM guest 函数调用。

use std::sync::Arc;

use astrcode_core::{
    extension::{
        CommandContext, CommandHandler, CompactContext, CompactEvent, CompactHandler,
        CompactResult, EXTENSION_TOOL_OUTCOME_KEY, Extension, ExtensionCommandResult,
        ExtensionError, ExtensionEvent, ExtensionToolOutcome, HookMode, HookResult,
        LifecycleContext, LifecycleHandler, PostToolUseContext, PostToolUseHandler,
        PostToolUseResult, PreToolUseContext, PreToolUseHandler, PreToolUseResult,
        PromptBuildContext, PromptBuildHandler, PromptContributions, ProviderContext,
        ProviderEvent, ProviderHandler, ProviderResult, Registrar, SlashCommand, ToolHandler,
    },
    tool::{ToolDefinition, ToolResult, tool_metadata},
};
use parking_lot::Mutex;
use serde_json::json;

use crate::wasm_api::{self, HostState};

// ─── Shared WASM runtime state ──────────────────────────────────────────

/// 持有 wasmtime 运行时状态。所有字段需要同时访问（Store 必须 &mut 才能调用函数）。
struct WasmInner {
    store: wasmtime::Store<HostState>,
    memory: wasmtime::Memory,
    alloc_fn: wasmtime::TypedFunc<i32, i32>,
    handle_tool_fn: Option<wasmtime::TypedFunc<(i32, i32), i32>>,
    handle_command_fn: Option<wasmtime::TypedFunc<(i32, i32), i32>>,
    handle_event_fn: Option<wasmtime::TypedFunc<(i32, i32), i32>>,
}

type SharedInner = Arc<Mutex<WasmInner>>;

fn call_guest(
    inner: &Mutex<WasmInner>,
    func: &wasmtime::TypedFunc<(i32, i32), i32>,
    request_json: &str,
) -> Result<(i8, String), ExtensionError> {
    let mut guard = inner.lock();
    let request_bytes = request_json.as_bytes();

    let memory = guard.memory;
    let alloc_fn = guard.alloc_fn.clone();

    let (ptr, len) = wasm_api::write_to_guest(&mut guard.store, &memory, &alloc_fn, request_bytes)
        .map_err(ExtensionError::Internal)?;

    let status = func
        .call(&mut guard.store, (ptr as i32, len as i32))
        .map_err(|e| ExtensionError::Internal(format!("wasm trap: {e}")))?;

    let response = wasm_api::take_response(&guard.store, &memory);
    guard.store.data_mut().response_ptr = 0;
    guard.store.data_mut().response_len = 0;

    Ok((status as i8, response))
}

// ─── WasmExtension ──────────────────────────────────────────────────────

/// 加载后的 WASM 扩展。
pub struct WasmExtension {
    id: String,
    inner: SharedInner,
    tools: Vec<ToolDefinition>,
    commands: Vec<SlashCommand>,
    subscriptions: Vec<(ExtensionEvent, HookMode)>,
}

impl WasmExtension {
    /// 从 `.wasm` 文件加载扩展。
    pub fn load(path: &std::path::Path, id: String) -> Result<Arc<Self>, String> {
        let engine = wasmtime::Engine::default();
        let module = wasmtime::Module::from_file(&engine, path)
            .map_err(|e| format!("compile wasm module: {e}"))?;

        let linker = wasm_api::create_linker(&engine)?;

        let mut store = wasmtime::Store::new(&engine, HostState::new());
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| format!("instantiate wasm: {e}"))?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| "wasm module must export 'memory'".to_string())?;

        let alloc_fn = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|e| format!("wasm module must export 'alloc': {e}"))?;

        let handle_tool_fn = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "handle_tool")
            .ok();
        let handle_command_fn = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "handle_command")
            .ok();
        let handle_event_fn = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "handle_event")
            .ok();

        if let Ok(init_fn) = instance.get_typed_func::<(), ()>(&mut store, "extension_init") {
            init_fn
                .call(&mut store, ())
                .map_err(|e| format!("extension_init trap: {e}"))?;
        }

        let state = store.data();
        let tools = state.tools.clone();
        let commands = state.commands.clone();
        let subscriptions = state.subscriptions.clone();

        Ok(Arc::new(Self {
            id,
            inner: Arc::new(Mutex::new(WasmInner {
                store,
                memory,
                alloc_fn,
                handle_tool_fn,
                handle_command_fn,
                handle_event_fn,
            })),
            tools,
            commands,
            subscriptions,
        }))
    }
}

// ─── Extension trait impl ───────────────────────────────────────────────

#[async_trait::async_trait]
impl Extension for WasmExtension {
    fn id(&self) -> &str {
        &self.id
    }

    fn register(&self, reg: &mut Registrar) {
        let inner = Arc::clone(&self.inner);

        let has_tool_handler = inner.lock().handle_tool_fn.is_some();
        let has_command_handler = inner.lock().handle_command_fn.is_some();
        let has_event_handler = inner.lock().handle_event_fn.is_some();

        for tool_def in &self.tools {
            if !has_tool_handler {
                tracing::warn!(
                    extension_id = %self.id,
                    tool_name = %tool_def.name,
                    "tool registered but handle_tool not exported, skipping"
                );
                continue;
            }
            reg.tool(
                tool_def.clone(),
                Arc::new(WasmToolHandler {
                    inner: Arc::clone(&inner),
                    extension_id: self.id.clone(),
                }),
            );
        }

        for cmd in &self.commands {
            if !has_command_handler {
                continue;
            }
            reg.command(
                cmd.clone(),
                Arc::new(WasmCommandHandler {
                    inner: Arc::clone(&inner),
                    extension_id: self.id.clone(),
                }),
            );
        }

        for (event, mode) in &self.subscriptions {
            if !has_event_handler {
                continue;
            }
            match event {
                ExtensionEvent::PreToolUse => {
                    reg.on_pre_tool_use(
                        *mode,
                        0,
                        Arc::new(WasmPreToolUseHandler {
                            inner: Arc::clone(&inner),
                        }),
                    );
                },
                ExtensionEvent::PostToolUse => {
                    reg.on_post_tool_use(
                        *mode,
                        0,
                        Arc::new(WasmPostToolUseHandler {
                            inner: Arc::clone(&inner),
                        }),
                    );
                },
                ExtensionEvent::BeforeProviderRequest | ExtensionEvent::AfterProviderResponse => {
                    reg.on_provider(
                        if event == &ExtensionEvent::BeforeProviderRequest {
                            ProviderEvent::BeforeRequest
                        } else {
                            ProviderEvent::AfterResponse
                        },
                        *mode,
                        0,
                        Arc::new(WasmProviderHandler {
                            inner: Arc::clone(&inner),
                        }),
                    );
                },
                ExtensionEvent::PromptBuild => {
                    reg.on_prompt_build(
                        0,
                        Arc::new(WasmPromptBuildHandler {
                            inner: Arc::clone(&inner),
                        }),
                    );
                },
                ExtensionEvent::PreCompact | ExtensionEvent::PostCompact => {
                    reg.on_compact(
                        if event == &ExtensionEvent::PreCompact {
                            CompactEvent::PreCompact
                        } else {
                            CompactEvent::PostCompact
                        },
                        0,
                        Arc::new(WasmCompactHandler {
                            inner: Arc::clone(&inner),
                        }),
                    );
                },
                _ => {
                    reg.on_event(
                        event.clone(),
                        *mode,
                        0,
                        Arc::new(WasmLifecycleHandler {
                            inner: Arc::clone(&inner),
                        }),
                    );
                },
            }
        }
    }
}

// ─── Handler Adapters ───────────────────────────────────────────────────

struct WasmToolHandler {
    inner: SharedInner,
    extension_id: String,
}

#[async_trait::async_trait]
impl ToolHandler for WasmToolHandler {
    async fn execute(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        working_dir: &str,
        ctx: &astrcode_core::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        let inner = self.inner.lock();
        let Some(func) = &inner.handle_tool_fn else {
            return Err(ExtensionError::NotFound(tool_name.into()));
        };
        let func = func.clone();
        drop(inner);

        let request = json!({
            "tool_name": tool_name,
            "arguments": arguments,
            "working_dir": working_dir,
            "session_id": ctx.session_id,
            "tool_call_id": ctx.tool_call_id,
        });

        let (status, response) = call_guest(&self.inner, &func, &request.to_string())?;

        match status {
            0 => {
                let content = if response.is_empty() {
                    String::new()
                } else {
                    serde_json::from_str::<serde_json::Value>(&response)
                        .map(|v| v["content"].as_str().unwrap_or(&response).to_string())
                        .unwrap_or(response)
                };
                Ok(ToolResult::text(content, false, Default::default()))
            },
            1 => Ok(ToolResult::text(response.clone(), true, Default::default())),
            2 => {
                let outcome: ExtensionToolOutcome = serde_json::from_str(&response)
                    .map_err(|e| ExtensionError::Internal(format!("parse outcome: {e}")))?;
                let outcome_json = serde_json::to_value(&outcome)
                    .map_err(|e| ExtensionError::Internal(format!("serialize outcome: {e}")))?;
                Ok(ToolResult::text(
                    String::new(),
                    false,
                    tool_metadata([(EXTENSION_TOOL_OUTCOME_KEY, outcome_json)]),
                ))
            },
            other => Err(ExtensionError::Internal(format!(
                "extension {} tool handler unknown status: {other}",
                self.extension_id
            ))),
        }
    }
}

struct WasmCommandHandler {
    inner: SharedInner,
    extension_id: String,
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
        let inner = self.inner.lock();
        let Some(func) = &inner.handle_command_fn else {
            return Err(ExtensionError::NotFound(command_name.into()));
        };
        let func = func.clone();
        drop(inner);

        let request = json!({
            "command_name": command_name,
            "arguments": arguments,
            "working_dir": working_dir,
            "session_id": ctx.session_id,
            "model": ctx.model,
        });

        let (status, response) = call_guest(&self.inner, &func, &request.to_string())?;

        match status {
            0 => serde_json::from_str(&response)
                .map_err(|e| ExtensionError::Internal(format!("parse command result: {e}"))),
            1 => Err(ExtensionError::Internal(response)),
            other => Err(ExtensionError::Internal(format!(
                "extension {} command handler unknown status: {other}",
                self.extension_id
            ))),
        }
    }
}

// ─── Event Handler Adapters ─────────────────────────────────────────────

struct WasmPreToolUseHandler {
    inner: SharedInner,
}

#[async_trait::async_trait]
impl PreToolUseHandler for WasmPreToolUseHandler {
    async fn handle(&self, ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError> {
        let inner = self.inner.lock();
        let Some(func) = &inner.handle_event_fn else {
            return Ok(PreToolUseResult::Allow);
        };
        let func = func.clone();
        drop(inner);

        let request = json!({
            "event": "PreToolUse",
            "context": {
                "session_id": ctx.session_id,
                "working_dir": ctx.working_dir,
                "model": ctx.model,
                "tool_name": ctx.tool_name,
                "tool_input": ctx.tool_input,
                "available_tools": ctx.available_tools,
            },
        });

        let (effect, content) = call_guest(&self.inner, &func, &request.to_string())?;

        match effect {
            1 => Ok(PreToolUseResult::Block { reason: content }),
            3 => {
                let tool_input = serde_json::from_str(&content)
                    .map_err(|e| ExtensionError::Internal(format!("invalid ModifiedInput: {e}")))?;
                Ok(PreToolUseResult::ModifyInput { tool_input })
            },
            _ => Ok(PreToolUseResult::Allow),
        }
    }
}

struct WasmPostToolUseHandler {
    inner: SharedInner,
}

#[async_trait::async_trait]
impl PostToolUseHandler for WasmPostToolUseHandler {
    async fn handle(&self, ctx: PostToolUseContext) -> Result<PostToolUseResult, ExtensionError> {
        let inner = self.inner.lock();
        let Some(func) = &inner.handle_event_fn else {
            return Ok(PostToolUseResult::Allow);
        };
        let func = func.clone();
        drop(inner);

        let request = json!({
            "event": "PostToolUse",
            "context": {
                "session_id": ctx.session_id,
                "working_dir": ctx.working_dir,
                "model": ctx.model,
                "tool_name": ctx.tool_name,
                "tool_input": ctx.tool_input,
                "tool_result": ctx.tool_result,
                "is_error": ctx.is_error,
            },
        });

        let (effect, content) = call_guest(&self.inner, &func, &request.to_string())?;

        match effect {
            1 => Ok(PostToolUseResult::Block { reason: content }),
            2 => Ok(PostToolUseResult::ModifyResult { content }),
            _ => Ok(PostToolUseResult::Allow),
        }
    }
}

struct WasmProviderHandler {
    inner: SharedInner,
}

#[async_trait::async_trait]
impl ProviderHandler for WasmProviderHandler {
    async fn handle(&self, ctx: ProviderContext) -> Result<ProviderResult, ExtensionError> {
        let inner = self.inner.lock();
        let Some(func) = &inner.handle_event_fn else {
            return Ok(ProviderResult::Allow);
        };
        let func = func.clone();
        drop(inner);

        let request = json!({
            "event": "Provider",
            "context": {
                "session_id": ctx.session_id,
                "working_dir": ctx.working_dir,
                "model": ctx.model,
                "messages": ctx.messages,
            },
        });

        let (effect, content) = call_guest(&self.inner, &func, &request.to_string())?;

        match effect {
            1 => Ok(ProviderResult::Block { reason: content }),
            6 => {
                let messages = serde_json::from_str(&content).map_err(|e| {
                    ExtensionError::Internal(format!("invalid ReplaceMessages: {e}"))
                })?;
                Ok(ProviderResult::ReplaceMessages { messages })
            },
            7 => {
                let messages = serde_json::from_str(&content).map_err(|e| {
                    ExtensionError::Internal(format!("invalid AppendMessages: {e}"))
                })?;
                Ok(ProviderResult::AppendMessages { messages })
            },
            _ => Ok(ProviderResult::Allow),
        }
    }
}

struct WasmPromptBuildHandler {
    inner: SharedInner,
}

#[async_trait::async_trait]
impl PromptBuildHandler for WasmPromptBuildHandler {
    async fn handle(&self, ctx: PromptBuildContext) -> Result<PromptContributions, ExtensionError> {
        let inner = self.inner.lock();
        let Some(func) = &inner.handle_event_fn else {
            return Ok(PromptContributions::default());
        };
        let func = func.clone();
        drop(inner);

        let request = json!({
            "event": "PromptBuild",
            "context": {
                "session_id": ctx.session_id,
                "working_dir": ctx.working_dir,
                "model": ctx.model,
            },
        });

        let (effect, content) = call_guest(&self.inner, &func, &request.to_string())?;

        if effect == 4 {
            serde_json::from_str(&content)
                .map_err(|e| ExtensionError::Internal(format!("invalid PromptContributions: {e}")))
        } else {
            Ok(PromptContributions::default())
        }
    }
}

struct WasmCompactHandler {
    inner: SharedInner,
}

#[async_trait::async_trait]
impl CompactHandler for WasmCompactHandler {
    async fn handle(&self, ctx: CompactContext) -> Result<CompactResult, ExtensionError> {
        let inner = self.inner.lock();
        let Some(func) = &inner.handle_event_fn else {
            return Ok(CompactResult::Allow);
        };
        let func = func.clone();
        drop(inner);

        let request = json!({
            "event": "Compact",
            "context": {
                "session_id": ctx.session_id,
                "working_dir": ctx.working_dir,
                "model": ctx.model,
                "trigger": ctx.trigger,
                "message_count": ctx.message_count,
                "pre_tokens": ctx.pre_tokens,
                "post_tokens": ctx.post_tokens,
                "summary": ctx.summary,
            },
        });

        let (effect, content) = call_guest(&self.inner, &func, &request.to_string())?;

        if effect == 5 {
            let contributions = serde_json::from_str(&content).map_err(|e| {
                ExtensionError::Internal(format!("invalid CompactContributions: {e}"))
            })?;
            Ok(CompactResult::Contributions(contributions))
        } else {
            Ok(CompactResult::Allow)
        }
    }
}

struct WasmLifecycleHandler {
    inner: SharedInner,
}

#[async_trait::async_trait]
impl LifecycleHandler for WasmLifecycleHandler {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        let inner = self.inner.lock();
        let Some(func) = &inner.handle_event_fn else {
            return Ok(HookResult::Allow);
        };
        let func = func.clone();
        drop(inner);

        let request = json!({
            "event": "Lifecycle",
            "context": {
                "session_id": ctx.session_id,
                "working_dir": ctx.working_dir,
                "model": ctx.model,
            },
        });

        let (effect, content) = call_guest(&self.inner, &func, &request.to_string())?;

        if effect == 1 {
            Ok(HookResult::Block { reason: content })
        } else {
            Ok(HookResult::Allow)
        }
    }
}
