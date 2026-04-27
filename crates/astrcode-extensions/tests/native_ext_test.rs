//! Test native extension FFI infrastructure without actual libloading.

use std::sync::Mutex;

use astrcode_core::{
    config::ModelSelection,
    extension::{ExtensionContext, ExtensionEvent, HookMode},
    tool::ToolDefinition,
};
use astrcode_extensions::ffi::{self, EventCallback, ExtensionApi, FfiCtxOwned};

/// Test context that returns fixed values.
struct TestCtx {
    sid: String,
    wd: String,
}
#[async_trait::async_trait]
impl ExtensionContext for TestCtx {
    fn session_id(&self) -> &str {
        &self.sid
    }
    fn working_dir(&self) -> &str {
        &self.wd
    }
    fn model_selection(&self) -> ModelSelection {
        ModelSelection {
            profile_name: String::new(),
            model: String::new(),
            provider_kind: String::new(),
        }
    }
    fn config_value(&self, _: &str) -> Option<String> {
        None
    }
    async fn emit_custom_event(&self, _: &str, _: serde_json::Value) {}
    fn find_tool(&self, _: &str) -> Option<ToolDefinition> {
        None
    }
    fn log_warn(&self, _: &str) {}
    fn snapshot(&self) -> std::sync::Arc<dyn ExtensionContext> {
        std::sync::Arc::new(TestCtx {
            sid: self.sid.clone(),
            wd: self.wd.clone(),
        })
    }
}

#[test]
fn ffi_vtable_register_handler_and_invoke() {
    let handlers: Mutex<Vec<(ExtensionEvent, HookMode, EventCallback)>> = Mutex::new(Vec::new());
    let tools: Mutex<Vec<ToolDefinition>> = Mutex::new(Vec::new());
    let commands = Mutex::new(Vec::new());

    let ud = Box::new(super_ud(&handlers, &tools, &commands));
    let api = ExtensionApi {
        user_data: Box::into_raw(ud) as *mut std::ffi::c_void,
        on: test_ffi_on,
        register_tool: test_ffi_register_tool,
        register_command: test_ffi_register_command,
    };

    // Simulate factory call: register a PreToolUse handler
    unsafe {
        (api.on)(&api, 4, 0, test_blocking_handler); // PreToolUse, Blocking
    }

    assert_eq!(handlers.lock().unwrap().len(), 1);
    assert_eq!(handlers.lock().unwrap()[0].0, ExtensionEvent::PreToolUse);

    // Cleanup
    let _ = unsafe { Box::from_raw(api.user_data as *mut TestUserData) };
}

#[test]
fn ffi_register_tool_stores_definition() {
    let handlers: Mutex<Vec<(ExtensionEvent, HookMode, EventCallback)>> = Mutex::new(Vec::new());
    let tools: Mutex<Vec<ToolDefinition>> = Mutex::new(Vec::new());
    let commands = Mutex::new(Vec::new());

    let ud = Box::new(super_ud(&handlers, &tools, &commands));
    let api = ExtensionApi {
        user_data: Box::into_raw(ud) as *mut std::ffi::c_void,
        on: test_ffi_on,
        register_tool: test_ffi_register_tool,
        register_command: test_ffi_register_command,
    };

    let name = b"my_tool";
    let desc = b"does things";
    let params = b"{}";

    unsafe {
        (api.register_tool)(
            &api,
            name.as_ptr(),
            name.len() as u32,
            desc.as_ptr(),
            desc.len() as u32,
            params.as_ptr(),
            params.len() as u32,
        );
    }

    assert_eq!(tools.lock().unwrap().len(), 1);
    assert_eq!(tools.lock().unwrap()[0].name, "my_tool");

    let _ = unsafe { Box::from_raw(api.user_data as *mut TestUserData) };
}

#[test]
fn ffi_ctx_passes_session_info() {
    let ctx = TestCtx {
        sid: "s1".into(),
        wd: "/tmp".into(),
    };
    let ffi_ctx = FfiCtxOwned::from_ext_ctx(&ctx);
    let raw = unsafe { &*(ffi_ctx.as_ptr() as *const astrcode_extensions::ffi::FfiCtx) };
    unsafe {
        let sid = ffi::read_ffi_str(raw.session_id_ptr, raw.session_id_len);
        assert_eq!(sid, "s1");
        let wd = ffi::read_ffi_str(raw.working_dir_ptr, raw.working_dir_len);
        assert_eq!(wd, "/tmp");
    }
}

#[test]
fn blocking_handler_effect_is_returned() {
    let handlers: Mutex<Vec<(ExtensionEvent, HookMode, EventCallback)>> = Mutex::new(Vec::new());
    let tools: Mutex<Vec<ToolDefinition>> = Mutex::new(Vec::new());
    let commands = Mutex::new(Vec::new());

    let ud = Box::new(super_ud(&handlers, &tools, &commands));
    let api = ExtensionApi {
        user_data: Box::into_raw(ud) as *mut std::ffi::c_void,
        on: test_ffi_on,
        register_tool: test_ffi_register_tool,
        register_command: test_ffi_register_command,
    };

    unsafe {
        (api.on)(&api, 4, 0, test_blocking_handler);
    }

    let callbacks: Vec<EventCallback> = handlers
        .lock()
        .unwrap()
        .iter()
        .map(|(_, _, cb)| *cb)
        .collect();

    let mut effect_out: u8 = 0;
    let mut reason_ptr: *const u8 = std::ptr::null();
    let mut reason_len: u32 = 0;
    unsafe {
        (callbacks[0])(
            4,
            std::ptr::null(),
            &mut effect_out,
            &mut reason_ptr,
            &mut reason_len,
        );
    }
    assert_eq!(effect_out, 1); // Block
    unsafe {
        let reason = ffi::read_ffi_str(reason_ptr, reason_len);
        assert_eq!(reason, "blocked");
    }

    let _ = unsafe { Box::from_raw(api.user_data as *mut TestUserData) };
}

// ─── Test helpers ──────────────────────────────────────────────────────

struct TestUserData<'a> {
    handlers: &'a Mutex<Vec<(ExtensionEvent, HookMode, EventCallback)>>,
    tools: &'a Mutex<Vec<ToolDefinition>>,
    commands: &'a Mutex<Vec<astrcode_core::extension::SlashCommand>>,
}

fn super_ud<'a>(
    h: &'a Mutex<Vec<(ExtensionEvent, HookMode, EventCallback)>>,
    t: &'a Mutex<Vec<ToolDefinition>>,
    c: &'a Mutex<Vec<astrcode_core::extension::SlashCommand>>,
) -> TestUserData<'a> {
    TestUserData {
        handlers: h,
        tools: t,
        commands: c,
    }
}

unsafe extern "C" fn test_ffi_on(
    api: *const ExtensionApi,
    event: u8,
    mode: u8,
    callback: EventCallback,
) {
    let ud = &*((*api).user_data as *const TestUserData);
    let Some(event) = ffi::event_from_discriminant(event) else {
        return;
    };
    let Some(mode) = ffi::mode_from_discriminant(mode) else {
        return;
    };
    ud.handlers.lock().unwrap().push((event, mode, callback));
}

unsafe extern "C" fn test_ffi_register_tool(
    api: *const ExtensionApi,
    name_ptr: *const u8,
    name_len: u32,
    desc_ptr: *const u8,
    desc_len: u32,
    params_ptr: *const u8,
    params_len: u32,
) {
    let ud = &*((*api).user_data as *const TestUserData);
    ud.tools.lock().unwrap().push(ToolDefinition {
        name: ffi::read_ffi_str(name_ptr, name_len).to_string(),
        description: ffi::read_ffi_str(desc_ptr, desc_len).to_string(),
        parameters: serde_json::from_str(ffi::read_ffi_str(params_ptr, params_len))
            .unwrap_or(serde_json::json!({})),
        is_builtin: false,
    });
}

unsafe extern "C" fn test_ffi_register_command(
    api: *const ExtensionApi,
    name_ptr: *const u8,
    name_len: u32,
    desc_ptr: *const u8,
    desc_len: u32,
) {
    let ud = &*((*api).user_data as *const TestUserData);
    ud.commands
        .lock()
        .unwrap()
        .push(astrcode_core::extension::SlashCommand {
            name: ffi::read_ffi_str(name_ptr, name_len).to_string(),
            description: ffi::read_ffi_str(desc_ptr, desc_len).to_string(),
            args_schema: None,
        });
}

unsafe extern "C" fn test_blocking_handler(
    _event: u8,
    _ctx: *const std::ffi::c_void,
    effect_out: *mut u8,
    reason_out: *mut *const u8,
    reason_len_out: *mut u32,
) {
    *effect_out = 1;
    *reason_out = b"blocked".as_ptr();
    *reason_len_out = 7;
}
