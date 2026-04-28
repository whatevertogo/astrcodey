//! FFI type definitions mirrored from the host.
//!
//! These match `astrcode-extensions/src/ffi.rs` exactly.
//! The cdylib cannot link against the host crate, so types are duplicated here
//! as a stable C ABI contract.

use std::ffi::c_void;

// ─── Event discriminants ─────────────────────────────────────────────────

pub const EVENT_SESSION_START: u8 = 0;

// ─── HookMode discriminants ──────────────────────────────────────────────

pub const MODE_BLOCKING: u8 = 0;

// ─── HookEffect discriminants ────────────────────────────────────────────

pub const EFFECT_ALLOW: u8 = 0;

// ─── Tool callback status ───────────────────────────────────────────────
//  0 = plain text success
//  1 = error
//  2 = JSON outcome (output_ptr serialized as ExtensionToolOutcome)

pub const TOOL_STATUS_OK: u8 = 0;
pub const TOOL_STATUS_ERROR: u8 = 1;
pub const TOOL_STATUS_OUTCOME_JSON: u8 = 2;

// ─── Callback types ──────────────────────────────────────────────────────

pub type EventCallback = unsafe extern "C" fn(
    event: u8,
    ctx: *const c_void,
    effect_out: *mut u8,
    output_ptr_out: *mut *const u8,
    output_len_out: *mut u32,
);

pub type ToolCallback = unsafe extern "C" fn(
    ctx: *const c_void,
    output_ptr_out: *mut *const u8,
    output_len_out: *mut u32,
    error_ptr_out: *mut *const u8,
    error_len_out: *mut u32,
) -> u8;

#[repr(C)]
pub struct ExtensionApi {
    pub user_data: *mut c_void,
    pub on: unsafe extern "C" fn(
        api: *const ExtensionApi,
        event: u8,
        mode: u8,
        callback: EventCallback,
    ),
    pub register_tool: unsafe extern "C" fn(
        api: *const ExtensionApi,
        name_ptr: *const u8,
        name_len: u32,
        desc_ptr: *const u8,
        desc_len: u32,
        params_json_ptr: *const u8,
        params_json_len: u32,
    ),
    pub register_tool_handler: unsafe extern "C" fn(
        api: *const ExtensionApi,
        name_ptr: *const u8,
        name_len: u32,
        callback: ToolCallback,
    ),
    pub register_command: unsafe extern "C" fn(
        api: *const ExtensionApi,
        name_ptr: *const u8,
        name_len: u32,
        desc_ptr: *const u8,
        desc_len: u32,
    ),
}

// ─── FFI Context ─────────────────────────────────────────────────────────

#[repr(C)]
pub struct FfiCtx {
    pub session_id_ptr: *const u8,
    pub session_id_len: u32,
    pub working_dir_ptr: *const u8,
    pub working_dir_len: u32,
    pub tool_name_ptr: *const u8,
    pub tool_name_len: u32,
    pub tool_input_ptr: *const u8,
    pub tool_input_len: u32,
    pub model_id_ptr: *const u8,
    pub model_id_len: u32,
    pub tools_json_ptr: *const u8,
    pub tools_json_len: u32,
}

pub unsafe fn read_ffi_str<'a>(ptr: *const u8, len: u32) -> &'a str {
    if ptr.is_null() || len == 0 {
        return "";
    }
    let bytes = std::slice::from_raw_parts(ptr, len as usize);
    std::str::from_utf8_unchecked(bytes)
}

pub unsafe fn read_ffi_ctx(ctx: *const c_void) -> &'static FfiCtx {
    &*(ctx as *const FfiCtx)
}
