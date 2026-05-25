//! WASM 扩展协议 — 宿主状态、内存读写、host import 注册。
//!
//! s6r 协议下宿主提供 WASI 支持（`wasi_snapshot_preview1`）和三个自定义 import：
//! `host_log`、`host_emit`、`host_invoke`。
//! 工具/命令/hook 注册改由 guest 的 `extension_manifest()` 以 JSON 完成，
//! 不再需要 `host_register_tool` / `host_register_command` / `host_subscribe` /
//! `host_set_response` 等命令式副作用 import。

use std::sync::Arc;

use astrcode_core::extension::ExtensionCapability;
use wasmtime::{Caller, Linker, ResourceLimiter};
use wasmtime_wasi::WasiCtxBuilder;

use crate::host_invoke;

// ─── WASM resource limits ────────────────────────────────────────────────

const DEFAULT_WASM_FUEL: u64 = 10_000_000;
const DEFAULT_WASM_MEMORY_BYTES: usize = 64 * 1024 * 1024;

// ─── HostInvoker ─────────────────────────────────────────────────────────

/// 宿主能力的同步调用接口。
///
/// 签名：`(capability_name, input_json) -> response_json`
///
/// 实现者负责将异步能力包装为同步（通过 `Handle::block_on`）。
/// 调用发生在 `spawn_blocking` 线程上，可以安全地 block。
pub type HostInvoker = Arc<dyn Fn(&str, &str) -> String + Send + Sync>;

// ─── Host State ──────────────────────────────────────────────────────────

/// 宿主在 wasmtime `Store` 中携带的状态。
///
/// s6r 下不再需要 tools/commands/subscriptions/response 等字段——
/// 注册信息由 `extension_manifest()` 声明式返回，响应由 `extension_call()` 返回值携带。
pub struct HostState {
    /// 单次 guest 调用的 fuel 预算。
    pub fuel_budget: u64,
    /// 线性内存增长上限（字节）。
    pub memory_limit: usize,
    /// WASI preview1 上下文，支持 wasm32-wasip1 编译的 guest 插件。
    wasi_ctx: wasmtime_wasi::p1::WasiP1Ctx,
    /// 全局宿主能力后端（由 server 注入）。`extension_manifest` 完成前为 None。
    pub invoker: Option<HostInvoker>,
    /// manifest 声明的能力；与 `ExtensionRunner` 的 per-extension `allows` 同源。
    pub declared_capabilities: Vec<ExtensionCapability>,
}

impl HostState {
    pub fn new() -> Self {
        Self {
            fuel_budget: DEFAULT_WASM_FUEL,
            memory_limit: DEFAULT_WASM_MEMORY_BYTES,
            wasi_ctx: WasiCtxBuilder::new().build_p1(),
            invoker: None,
            declared_capabilities: Vec::new(),
        }
    }

    pub fn with_limits(mut self, fuel: u64, memory_bytes: usize) -> Self {
        self.fuel_budget = fuel;
        self.memory_limit = memory_bytes;
        self
    }

    /// `extension_manifest` 成功后绑定声明与宿主后端。
    pub fn finish_manifest(
        &mut self,
        declared: Vec<ExtensionCapability>,
        invoker: Option<HostInvoker>,
    ) {
        self.declared_capabilities = declared;
        self.invoker = invoker;
    }
}

impl Default for HostState {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceLimiter for HostState {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        let allowed = desired <= self.memory_limit;
        if !allowed {
            tracing::warn!(
                desired_bytes = desired,
                limit_bytes = self.memory_limit,
                "wasm extension exceeded memory limit"
            );
        }
        Ok(allowed)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        const TABLE_ENTRY_LIMIT: usize = 1024;
        let allowed = desired <= TABLE_ENTRY_LIMIT;
        if !allowed {
            tracing::warn!(
                desired_entries = desired,
                limit = TABLE_ENTRY_LIMIT,
                "wasm extension exceeded table entry limit"
            );
        }
        Ok(allowed)
    }
}

// ─── Memory helpers ──────────────────────────────────────────────────────

/// 从 `Caller` 的线性内存中读取字符串（在 host import 函数内部使用）。
fn read_caller_string(caller: &mut Caller<'_, HostState>, ptr: u32, len: u32) -> String {
    if len == 0 {
        return String::new();
    }
    let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
        tracing::warn!("wasm guest: memory export not found");
        return String::new();
    };
    let data = mem.data(caller);
    let start = ptr as usize;
    let end = start.saturating_add(len as usize);
    if end > data.len() {
        tracing::warn!(ptr, len, mem_size = data.len(), "wasm guest: out-of-bounds memory read");
        return String::new();
    }
    String::from_utf8_lossy(&data[start..end]).into_owned()
}

/// 从 `Store` 的线性内存中按 `(ptr, len)` 读取字符串。
///
/// 用于 guest 函数**返回后**从 packed i64 中取响应 JSON。
/// 调用方在读取完毕后必须调用 guest 的 `dealloc(ptr, len)` 释放内存。
pub fn read_str_from_memory(
    store: &wasmtime::Store<HostState>,
    memory: &wasmtime::Memory,
    ptr: u32,
    len: u32,
) -> Result<String, String> {
    if len == 0 {
        return Ok(String::new());
    }
    let data = memory.data(store);
    let start = ptr as usize;
    let end = start.checked_add(len as usize).ok_or("ptr+len overflow")?;
    if end > data.len() {
        return Err(format!(
            "out-of-bounds read: ptr={ptr}, len={len}, mem_size={}",
            data.len()
        ));
    }
    Ok(String::from_utf8_lossy(&data[start..end]).into_owned())
}

// ─── Host import: host_emit ───────────────────────────────────────────────

/// `host_emit` 的宿主侧实现占位。
///
/// 实际的 emit 逻辑需要访问 `ExtensionEventSink`，它不能直接放在 HostState 中
/// （trait object 生命周期复杂）。当前以 warn 日志代替，供 EmitEvents capability
/// 的完整实现者替换。
///
/// TODO: 通过 Arc<dyn ExtensionEventSink> 注入到 HostState 并在此调用。
fn host_emit_stub(
    mut caller: Caller<'_, HostState>,
    event_ptr: i32,
    event_len: i32,
) -> i64 {
    let json = read_caller_string(&mut caller, event_ptr as u32, event_len as u32);
    tracing::warn!(target: "wasm_ext", "host_emit called but not fully implemented: {json}");
    0_i64 // 返回 0 表示失败（packed null）
}

fn host_log(mut caller: Caller<'_, HostState>, level: i32, msg_ptr: i32, msg_len: i32) {
    let msg = read_caller_string(&mut caller, msg_ptr as u32, msg_len as u32);
    match level {
        0 => tracing::trace!(target: "wasm_ext", "{}", msg),
        1 => tracing::debug!(target: "wasm_ext", "{}", msg),
        3 => tracing::warn!(target: "wasm_ext",  "{}", msg),
        4 => tracing::error!(target: "wasm_ext", "{}", msg),
        _ => tracing::info!(target: "wasm_ext",  "{}", msg),
    }
}

// ─── Host import: host_invoke ──────────────────────────────────────────────

/// guest 调用宿主能力的统一入口。
///
/// ABI: `host_invoke(cap_ptr, cap_len, input_ptr, input_len) -> i64`
/// 返回 packed i64 `(resp_ptr << 32 | resp_len)` 指向 guest 内存中的 ResultMsg JSON。
/// 返回 0 表示能力不存在或内部错误。
fn host_invoke(
    mut caller: Caller<'_, HostState>,
    cap_ptr: i32,
    cap_len: i32,
    input_ptr: i32,
    input_len: i32,
) -> i64 {
    let cap = read_caller_string(&mut caller, cap_ptr as u32, cap_len as u32);
    let input = read_caller_string(&mut caller, input_ptr as u32, input_len as u32);

    let resp_json = {
        let state = caller.data();
        match host_invoke::authorize(&cap, &state.declared_capabilities) {
            Err(msg) => host_invoke::err(msg),
            Ok(()) => {
                let Some(invoker) = state.invoker.as_ref() else {
                    tracing::debug!(target: "wasm_ext", cap, "host_invoke: no backend configured");
                    return 0;
                };
                invoker(&cap, &input)
            },
        }
    };
    let resp_bytes = resp_json.as_bytes();
    let resp_len = resp_bytes.len();

    let Some(alloc_export) = caller.get_export("alloc").and_then(|e| e.into_func()) else {
        tracing::warn!(target: "wasm_ext", "host_invoke: guest missing alloc export");
        return 0;
    };
    let Ok(typed_alloc) = alloc_export.typed::<i32, i32>(&caller) else {
        tracing::warn!(target: "wasm_ext", "host_invoke: alloc has wrong type");
        return 0;
    };
    let ptr = match typed_alloc.call(&mut caller, resp_len as i32) {
        Ok(p) => p as u32,
        Err(e) => {
            tracing::warn!(target: "wasm_ext", "host_invoke: guest alloc failed: {e}");
            return 0;
        },
    };

    let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
        tracing::warn!(target: "wasm_ext", "host_invoke: guest missing memory export");
        return 0;
    };
    let start = ptr as usize;
    let end = start + resp_len;
    if end > mem.data(&caller).len() {
        tracing::warn!(target: "wasm_ext", "host_invoke: response out of bounds");
        return 0;
    }
    mem.data_mut(&mut caller)[start..end].copy_from_slice(resp_bytes);

    ((ptr as i64) << 32) | (resp_len as i64)
}

// ─── Linker builder ──────────────────────────────────────────────────────

/// 创建 s6r Linker：注册 WASI 支持和自定义 `host_log` / `host_emit` / `host_invoke`。
pub fn create_linker(engine: &wasmtime::Engine) -> Result<Linker<HostState>, String> {
    let mut linker = Linker::new(engine);

    // WASI preview1 支持 — wasm32-wasip1 编译的 guest 需要
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |state: &mut HostState| &mut state.wasi_ctx)
        .map_err(|e| format!("add wasi to linker: {e}"))?;

    linker
        .func_wrap("env", "host_log", host_log)
        .map_err(|e| format!("register host_log: {e}"))?;
    linker
        .func_wrap("env", "host_emit", host_emit_stub)
        .map_err(|e| format!("register host_emit: {e}"))?;
    linker
        .func_wrap("env", "host_invoke", host_invoke)
        .map_err(|e| format!("register host_invoke: {e}"))?;
    Ok(linker)
}

// ─── Guest memory write ───────────────────────────────────────────────────

/// 通过 guest 的 `alloc` 在线性内存中分配空间并写入 `data`。
///
/// 返回 `(ptr, len)`。调用方在 guest 函数返回后必须调用 `dealloc(ptr, len)`。
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
    let end = start.checked_add(data.len()).ok_or("ptr+len overflow in write_to_guest")?;
    if end > mem_data.len() {
        return Err("wasm alloc returned out-of-bounds pointer".into());
    }
    mem_data[start..end].copy_from_slice(data);
    Ok((ptr, data.len() as u32))
}
