//! FFI boundary — C ABI vtable for extension ↔ host communication.
//!
//! Extensions are `.dll`/`.so` files. They export a single function:
//! ```c
//! void extension_factory(const ExtensionApi *api);
//! ```
//!
//! Strings cross the FFI boundary as `(ptr: *const u8, len: u32)` pairs.

use std::ffi::c_void;

use astrcode_core::extension::{ExtensionEvent, HookMode};

/// Signature of a hook handler callback registered by an extension.
pub type EventCallback = unsafe extern "C" fn(
    event: u8,
    ctx: *const c_void,
    effect_out: *mut u8,
    block_reason_out: *mut *const u8,
    block_reason_len_out: *mut u32,
);

/// The vtable passed to `extension_factory()`.
#[repr(C)]
pub struct ExtensionApi {
    /// Opaque host data passed to every vtable call.
    pub user_data: *mut c_void,

    /// Register an event handler.
    pub on: unsafe extern "C" fn(
        api: *const ExtensionApi,
        event: u8,
        mode: u8,
        callback: EventCallback,
    ),

    /// Register a tool definition.
    pub register_tool: unsafe extern "C" fn(
        api: *const ExtensionApi,
        name_ptr: *const u8,
        name_len: u32,
        desc_ptr: *const u8,
        desc_len: u32,
        params_json_ptr: *const u8,
        params_json_len: u32,
    ),

    /// Register a slash command.
    pub register_command: unsafe extern "C" fn(
        api: *const ExtensionApi,
        name_ptr: *const u8,
        name_len: u32,
        desc_ptr: *const u8,
        desc_len: u32,
    ),
}

// ─── Discriminant helpers ────────────────────────────────────────────────

pub const fn event_discriminant(event: ExtensionEvent) -> u8 {
    match event {
        ExtensionEvent::SessionStart => 0,
        ExtensionEvent::SessionShutdown => 1,
        ExtensionEvent::TurnStart => 2,
        ExtensionEvent::TurnEnd => 3,
        ExtensionEvent::PreToolUse => 4,
        ExtensionEvent::PostToolUse => 5,
        ExtensionEvent::BeforeProviderRequest => 6,
        ExtensionEvent::AfterProviderResponse => 7,
        ExtensionEvent::UserPromptSubmit => 8,
    }
}

pub fn event_from_discriminant(d: u8) -> Option<ExtensionEvent> {
    match d {
        0 => Some(ExtensionEvent::SessionStart),
        1 => Some(ExtensionEvent::SessionShutdown),
        2 => Some(ExtensionEvent::TurnStart),
        3 => Some(ExtensionEvent::TurnEnd),
        4 => Some(ExtensionEvent::PreToolUse),
        5 => Some(ExtensionEvent::PostToolUse),
        6 => Some(ExtensionEvent::BeforeProviderRequest),
        7 => Some(ExtensionEvent::AfterProviderResponse),
        8 => Some(ExtensionEvent::UserPromptSubmit),
        _ => None,
    }
}

pub const fn mode_discriminant(mode: HookMode) -> u8 {
    match mode {
        HookMode::Blocking => 0,
        HookMode::NonBlocking => 1,
        HookMode::Advisory => 2,
    }
}

pub fn mode_from_discriminant(d: u8) -> Option<HookMode> {
    match d {
        0 => Some(HookMode::Blocking),
        1 => Some(HookMode::NonBlocking),
        2 => Some(HookMode::Advisory),
        _ => None,
    }
}

/// Read a (ptr, len) string from FFI into a Rust &str.
///
/// # Safety
/// `ptr` must point to `len` bytes of valid UTF-8.
pub unsafe fn read_ffi_str<'a>(ptr: *const u8, len: u32) -> &'a str {
    if ptr.is_null() || len == 0 {
        return "";
    }
    let bytes = std::slice::from_raw_parts(ptr, len as usize);
    std::str::from_utf8_unchecked(bytes)
}

// ─── FFI Context ────────────────────────────────────────────────────────

/// Context passed to extension event callbacks.
///
/// Contains a minimal read-only view of the session and current event data.
/// All strings are (ptr, len) pairs pointing to host-owned UTF-8 data valid
/// for the duration of the callback.
#[repr(C)]
pub struct FfiCtx {
    /// Session ID (ptr, len).
    pub session_id_ptr: *const u8,
    pub session_id_len: u32,
    /// Working directory (ptr, len).
    pub working_dir_ptr: *const u8,
    pub working_dir_len: u32,
    /// Tool name (ptr, len) — only set for PreToolUse/PostToolUse.
    pub tool_name_ptr: *const u8,
    pub tool_name_len: u32,
    /// Tool input JSON (ptr, len) — only set for PreToolUse/PostToolUse.
    pub tool_input_ptr: *const u8,
    pub tool_input_len: u32,
}

impl FfiCtx {
    fn from_parts(
        session_id: &str,
        working_dir: &str,
        tool_name: &str,
        tool_input_json: &str,
    ) -> Self {
        let sid = session_id.as_bytes();
        let wd = working_dir.as_bytes();
        let tn = tool_name.as_bytes();
        let input = tool_input_json.as_bytes();
        Self {
            session_id_ptr: sid.as_ptr(),
            session_id_len: sid.len() as u32,
            working_dir_ptr: wd.as_ptr(),
            working_dir_len: wd.len() as u32,
            tool_name_ptr: tn.as_ptr(),
            tool_name_len: tn.len() as u32,
            tool_input_ptr: input.as_ptr(),
            tool_input_len: input.len() as u32,
        }
    }
}

/// Owned backing storage for `FfiCtx`.
///
/// `FfiCtx` itself is a raw C view, so this wrapper keeps all pointed-to
/// strings alive for the duration of the callback.
pub struct FfiCtxOwned {
    session_id: String,
    working_dir: String,
    tool_name: String,
    tool_input_json: String,
    raw: FfiCtx,
}

impl FfiCtxOwned {
    pub fn from_ext_ctx(ctx: &dyn astrcode_core::extension::ExtensionContext) -> Self {
        let session_id = ctx.session_id().to_string();
        let working_dir = ctx.working_dir().to_string();
        let pre = ctx.pre_tool_use_input();
        let post = ctx.post_tool_use_input();
        let (tool_name, tool_input) = if let Some(input) = pre {
            (input.tool_name, input.tool_input)
        } else if let Some(input) = post {
            (input.tool_name, input.tool_input)
        } else {
            (String::new(), serde_json::Value::Null)
        };
        let tool_input_json = if tool_input.is_null() {
            String::new()
        } else {
            tool_input.to_string()
        };
        let mut owned = Self {
            session_id,
            working_dir,
            tool_name,
            tool_input_json,
            raw: FfiCtx {
                session_id_ptr: std::ptr::null(),
                session_id_len: 0,
                working_dir_ptr: std::ptr::null(),
                working_dir_len: 0,
                tool_name_ptr: std::ptr::null(),
                tool_name_len: 0,
                tool_input_ptr: std::ptr::null(),
                tool_input_len: 0,
            },
        };
        owned.raw = FfiCtx::from_parts(
            &owned.session_id,
            &owned.working_dir,
            &owned.tool_name,
            &owned.tool_input_json,
        );
        owned
    }

    pub fn as_ptr(&self) -> *const c_void {
        &self.raw as *const FfiCtx as *const c_void
    }
}
