//! 原生扩展适配器 — 封装 `libloading::Library` + 已注册的处理器。
//!
//! 每个 `.dll`/`.so` 扩展通过 `libloading` 加载，其 `extension_factory` 符号
//! 被调用时传入一个 `ExtensionApi` vtable，工厂函数借此注册事件处理器、
//! 工具和命令。
//!
//! 本结构体实现了 `Extension` trait，通过 `register()` 将 FFI 回调包装为
//! 类型化 handler 注册到 Registrar。

use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    extension::{
        CommandContext, CommandHandler, CompactContext, CompactContributions, CompactEvent,
        CompactHandler, CompactResult, Extension, ExtensionCommandResult, ExtensionError,
        ExtensionEvent, ExtensionToolOutcome, HookMode, HookResult, LifecycleContext,
        LifecycleHandler, PostToolUseContext, PostToolUseHandler, PostToolUseResult,
        PreToolUseContext, PreToolUseHandler, PreToolUseResult, PromptBuildContext,
        PromptBuildHandler, PromptContributions, ProviderContext, ProviderEvent, ProviderHandler,
        ProviderResult, Registrar, ToolHandler,
    },
    tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult},
};
use parking_lot::Mutex;

use crate::ffi::{
    self, CommandCallback, EventCallback, ExtensionApi, FfiCtxOwned, OutputFreeCallback,
    ToolCallback,
};

/// 从 FFI 输出指针读取字符串并释放，处理空指针情况。
fn read_and_release(ptr: *const u8, len: u32, output_free: Option<OutputFreeCallback>) -> String {
    let text = if ptr.is_null() || len == 0 {
        String::new()
    } else {
        unsafe { ffi::read_ffi_str(ptr, len) }
    };
    release_output(ptr, len, output_free);
    text
}

/// 释放 FFI 输出指针指向的内存。
fn release_output(ptr: *const u8, len: u32, output_free: Option<OutputFreeCallback>) {
    if ptr.is_null() || len == 0 {
        return;
    }
    if let Some(callback) = output_free {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            callback(ptr, len)
        }));
    }
}

/// 调用 FFI 事件回调并返回 (effect_code, output_string)。
fn call_ffi_event(
    callback: EventCallback,
    event_disc: u8,
    ffi_ctx: &FfiCtxOwned,
    output_free: Option<OutputFreeCallback>,
) -> (u8, String) {
    let mut effect_out: u8 = 0;
    let mut output_ptr: *const u8 = std::ptr::null();
    let mut output_len: u32 = 0;

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        (callback)(
            event_disc,
            ffi_ctx.as_ptr(),
            &mut effect_out,
            &mut output_ptr,
            &mut output_len,
        )
    }));

    match result {
        Ok(()) => {
            let content = read_and_release(output_ptr, output_len, output_free);
            (effect_out, content)
        },
        Err(_) => {
            release_output(output_ptr, output_len, output_free);
            tracing::warn!("FFI event callback panicked");
            (0, String::new())
        },
    }
}

// ─── FFI Handler Adapters ──────────────────────────────────────────────

/// FFI 工具执行处理器。
struct FfiToolHandler {
    callback: ToolCallback,
    output_free: Option<OutputFreeCallback>,
    extension_id: String,
}

#[async_trait::async_trait]
impl ToolHandler for FfiToolHandler {
    async fn execute(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        working_dir: &str,
        ctx: &astrcode_core::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        let ffi_ctx = FfiCtxOwned::from_tool_execution(working_dir, tool_name, &arguments, ctx);
        let mut output_ptr: *const u8 = std::ptr::null();
        let mut output_len: u32 = 0;
        let mut error_ptr: *const u8 = std::ptr::null();
        let mut error_len: u32 = 0;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            (self.callback)(
                ffi_ctx.as_ptr(),
                &mut output_ptr,
                &mut output_len,
                &mut error_ptr,
                &mut error_len,
            )
        }));

        let status = match result {
            Ok(status) => status,
            Err(_) => {
                release_output(output_ptr, output_len, self.output_free);
                release_output(error_ptr, error_len, self.output_free);
                return Err(ExtensionError::Internal(format!(
                    "extension {} tool handler panicked for {tool_name}",
                    self.extension_id
                )));
            },
        };

        let parsed_outcome = unsafe {
            ffi::parse_tool_outcome(status, output_ptr, output_len, error_ptr, error_len)
        };
        release_output(output_ptr, output_len, self.output_free);
        release_output(error_ptr, error_len, self.output_free);
        let outcome = parsed_outcome.map_err(|e| {
            ExtensionError::Internal(format!(
                "extension {} tool handler invalid result: {e}",
                self.extension_id
            ))
        })?;

        match outcome {
            ExtensionToolOutcome::Text { content, is_error } => Ok(ToolResult {
                call_id: ctx.tool_call_id.clone().unwrap_or_default(),
                content: content.clone(),
                is_error,
                error: if is_error { Some(content) } else { None },
                metadata: Default::default(),
                duration_ms: None,
            }),
            ext_outcome @ ExtensionToolOutcome::RunSession { .. } => {
                let outcome_json = serde_json::to_value(&ext_outcome).map_err(|e| {
                    ExtensionError::Internal(format!(
                        "extension {} serializing outcome: {e}",
                        self.extension_id
                    ))
                })?;
                let mut metadata = std::collections::BTreeMap::new();
                metadata.insert("extension_tool_outcome".into(), outcome_json);
                Ok(ToolResult {
                    call_id: ctx.tool_call_id.clone().unwrap_or_default(),
                    content: String::new(),
                    is_error: false,
                    error: None,
                    metadata,
                    duration_ms: None,
                })
            },
        }
    }
}

/// FFI 斜杠命令处理器。
struct FfiCommandHandler {
    callback: CommandCallback,
    output_free: Option<OutputFreeCallback>,
    extension_id: String,
}

#[async_trait::async_trait]
impl CommandHandler for FfiCommandHandler {
    async fn execute(
        &self,
        command_name: &str,
        arguments: &str,
        working_dir: &str,
        ctx: &CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        let ffi_ctx = FfiCtxOwned::new(
            ctx.session_id.clone(),
            working_dir.to_string(),
            command_name.to_string(),
            String::new(),
            ctx.model.model.clone(),
            String::new(),
            String::new(),
            String::new(),
        );

        let args_bytes = arguments.as_bytes();
        let mut output_ptr: *const u8 = std::ptr::null();
        let mut output_len: u32 = 0;
        let mut error_ptr: *const u8 = std::ptr::null();
        let mut error_len: u32 = 0;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            (self.callback)(
                ffi_ctx.as_ptr(),
                args_bytes.as_ptr(),
                args_bytes.len() as u32,
                &mut output_ptr,
                &mut output_len,
                &mut error_ptr,
                &mut error_len,
            )
        }));

        let status = match result {
            Ok(status) => status,
            Err(_) => {
                release_output(output_ptr, output_len, self.output_free);
                release_output(error_ptr, error_len, self.output_free);
                return Err(ExtensionError::Internal(format!(
                    "extension {} command handler panicked for {command_name}",
                    self.extension_id
                )));
            },
        };

        match status {
            0 => {
                let json = read_and_release(output_ptr, output_len, self.output_free);
                release_output(error_ptr, error_len, self.output_free);
                serde_json::from_str(&json).map_err(|e| {
                    ExtensionError::Internal(format!(
                        "extension {} command handler invalid result: {e}",
                        self.extension_id
                    ))
                })
            },
            1 => {
                let error = read_and_release(error_ptr, error_len, self.output_free);
                release_output(output_ptr, output_len, self.output_free);
                Err(ExtensionError::Internal(error))
            },
            other => {
                release_output(output_ptr, output_len, self.output_free);
                release_output(error_ptr, error_len, self.output_free);
                Err(ExtensionError::Internal(format!(
                    "extension {} command handler unknown return code: {other}",
                    self.extension_id
                )))
            },
        }
    }
}

/// FFI PreToolUse 钩子处理器。
struct FfiPreToolUseHandler {
    callback: EventCallback,
    event_disc: u8,
    output_free: Option<OutputFreeCallback>,
}

#[async_trait::async_trait]
impl PreToolUseHandler for FfiPreToolUseHandler {
    async fn handle(&self, ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError> {
        let tools_json = serde_json::to_string(&ctx.available_tools).unwrap_or_default();
        let ffi_ctx = FfiCtxOwned::new(
            ctx.session_id,
            ctx.working_dir,
            ctx.tool_name,
            ctx.tool_input.to_string(),
            ctx.model.model,
            tools_json,
            String::new(),
            String::new(),
        );
        let (effect, content) =
            call_ffi_event(self.callback, self.event_disc, &ffi_ctx, self.output_free);
        match effect {
            1 => Ok(PreToolUseResult::Block { reason: content }),
            3 => {
                let tool_input = serde_json::from_str(&content).map_err(|e| {
                    ExtensionError::Internal(format!("FFI PreToolUse invalid ModifiedInput: {e}"))
                })?;
                Ok(PreToolUseResult::ModifyInput { tool_input })
            },
            _ => Ok(PreToolUseResult::Allow),
        }
    }
}

/// FFI PostToolUse 钩子处理器。
struct FfiPostToolUseHandler {
    callback: EventCallback,
    event_disc: u8,
    output_free: Option<OutputFreeCallback>,
}

#[async_trait::async_trait]
impl PostToolUseHandler for FfiPostToolUseHandler {
    async fn handle(&self, ctx: PostToolUseContext) -> Result<PostToolUseResult, ExtensionError> {
        let tool_result_json = serde_json::to_string(&ctx.tool_result).unwrap_or_default();
        let ffi_ctx = FfiCtxOwned::new(
            ctx.session_id,
            ctx.working_dir,
            ctx.tool_name,
            ctx.tool_input.to_string(),
            ctx.model.model,
            String::new(),
            tool_result_json,
            String::new(),
        );
        let (effect, content) =
            call_ffi_event(self.callback, self.event_disc, &ffi_ctx, self.output_free);
        match effect {
            1 => Ok(PostToolUseResult::Block { reason: content }),
            2 => Ok(PostToolUseResult::ModifyResult { content }),
            _ => Ok(PostToolUseResult::Allow),
        }
    }
}

/// FFI Provider 钩子处理器。
/// TODO: 当前仅支持 Block / Allow，未支持 ReplaceMessages / AppendMessages。
///       需要时需扩展 FfiCtxOwned 传入 messages JSON，并新增 effect code 映射。
struct FfiProviderHandler {
    callback: EventCallback,
    event_disc: u8,
    output_free: Option<OutputFreeCallback>,
}

#[async_trait::async_trait]
impl ProviderHandler for FfiProviderHandler {
    async fn handle(&self, ctx: ProviderContext) -> Result<ProviderResult, ExtensionError> {
        let ffi_ctx = FfiCtxOwned::new(
            ctx.session_id,
            ctx.working_dir,
            String::new(),
            String::new(),
            ctx.model.model,
            String::new(),
            String::new(),
            String::new(),
        );
        let (effect, content) =
            call_ffi_event(self.callback, self.event_disc, &ffi_ctx, self.output_free);
        match effect {
            1 => Ok(ProviderResult::Block { reason: content }),
            _ => Ok(ProviderResult::Allow),
        }
    }
}

/// FFI PromptBuild 钩子处理器。
struct FfiPromptBuildHandler {
    callback: EventCallback,
    event_disc: u8,
    output_free: Option<OutputFreeCallback>,
}

#[async_trait::async_trait]
impl PromptBuildHandler for FfiPromptBuildHandler {
    async fn handle(&self, ctx: PromptBuildContext) -> Result<PromptContributions, ExtensionError> {
        let ffi_ctx = FfiCtxOwned::new(
            ctx.session_id,
            ctx.working_dir,
            String::new(),
            String::new(),
            ctx.model.model,
            String::new(),
            String::new(),
            String::new(),
        );
        let (effect, content) =
            call_ffi_event(self.callback, self.event_disc, &ffi_ctx, self.output_free);
        if effect == 4 {
            serde_json::from_str(&content).map_err(|e| {
                ExtensionError::Internal(format!("FFI PromptBuild invalid contributions: {e}"))
            })
        } else {
            Ok(PromptContributions::default())
        }
    }
}

/// FFI Compact 钩子处理器。
struct FfiCompactHandler {
    callback: EventCallback,
    event_disc: u8,
    output_free: Option<OutputFreeCallback>,
}

#[async_trait::async_trait]
impl CompactHandler for FfiCompactHandler {
    async fn handle(&self, ctx: CompactContext) -> Result<CompactResult, ExtensionError> {
        let event_context_json = serde_json::json!({
            "trigger": ctx.trigger,
            "message_count": ctx.message_count,
            "pre_tokens": ctx.pre_tokens,
            "post_tokens": ctx.post_tokens,
            "summary": ctx.summary,
        })
        .to_string();
        let ffi_ctx = FfiCtxOwned::new(
            ctx.session_id,
            ctx.working_dir,
            String::new(),
            String::new(),
            ctx.model.model,
            String::new(),
            String::new(),
            event_context_json,
        );
        let (effect, content) =
            call_ffi_event(self.callback, self.event_disc, &ffi_ctx, self.output_free);
        if effect == 5 {
            let contributions: CompactContributions =
                serde_json::from_str(&content).map_err(|e| {
                    ExtensionError::Internal(format!("FFI Compact invalid contributions: {e}"))
                })?;
            Ok(CompactResult::Contributions(contributions))
        } else {
            Ok(CompactResult::Allow)
        }
    }
}

/// FFI 通用生命周期钩子处理器。
struct FfiLifecycleHandler {
    callback: EventCallback,
    event_disc: u8,
    output_free: Option<OutputFreeCallback>,
}

#[async_trait::async_trait]
impl LifecycleHandler for FfiLifecycleHandler {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        let ffi_ctx = FfiCtxOwned::new(
            ctx.session_id,
            ctx.working_dir,
            String::new(),
            String::new(),
            ctx.model.model,
            String::new(),
            String::new(),
            String::new(),
        );
        let (effect, content) =
            call_ffi_event(self.callback, self.event_disc, &ffi_ctx, self.output_free);
        if effect == 1 {
            Ok(HookResult::Block { reason: content })
        } else {
            Ok(HookResult::Allow)
        }
    }
}

// ─── NativeExtension ─────────────────────────────────────────────────────

/// 已加载的原生扩展。
///
/// `Library` 在扩展注册期间保持存活。当被 drop 时，库被卸载。
pub struct NativeExtension {
    /// 扩展唯一标识
    id: String,
    /// 底层动态库句柄，保持库在内存中
    _library: libloading::Library,
    /// 已注册的事件处理器列表: (事件类型, 钩子模式, 回调函数)
    handlers: Arc<Mutex<Vec<(ExtensionEvent, HookMode, EventCallback)>>>,
    /// 已注册的工具定义列表
    tools: Arc<Mutex<Vec<ToolDefinition>>>,
    /// 工具名称到回调函数的映射
    tool_handlers: Arc<Mutex<HashMap<String, ToolCallback>>>,
    /// 已注册的斜杠命令列表
    commands: Arc<Mutex<Vec<astrcode_core::extension::SlashCommand>>>,
    /// 命令名称到回调函数的映射
    command_handlers: Arc<Mutex<HashMap<String, CommandCallback>>>,
    /// 插件注册的输出释放回调
    output_free: Arc<Mutex<Option<OutputFreeCallback>>>,
}

impl NativeExtension {
    /// 从共享库文件加载扩展。
    ///
    /// 加载动态库，查找 `extension_factory` 符号并调用它，
    /// 扩展通过传入的 vtable 注册事件处理器、工具和命令。
    ///
    /// # Safety
    /// `path` 处的库必须导出符合 FFI 契约的有效 `extension_factory` 符号。
    pub unsafe fn load(path: &std::path::Path, id: String) -> Result<Self, String> {
        let library = libloading::Library::new(path).map_err(|e| format!("load library: {e}"))?;

        let factory: libloading::Symbol<unsafe extern "C" fn(api: *const ExtensionApi)> = library
            .get(b"extension_factory")
            .map_err(|e| format!("find extension_factory: {e}"))?;

        let handlers = Arc::new(Mutex::new(Vec::new()));
        let tools = Arc::new(Mutex::new(Vec::new()));
        let tool_handlers = Arc::new(Mutex::new(HashMap::new()));
        let commands = Arc::new(Mutex::new(Vec::new()));
        let command_handlers = Arc::new(Mutex::new(HashMap::new()));
        let output_free = Arc::new(Mutex::new(None));

        let user_data = Box::new(FfiUserData {
            handlers: Arc::clone(&handlers),
            tools: Arc::clone(&tools),
            tool_handlers: Arc::clone(&tool_handlers),
            commands: Arc::clone(&commands),
            command_handlers: Arc::clone(&command_handlers),
            output_free: Arc::clone(&output_free),
        });

        let api = ExtensionApi {
            user_data: Box::into_raw(user_data) as *mut std::ffi::c_void,
            on: ffi_on,
            register_tool: ffi_register_tool,
            register_tool_handler: ffi_register_tool_handler,
            register_command: ffi_register_command,
            register_command_handler: ffi_register_command_handler,
            register_output_free_handler: ffi_register_output_free_handler,
        };

        let factory_fn = *factory;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            factory_fn(&api as *const ExtensionApi)
        }));

        let _ = unsafe { Box::from_raw(api.user_data as *mut FfiUserData) };

        match result {
            Ok(()) => {},
            Err(_) => return Err(format!("extension_factory panicked for {id}")),
        }

        Ok(Self {
            id,
            _library: library,
            handlers,
            tools,
            tool_handlers,
            commands,
            command_handlers,
            output_free,
        })
    }
}

#[async_trait::async_trait]
impl Extension for NativeExtension {
    fn id(&self) -> &str {
        &self.id
    }

    fn register(&self, reg: &mut Registrar) {
        let output_free = *self.output_free.lock();

        // 注册工具及其执行处理器
        let tools = self.tools.lock().clone();
        let tool_map = self.tool_handlers.lock().clone();
        for tool_def in tools {
            let Some(callback) = tool_map.get(&tool_def.name).copied() else {
                tracing::warn!(
                    extension_id = %self.id,
                    tool_name = %tool_def.name,
                    "tool '{}' registered without handler, skipping",
                    tool_def.name
                );
                continue;
            };
            reg.tool(
                tool_def,
                Arc::new(FfiToolHandler {
                    callback,
                    output_free,
                    extension_id: self.id.clone(),
                }),
            );
        }

        // 注册斜杠命令及其执行处理器
        let commands = self.commands.lock().clone();
        let cmd_map = self.command_handlers.lock().clone();
        for cmd in commands {
            let Some(callback) = cmd_map.get(&cmd.name).copied() else {
                tracing::warn!(
                    extension_id = %self.id,
                    command_name = %cmd.name,
                    "command '{}' registered without handler, skipping",
                    cmd.name
                );
                continue;
            };
            reg.command(
                cmd,
                Arc::new(FfiCommandHandler {
                    callback,
                    output_free,
                    extension_id: self.id.clone(),
                }),
            );
        }

        // 注册事件处理器
        let handlers = self.handlers.lock().clone();
        for (event, mode, callback) in handlers {
            let event_disc = ffi::event_discriminant(event.clone());

            match event {
                ExtensionEvent::PreToolUse => {
                    reg.on_pre_tool_use(
                        mode,
                        0,
                        Arc::new(FfiPreToolUseHandler {
                            callback,
                            event_disc,
                            output_free,
                        }),
                    );
                },
                ExtensionEvent::PostToolUse => {
                    reg.on_post_tool_use(
                        mode,
                        0,
                        Arc::new(FfiPostToolUseHandler {
                            callback,
                            event_disc,
                            output_free,
                        }),
                    );
                },
                ExtensionEvent::BeforeProviderRequest => {
                    reg.on_provider(
                        ProviderEvent::BeforeRequest,
                        mode,
                        0,
                        Arc::new(FfiProviderHandler {
                            callback,
                            event_disc,
                            output_free,
                        }),
                    );
                },
                ExtensionEvent::AfterProviderResponse => {
                    reg.on_provider(
                        ProviderEvent::AfterResponse,
                        mode,
                        0,
                        Arc::new(FfiProviderHandler {
                            callback,
                            event_disc,
                            output_free,
                        }),
                    );
                },
                ExtensionEvent::PromptBuild => {
                    reg.on_prompt_build(
                        0,
                        Arc::new(FfiPromptBuildHandler {
                            callback,
                            event_disc,
                            output_free,
                        }),
                    );
                },
                ExtensionEvent::PreCompact => {
                    reg.on_compact(
                        CompactEvent::PreCompact,
                        0,
                        Arc::new(FfiCompactHandler {
                            callback,
                            event_disc,
                            output_free,
                        }),
                    );
                },
                ExtensionEvent::PostCompact => {
                    reg.on_compact(
                        CompactEvent::PostCompact,
                        0,
                        Arc::new(FfiCompactHandler {
                            callback,
                            event_disc,
                            output_free,
                        }),
                    );
                },
                _ => {
                    reg.on_event(
                        event,
                        mode,
                        0,
                        Arc::new(FfiLifecycleHandler {
                            callback,
                            event_disc,
                            output_free,
                        }),
                    );
                },
            }
        }
    }
}

// ─── FFI 用户数据 ───────────────────────────────────────────────────────

struct FfiUserData {
    handlers: Arc<Mutex<Vec<(ExtensionEvent, HookMode, EventCallback)>>>,
    tools: Arc<Mutex<Vec<ToolDefinition>>>,
    tool_handlers: Arc<Mutex<HashMap<String, ToolCallback>>>,
    commands: Arc<Mutex<Vec<astrcode_core::extension::SlashCommand>>>,
    command_handlers: Arc<Mutex<HashMap<String, CommandCallback>>>,
    output_free: Arc<Mutex<Option<OutputFreeCallback>>>,
}

// ─── FFI vtable 实现 ──────────────────────────────────────────────────

macro_rules! user_data {
    ($api:expr) => {
        &*((*$api).user_data as *const FfiUserData)
    };
}

unsafe extern "C" fn ffi_on(
    api: *const ExtensionApi,
    event: u8,
    mode: u8,
    callback: EventCallback,
) {
    let Some(event) = ffi::event_from_discriminant(event) else {
        return;
    };
    let Some(mode) = ffi::mode_from_discriminant(mode) else {
        return;
    };
    user_data!(api)
        .handlers
        .lock()
        .push((event, mode, callback));
}

unsafe extern "C" fn ffi_register_tool(
    api: *const ExtensionApi,
    name_ptr: *const u8,
    name_len: u32,
    desc_ptr: *const u8,
    desc_len: u32,
    params_json_ptr: *const u8,
    params_json_len: u32,
) {
    let name = ffi::read_ffi_str(name_ptr, name_len);
    let desc = ffi::read_ffi_str(desc_ptr, desc_len);
    let params_json = ffi::read_ffi_str(params_json_ptr, params_json_len);
    let params: serde_json::Value =
        serde_json::from_str(&params_json).unwrap_or(serde_json::json!({}));
    user_data!(api).tools.lock().push(ToolDefinition {
        name,
        description: desc,
        parameters: params,
        origin: ToolOrigin::Extension,
        execution_mode: ExecutionMode::Sequential,
    });
}

unsafe extern "C" fn ffi_register_tool_handler(
    api: *const ExtensionApi,
    name_ptr: *const u8,
    name_len: u32,
    callback: ToolCallback,
) {
    let name = ffi::read_ffi_str(name_ptr, name_len);
    user_data!(api).tool_handlers.lock().insert(name, callback);
}

unsafe extern "C" fn ffi_register_command(
    api: *const ExtensionApi,
    name_ptr: *const u8,
    name_len: u32,
    desc_ptr: *const u8,
    desc_len: u32,
) {
    let name = ffi::read_ffi_str(name_ptr, name_len);
    let desc = ffi::read_ffi_str(desc_ptr, desc_len);
    user_data!(api)
        .commands
        .lock()
        .push(astrcode_core::extension::SlashCommand {
            name,
            description: desc,
            args_schema: None,
        });
}

unsafe extern "C" fn ffi_register_command_handler(
    api: *const ExtensionApi,
    name_ptr: *const u8,
    name_len: u32,
    callback: CommandCallback,
) {
    let name = ffi::read_ffi_str(name_ptr, name_len);
    user_data!(api)
        .command_handlers
        .lock()
        .insert(name, callback);
}

unsafe extern "C" fn ffi_register_output_free_handler(
    api: *const ExtensionApi,
    callback: OutputFreeCallback,
) {
    *user_data!(api).output_free.lock() = Some(callback);
}
