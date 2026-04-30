//! 原生扩展适配器 — 封装 `libloading::Library` + 已注册的处理器。
//!
//! 每个 `.dll`/`.so` 扩展通过 `libloading` 加载，其 `extension_factory` 符号
//! 被调用时传入一个 `ExtensionApi` vtable，工厂函数借此注册事件处理器、
//! 工具和命令。
//!
//! 本结构体实现了 `Extension` trait，将 `on_event()` 委托给工厂调用期间
//! 注册的 FFI 回调。

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use astrcode_core::{
    extension::{
        Extension, ExtensionContext, ExtensionError, ExtensionEvent, ExtensionToolOutcome,
        HookEffect, HookMode,
    },
    tool::{ToolDefinition, ToolOrigin, ToolResult},
};

use crate::ffi::{
    self, EventCallback, ExtensionApi, FfiCtxOwned, OutputFreeCallback, ToolCallback,
};

/// 从 FFI 输出指针读取字符串，处理空指针情况。
fn read_output(ptr: *const u8, len: u32) -> String {
    if ptr.is_null() || len == 0 {
        String::new()
    } else {
        unsafe { ffi::read_ffi_str(ptr, len) }.to_string()
    }
}

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
        let output_free = Arc::new(Mutex::new(None));

        // 将 Arc 克隆传入 user_data — 无自借用，无裸指针生命周期技巧
        let user_data = Box::new(FfiUserData {
            handlers: Arc::clone(&handlers),
            tools: Arc::clone(&tools),
            tool_handlers: Arc::clone(&tool_handlers),
            commands: Arc::clone(&commands),
            output_free: Arc::clone(&output_free),
        });

        let api = ExtensionApi {
            user_data: Box::into_raw(user_data) as *mut std::ffi::c_void,
            on: ffi_on,
            register_tool: ffi_register_tool,
            register_tool_handler: ffi_register_tool_handler,
            register_command: ffi_register_command,
            register_output_free_handler: ffi_register_output_free_handler,
        };

        // 调用工厂函数 — 扩展在此注册所有内容
        let factory_fn = *factory;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            factory_fn(&api as *const ExtensionApi)
        }));

        // 重建 user_data Box 以释放内存（handlers/tools/commands 已由 Mutex 持有）
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
            output_free,
        })
    }

    fn read_and_release_output(&self, ptr: *const u8, len: u32) -> String {
        let text = read_output(ptr, len);
        self.release_output(ptr, len);
        text
    }

    fn release_output(&self, ptr: *const u8, len: u32) {
        if ptr.is_null() || len == 0 {
            return;
        }
        let callback = self.output_free.lock().ok().and_then(|free| *free);
        if let Some(callback) = callback {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                callback(ptr, len)
            }));
            if result.is_err() {
                tracing::warn!("Extension {} output free callback panicked", self.id);
            }
        }
    }
}

#[async_trait::async_trait]
impl Extension for NativeExtension {
    fn id(&self) -> &str {
        &self.id
    }

    /// 返回此扩展订阅的所有 (事件, 模式) 对。
    fn subscriptions(&self) -> Vec<(ExtensionEvent, HookMode)> {
        self.handlers
            .lock()
            .unwrap()
            .iter()
            .map(|(e, m, _)| (e.clone(), *m))
            .collect()
    }

    /// 将事件分发给所有匹配的 FFI 回调。
    ///
    /// 遍历已注册的处理器，调用匹配当前事件的回调，
    /// 并将 FFI effect_out 转换为 Rust [`HookEffect`]。
    async fn on_event(
        &self,
        event: ExtensionEvent,
        ctx: &dyn ExtensionContext,
    ) -> Result<HookEffect, ExtensionError> {
        let event_disc = ffi::event_discriminant(event.clone());
        let ffi_ctx = FfiCtxOwned::from_ext_ctx(ctx);
        let handlers = self.handlers.lock().unwrap();
        // 收集匹配的回调，然后释放锁
        let callbacks: Vec<EventCallback> = handlers
            .iter()
            .filter(|(e, _, _)| *e == event)
            .map(|(_, _, cb)| *cb)
            .collect();
        drop(handlers);

        for callback in &callbacks {
            let mut effect_out: u8 = 0;
            let mut output_ptr: *const u8 = std::ptr::null();
            let mut output_len: u32 = 0;

            // 捕获回调中的 panic，防止扩展崩溃影响宿主
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
                Ok(_) => match effect_out {
                    // 0 = Allow，继续处理下一个回调
                    0 => {
                        self.release_output(output_ptr, output_len);
                    },
                    // 1 = Block，立即返回阻止效果
                    1 => {
                        let reason = self.read_and_release_output(output_ptr, output_len);
                        return Ok(HookEffect::Block { reason });
                    },
                    // 2 = ModifiedResult
                    2 => {
                        let content = self.read_and_release_output(output_ptr, output_len);
                        return Ok(HookEffect::ModifiedResult { content });
                    },
                    // 3 = ModifiedInput
                    3 => {
                        let content = self.read_and_release_output(output_ptr, output_len);
                        let tool_input = serde_json::from_str(&content).map_err(|e| {
                            ExtensionError::Internal(format!(
                                "extension {} returned invalid ModifiedInput JSON: {e}",
                                self.id
                            ))
                        })?;
                        return Ok(HookEffect::ModifiedInput { tool_input });
                    },
                    // 4 = PromptContributions
                    4 => {
                        let content = self.read_and_release_output(output_ptr, output_len);
                        let contributions = serde_json::from_str(&content).map_err(|e| {
                            ExtensionError::Internal(format!(
                                "extension {} returned invalid PromptContributions JSON: {e}",
                                self.id
                            ))
                        })?;
                        return Ok(HookEffect::PromptContributions(contributions));
                    },
                    _ => {
                        self.release_output(output_ptr, output_len);
                    },
                },
                Err(_) => {
                    tracing::warn!(
                        "Extension {} callback panicked for event {event:?}",
                        self.id
                    );
                },
            }
        }

        Ok(HookEffect::Allow)
    }

    /// 返回此扩展注册的所有工具定义。
    fn tools(&self) -> Vec<ToolDefinition> {
        self.tools.lock().unwrap().clone()
    }

    /// 执行指定名称的工具。
    ///
    /// 查找已注册的工具回调，通过 FFI 调用它，并将结果解析为 [`ToolResult`]。
    async fn execute_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        working_dir: &str,
        ctx: &astrcode_core::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        let callback = self
            .tool_handlers
            .lock()
            .map_err(|e| ExtensionError::Internal(e.to_string()))?
            .get(tool_name)
            .copied();
        let Some(callback) = callback else {
            return Err(ExtensionError::NotFound(tool_name.into()));
        };

        let ffi_ctx = FfiCtxOwned::from_tool_execution(working_dir, tool_name, &arguments, ctx);
        let mut output_ptr: *const u8 = std::ptr::null();
        let mut output_len: u32 = 0;
        let mut error_ptr: *const u8 = std::ptr::null();
        let mut error_len: u32 = 0;

        // 捕获工具回调中的 panic
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            (callback)(
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
                return Err(ExtensionError::Internal(format!(
                    "extension {} tool handler panicked for {tool_name}",
                    self.id
                )));
            },
        };

        // 解析 FFI 返回的工具结果
        let parsed_outcome = unsafe {
            ffi::parse_tool_outcome(status, output_ptr, output_len, error_ptr, error_len)
        };
        self.release_output(output_ptr, output_len);
        self.release_output(error_ptr, error_len);
        let outcome = parsed_outcome.map_err(|e| {
            ExtensionError::Internal(format!(
                "extension {} tool handler invalid result: {e}",
                self.id
            ))
        })?;

        match outcome {
            ExtensionToolOutcome::Text { content, is_error } => Ok(ToolResult {
                call_id: String::new(),
                content: content.clone(),
                is_error,
                error: if is_error { Some(content) } else { None },
                metadata: Default::default(),
                duration_ms: None,
            }),
            // RunSession 声明式结果：将结果序列化为 JSON 存入 metadata
            ext_outcome @ ExtensionToolOutcome::RunSession { .. } => {
                let outcome_json = serde_json::to_value(&ext_outcome).map_err(|e| {
                    ExtensionError::Internal(format!(
                        "extension {} serializing outcome: {e}",
                        self.id
                    ))
                })?;
                let mut metadata = std::collections::BTreeMap::new();
                metadata.insert("extension_tool_outcome".into(), outcome_json);
                Ok(ToolResult {
                    call_id: String::new(),
                    content: String::new(),
                    is_error: false,
                    error: None,
                    metadata,
                    duration_ms: None,
                })
            },
        }
    }

    /// 返回此扩展注册的所有斜杠命令。
    fn slash_commands(&self) -> Vec<astrcode_core::extension::SlashCommand> {
        self.commands.lock().unwrap().clone()
    }
}

// ─── FFI 用户数据 ───────────────────────────────────────────────────────

/// 通过 `api.user_data` 传递给 vtable 回调的数据。
///
/// 每个字段都是 `Arc` — 工厂函数存储的是克隆的 Arc，
/// 因此工厂返回后 `Box::from_raw(user_data)` 只是释放克隆。
/// 原始 Arc 转移到 `NativeExtension` 中，在 DLL 生命周期内保持存活。
struct FfiUserData {
    handlers: Arc<Mutex<Vec<(ExtensionEvent, HookMode, EventCallback)>>>,
    tools: Arc<Mutex<Vec<ToolDefinition>>>,
    tool_handlers: Arc<Mutex<HashMap<String, ToolCallback>>>,
    commands: Arc<Mutex<Vec<astrcode_core::extension::SlashCommand>>>,
    output_free: Arc<Mutex<Option<OutputFreeCallback>>>,
}

// ─── FFI vtable 实现 ──────────────────────────────────────────────────

/// 从 api 指针提取 FfiUserData 的宏。
/// # Safety: ptr 必须有效
macro_rules! user_data {
    ($api:expr) => {
        &*((*$api).user_data as *const FfiUserData)
    };
}

/// FFI `on` 回调实现：注册事件处理器。
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
        .unwrap()
        .push((event, mode, callback));
}

/// FFI `register_tool` 回调实现：注册工具定义。
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
    // 解析参数 JSON，失败时使用空对象
    let params: serde_json::Value =
        serde_json::from_str(params_json).unwrap_or(serde_json::json!({}));
    user_data!(api).tools.lock().unwrap().push(ToolDefinition {
        name: name.to_string(),
        description: desc.to_string(),
        parameters: params,
        origin: ToolOrigin::Extension,
    });
}

/// FFI `register_tool_handler` 回调实现：注册工具执行处理器。
unsafe extern "C" fn ffi_register_tool_handler(
    api: *const ExtensionApi,
    name_ptr: *const u8,
    name_len: u32,
    callback: ToolCallback,
) {
    let name = ffi::read_ffi_str(name_ptr, name_len).to_string();
    if let Ok(mut handlers) = user_data!(api).tool_handlers.lock() {
        handlers.insert(name, callback);
    }
}

/// FFI `register_command` 回调实现：注册斜杠命令。
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
        .unwrap()
        .push(astrcode_core::extension::SlashCommand {
            name: name.to_string(),
            description: desc.to_string(),
            args_schema: None,
        });
}

/// FFI `register_output_free_handler` 回调实现：注册插件侧输出释放函数。
unsafe extern "C" fn ffi_register_output_free_handler(
    api: *const ExtensionApi,
    callback: OutputFreeCallback,
) {
    if let Ok(mut output_free) = user_data!(api).output_free.lock() {
        *output_free = Some(callback);
    }
}
