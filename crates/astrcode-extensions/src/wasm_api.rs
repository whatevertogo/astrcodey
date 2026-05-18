//! WASM 扩展协议 — 宿主状态、内存读写、host import 注册。
//!
//! 定义了 WASM 插件和宿主之间的通信协议：
//! - 宿主通过 Linker 提供 host_xxx import 函数
//! - 插件在 `extension_init()` 中调用这些 import 注册工具/命令/事件订阅
//! - 宿主调用插件的 `handle_tool` / `handle_command` / `handle_event` 时， 通过线性内存传递 JSON
//!   请求，插件通过 `host_set_response` 返回结果

use astrcode_core::{
    extension::{ExtensionEvent, HookMode, SlashCommand},
    tool::{ExecutionMode, ToolDefinition, ToolOrigin},
};
use wasmtime::{Caller, Linker};

// ─── Discriminant helpers ───────────────────────────────────────────────

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
        ExtensionEvent::PromptBuild => 9,
        ExtensionEvent::PreCompact => 10,
        ExtensionEvent::PostCompact => 11,
        ExtensionEvent::TurnAborted => 12,
        ExtensionEvent::PostToolUseFailure => 13,
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
        9 => Some(ExtensionEvent::PromptBuild),
        10 => Some(ExtensionEvent::PreCompact),
        11 => Some(ExtensionEvent::PostCompact),
        12 => Some(ExtensionEvent::TurnAborted),
        13 => Some(ExtensionEvent::PostToolUseFailure),
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

// ─── Tool execution mode discriminants ───────────────────────────────────

pub const fn execution_mode_discriminant(mode: ExecutionMode) -> u8 {
    match mode {
        ExecutionMode::Sequential => 0,
        ExecutionMode::Parallel => 1,
    }
}

/// 把 guest 传过来的判别值转成 `ExecutionMode`。
///
/// 未知值默认回退到 `Sequential` ——这是更安全的选择：并发执行可能与共享文件
/// 状态、subprocess 等冲突。明确想要 `Parallel` 的扩展必须传 `1`。
pub fn execution_mode_from_discriminant(d: u8) -> ExecutionMode {
    match d {
        1 => ExecutionMode::Parallel,
        _ => ExecutionMode::Sequential,
    }
}

// ─── Guest response effect codes ─────────────────────────────────────────

/// WASM guest `handle_event` / `handle_tool` 返回的 effect code。
pub const GUEST_EFFECT_OK: i8 = 0;
/// 操作失败，content 为错误信息。
pub const GUEST_EFFECT_ERROR: i8 = 1;
/// 工具执行结果包含 `RunSession` outcome。
pub const GUEST_EFFECT_TOOL_OUTCOME: i8 = 2;
/// `PreToolUse` 返回 `ModifiedInput`，content 为新 tool_input JSON。
pub const GUEST_EFFECT_MODIFIED_INPUT: i8 = 3;
/// `PromptBuild` 返回贡献，content 为 `PromptContributions` JSON。
pub const GUEST_EFFECT_PROMPT_CONTRIBUTIONS: i8 = 4;
/// `Compact` 返回贡献，content 为 `CompactContributions` JSON。
pub const GUEST_EFFECT_COMPACT_CONTRIBUTIONS: i8 = 5;
/// `Provider` 返回 `ReplaceMessages`，content 为 messages JSON。
pub const GUEST_EFFECT_REPLACE_MESSAGES: i8 = 6;
/// `Provider` 返回 `AppendMessages`，content 为 messages JSON。
pub const GUEST_EFFECT_APPEND_MESSAGES: i8 = 7;

// ─── Host State ─────────────────────────────────────────────────────────

/// 宿主在 wasmtime Store 中携带的状态。
///
/// 在 `extension_init()` 期间由 host import 函数填充，
/// 在后续调用中用于读取插件响应。
pub struct HostState {
    pub tools: Vec<ToolDefinition>,
    pub commands: Vec<SlashCommand>,
    pub subscriptions: Vec<(ExtensionEvent, HookMode)>,
    pub response_ptr: u32,
    pub response_len: u32,
}

impl HostState {
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            commands: Vec::new(),
            subscriptions: Vec::new(),
            response_ptr: 0,
            response_len: 0,
        }
    }
}

impl Default for HostState {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Memory helpers ─────────────────────────────────────────────────────

fn read_memory_string(caller: &mut Caller<'_, HostState>, ptr: u32, len: u32) -> String {
    if len == 0 {
        return String::new();
    }
    let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
        return String::new();
    };
    let data = mem.data(caller);
    let start = ptr as usize;
    let end = start + len as usize;
    if end > data.len() {
        return String::new();
    }
    String::from_utf8_lossy(&data[start..end]).into_owned()
}

// ─── Host import functions ──────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn host_register_tool(
    mut caller: Caller<'_, HostState>,
    name_ptr: i32,
    name_len: i32,
    desc_ptr: i32,
    desc_len: i32,
    schema_ptr: i32,
    schema_len: i32,
    execution_mode_disc: i32,
) {
    let name = read_memory_string(&mut caller, name_ptr as u32, name_len as u32);
    let desc = read_memory_string(&mut caller, desc_ptr as u32, desc_len as u32);
    let schema = read_memory_string(&mut caller, schema_ptr as u32, schema_len as u32);
    let params: serde_json::Value = serde_json::from_str(&schema).unwrap_or(serde_json::json!({}));
    let execution_mode = execution_mode_from_discriminant(execution_mode_disc as u8);
    caller.data_mut().tools.push(ToolDefinition {
        name,
        description: desc,
        parameters: params,
        origin: ToolOrigin::Extension,
        execution_mode,
    });
}

fn host_register_command(
    mut caller: Caller<'_, HostState>,
    name_ptr: i32,
    name_len: i32,
    desc_ptr: i32,
    desc_len: i32,
) {
    let name = read_memory_string(&mut caller, name_ptr as u32, name_len as u32);
    let desc = read_memory_string(&mut caller, desc_ptr as u32, desc_len as u32);
    caller.data_mut().commands.push(SlashCommand {
        name,
        description: desc,
        args_schema: None,
    });
}

fn host_subscribe(mut caller: Caller<'_, HostState>, event_disc: i32, mode_disc: i32) {
    let Some(event) = event_from_discriminant(event_disc as u8) else {
        return;
    };
    let Some(mode) = mode_from_discriminant(mode_disc as u8) else {
        return;
    };
    caller.data_mut().subscriptions.push((event, mode));
}

fn host_set_response(mut caller: Caller<'_, HostState>, ptr: i32, len: i32) {
    caller.data_mut().response_ptr = ptr as u32;
    caller.data_mut().response_len = len as u32;
}

fn host_log(mut caller: Caller<'_, HostState>, _level: i32, msg_ptr: i32, msg_len: i32) {
    let msg = read_memory_string(&mut caller, msg_ptr as u32, msg_len as u32);
    tracing::info!(target: "wasm_ext", "{}", msg);
}

// ─── Linker builder ─────────────────────────────────────────────────────

/// 创建包含所有 host import 函数的 Linker。
pub fn create_linker(engine: &wasmtime::Engine) -> Result<Linker<HostState>, String> {
    let mut linker = Linker::new(engine);
    linker
        .func_wrap("env", "host_register_tool", host_register_tool)
        .map_err(|e| format!("register host_register_tool: {e}"))?;
    linker
        .func_wrap("env", "host_register_command", host_register_command)
        .map_err(|e| format!("register host_register_command: {e}"))?;
    linker
        .func_wrap("env", "host_subscribe", host_subscribe)
        .map_err(|e| format!("register host_subscribe: {e}"))?;
    linker
        .func_wrap("env", "host_set_response", host_set_response)
        .map_err(|e| format!("register host_set_response: {e}"))?;
    linker
        .func_wrap("env", "host_log", host_log)
        .map_err(|e| format!("register host_log: {e}"))?;
    Ok(linker)
}

/// 从 HostState 读取响应字符串并清空。
pub fn take_response(store: &wasmtime::Store<HostState>, memory: &wasmtime::Memory) -> String {
    let state = store.data();
    let (ptr, len) = (state.response_ptr, state.response_len);
    if len == 0 {
        return String::new();
    }
    let data = memory.data(store);
    let start = ptr as usize;
    let end = start + len as usize;
    if end > data.len() {
        return String::new();
    }
    String::from_utf8_lossy(&data[start..end]).into_owned()
}

/// 向 guest 内存写入数据，返回 (ptr, len)。
///
/// 使用 guest 导出的 `alloc` 函数分配空间。
pub fn write_to_guest(
    store: &mut wasmtime::Store<HostState>,
    memory: &wasmtime::Memory,
    alloc_fn: &wasmtime::TypedFunc<i32, i32>,
    data: &[u8],
) -> Result<(u32, u32), String> {
    let ptr = alloc_fn
        .call(&mut *store, data.len() as i32)
        .map_err(|e| format!("wasm alloc failed: {e}"))? as u32;
    let mem_data = memory.data_mut(&mut *store);
    let start = ptr as usize;
    let end = start + data.len();
    if end > mem_data.len() {
        return Err("wasm alloc returned out-of-bounds pointer".into());
    }
    mem_data[start..end].copy_from_slice(data);
    Ok((ptr, data.len() as u32))
}
