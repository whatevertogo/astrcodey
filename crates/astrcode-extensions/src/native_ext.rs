//! Native extension adapter — wraps a `libloading::Library` + registered handlers.
//!
//! Each `.dll`/`.so` extension is loaded via `libloading`, its
//! `extension_factory` symbol is called with an `ExtensionApi` vtable,
//! and the factory registers event handlers, tools, and commands.
//!
//! This struct implements the `Extension` trait, delegating `on_event()`
//! to the FFI callbacks registered during the factory call.

use std::{collections::HashMap, sync::Mutex};

use astrcode_core::{
    extension::{
        Extension, ExtensionContext, ExtensionError, ExtensionEvent, ExtensionToolOutcome,
        HookEffect, HookMode,
    },
    prompt::BlockSpec,
    tool::{CapabilitySpec, ToolDefinition, ToolResult},
};

use crate::ffi::{self, EventCallback, ExtensionApi, FfiCtxOwned, ToolCallback};

fn read_output(ptr: *const u8, len: u32) -> String {
    if ptr.is_null() || len == 0 {
        String::new()
    } else {
        unsafe { ffi::read_ffi_str(ptr, len) }.to_string()
    }
}

/// A loaded native extension.
///
/// The `Library` is kept alive as long as the extension is registered.
/// When dropped, the library is unloaded.
pub struct NativeExtension {
    id: String,
    /// Keeps the DLL/SO loaded. Must be dropped after all callbacks are no longer called.
    _library: libloading::Library,
    /// Handlers registered through `api.on()` during the factory call.
    handlers: Mutex<Vec<(ExtensionEvent, HookMode, EventCallback)>>,
    /// Tools registered through `api.register_tool()`.
    tools: Mutex<Vec<ToolDefinition>>,
    /// Executable handlers registered through `api.register_tool_handler()`.
    tool_handlers: Mutex<HashMap<String, ToolCallback>>,
    /// Slash commands registered through `api.register_command()`.
    commands: Mutex<Vec<astrcode_core::extension::SlashCommand>>,
}

impl NativeExtension {
    /// Load an extension from a shared library file.
    ///
    /// # Safety
    /// The library at `path` must export a valid `extension_factory` symbol
    /// that follows the FFI contract.
    pub unsafe fn load(path: &std::path::Path, id: String) -> Result<Self, String> {
        let library = libloading::Library::new(path).map_err(|e| format!("load library: {e}"))?;

        let factory: libloading::Symbol<unsafe extern "C" fn(api: *const ExtensionApi)> = library
            .get(b"extension_factory")
            .map_err(|e| format!("find extension_factory: {e}"))?;

        let handlers: Mutex<Vec<(ExtensionEvent, HookMode, EventCallback)>> =
            Mutex::new(Vec::new());
        let tools: Mutex<Vec<ToolDefinition>> = Mutex::new(Vec::new());
        let tool_handlers: Mutex<HashMap<String, ToolCallback>> = Mutex::new(HashMap::new());
        let commands: Mutex<Vec<astrcode_core::extension::SlashCommand>> = Mutex::new(Vec::new());

        // Prepare user_data that FFI callbacks will access via api.user_data.
        let user_data = Box::new(FfiUserData {
            handlers: &handlers,
            tools: &tools,
            tool_handlers: &tool_handlers,
            commands: &commands,
        });

        let api = ExtensionApi {
            user_data: Box::into_raw(user_data) as *mut std::ffi::c_void,
            on: ffi_on,
            register_tool: ffi_register_tool,
            register_tool_handler: ffi_register_tool_handler,
            register_command: ffi_register_command,
        };

        // Call the factory — this is where the extension registers everything.
        let factory_fn = *factory;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            factory_fn(&api as *const ExtensionApi)
        }));

        // Reconstruct user_data Box to free it (handlers/tools/commands are already Mutex-owned)
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
        })
    }
}

#[async_trait::async_trait]
impl Extension for NativeExtension {
    fn id(&self) -> &str {
        &self.id
    }

    fn subscriptions(&self) -> Vec<(ExtensionEvent, HookMode)> {
        self.handlers
            .lock()
            .unwrap()
            .iter()
            .map(|(e, m, _)| (e.clone(), *m))
            .collect()
    }

    async fn on_event(
        &self,
        event: ExtensionEvent,
        ctx: &dyn ExtensionContext,
    ) -> Result<HookEffect, ExtensionError> {
        let event_disc = ffi::event_discriminant(event.clone());
        let ffi_ctx = FfiCtxOwned::from_ext_ctx(ctx);
        let handlers = self.handlers.lock().unwrap();
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
                    0 => {},
                    1 => {
                        let reason = read_output(output_ptr, output_len);
                        return Ok(HookEffect::Block { reason });
                    },
                    2 => {
                        let content = read_output(output_ptr, output_len);
                        return Ok(HookEffect::ModifiedResult { content });
                    },
                    3 => {
                        let content = read_output(output_ptr, output_len);
                        let tool_input = serde_json::from_str(&content).map_err(|e| {
                            ExtensionError::Internal(format!(
                                "extension {} returned invalid ModifiedInput JSON: {e}",
                                self.id
                            ))
                        })?;
                        return Ok(HookEffect::ModifiedInput { tool_input });
                    },
                    _ => {},
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

    fn tools(&self) -> Vec<ToolDefinition> {
        self.tools.lock().unwrap().clone()
    }

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

        let outcome = unsafe {
            ffi::parse_tool_outcome(status, output_ptr, output_len, error_ptr, error_len)
        }
        .map_err(|e| {
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

    fn slash_commands(&self) -> Vec<astrcode_core::extension::SlashCommand> {
        self.commands.lock().unwrap().clone()
    }

    fn context_contributions(&self) -> Vec<BlockSpec> {
        vec![]
    }

    fn capabilities(&self) -> Vec<CapabilitySpec> {
        vec![]
    }
}

// ─── FFI user data ───────────────────────────────────────────────────────

/// Data passed through `api.user_data` to vtable callbacks.
struct FfiUserData<'a> {
    handlers: &'a Mutex<Vec<(ExtensionEvent, HookMode, EventCallback)>>,
    tools: &'a Mutex<Vec<ToolDefinition>>,
    tool_handlers: &'a Mutex<HashMap<String, ToolCallback>>,
    commands: &'a Mutex<Vec<astrcode_core::extension::SlashCommand>>,
}

// ─── FFI vtable implementations ──────────────────────────────────────────

/// Extract FfiUserData from api pointer. #Safety: ptr must be valid.
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
        .unwrap()
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
        serde_json::from_str(params_json).unwrap_or(serde_json::json!({}));
    user_data!(api).tools.lock().unwrap().push(ToolDefinition {
        name: name.to_string(),
        description: desc.to_string(),
        parameters: params,
        is_builtin: false,
    });
}

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
