//! WASM 扩展协议 — 宿主状态、内存读写、host import 注册。

use std::sync::Arc;

use astrcode_extension_sdk::s5r::WireMessage;
use wasmtime::{Caller, Linker, ResourceLimiter};
use wasmtime_wasi::WasiCtxBuilder;

use crate::{extension_peer::ExtensionPeer, host_router::InvokeContext};

const DEFAULT_WASM_FUEL: u64 = 10_000_000;
const DEFAULT_WASM_MEMORY_BYTES: usize = 64 * 1024 * 1024;

/// 宿主在 wasmtime `Store` 中携带的状态。
pub struct HostState {
    pub fuel_budget: u64,
    pub memory_limit: usize,
    wasi_ctx: wasmtime_wasi::p1::WasiP1Ctx,
    /// 对等会话（握手完成后设置）。
    peer: Option<Arc<ExtensionPeer>>,
    /// 当前 guest 调用栈上的 invoke 上下文（供嵌套 host import 使用）。
    invoke_ctx: Option<InvokeContext>,
}

impl HostState {
    pub fn new() -> Self {
        Self {
            fuel_budget: DEFAULT_WASM_FUEL,
            memory_limit: DEFAULT_WASM_MEMORY_BYTES,
            wasi_ctx: WasiCtxBuilder::new().build_p1(),
            peer: None,
            invoke_ctx: None,
        }
    }

    pub fn with_limits(mut self, fuel: u64, memory_bytes: usize) -> Self {
        self.fuel_budget = fuel;
        self.memory_limit = memory_bytes;
        self
    }

    pub fn set_peer(&mut self, peer: Arc<ExtensionPeer>) {
        self.peer = Some(peer);
    }

    pub fn set_invoke_context(&mut self, ctx: &InvokeContext) {
        self.invoke_ctx = Some(ctx.clone());
    }

    pub fn clear_invoke_context(&mut self) {
        self.invoke_ctx = None;
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
        Ok(desired <= self.memory_limit)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        const TABLE_ENTRY_LIMIT: usize = 1024;
        Ok(desired <= TABLE_ENTRY_LIMIT)
    }
}

fn read_caller_string(caller: &mut Caller<'_, HostState>, ptr: u32, len: u32) -> String {
    if len == 0 {
        return String::new();
    }
    let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
        return String::new();
    };
    let data = mem.data(caller);
    let start = ptr as usize;
    let end = start.saturating_add(len as usize);
    if end > data.len() {
        return String::new();
    }
    String::from_utf8_lossy(&data[start..end]).into_owned()
}

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
        return Err(format!("out-of-bounds read: ptr={ptr}, len={len}"));
    }
    Ok(String::from_utf8_lossy(&data[start..end]).into_owned())
}

fn write_result_to_guest(caller: &mut Caller<'_, HostState>, resp_bytes: &[u8]) -> i64 {
    let resp_len = resp_bytes.len();
    let Some(alloc_export) = caller.get_export("alloc").and_then(|e| e.into_func()) else {
        return 0;
    };
    let Ok(typed_alloc) = alloc_export.typed::<i32, i32>(&*caller) else {
        return 0;
    };
    let ptr = match typed_alloc.call(&mut *caller, resp_len as i32) {
        Ok(p) => p as u32,
        Err(_) => return 0,
    };

    let Some(mem) = caller.get_export("memory").and_then(|e| e.into_memory()) else {
        return 0;
    };
    let start = ptr as usize;
    let end = start + resp_len;
    if end > mem.data(&*caller).len() {
        return 0;
    }
    mem.data_mut(&mut *caller)[start..end].copy_from_slice(resp_bytes);
    ((ptr as i64) << 32) | (resp_len as i64)
}

fn host_log(mut caller: Caller<'_, HostState>, level: i32, msg_ptr: i32, msg_len: i32) {
    let msg = read_caller_string(&mut caller, msg_ptr as u32, msg_len as u32);
    match level {
        0 => tracing::trace!(target: "wasm_ext", "{msg}"),
        1 => tracing::debug!(target: "wasm_ext", "{msg}"),
        3 => tracing::warn!(target: "wasm_ext", "{msg}"),
        4 => tracing::error!(target: "wasm_ext", "{msg}"),
        _ => tracing::info!(target: "wasm_ext", "{msg}"),
    }
}

/// guest→host 统一 RPC 入口（与 guest export `peer_exchange` 对称）。
fn peer_exchange_import(mut caller: Caller<'_, HostState>, req_ptr: i32, req_len: i32) -> i64 {
    let json = read_caller_string(&mut caller, req_ptr as u32, req_len as u32);
    let resp_json = {
        let state = caller.data();
        let Some(peer_mtx) = state.peer.as_ref() else {
            return write_result_to_guest(
                &mut caller,
                r#"{"type":"result","id":"?","kind":"invoke_result","success":false,"error":{"code":"not_ready","message":"peer not initialized","retryable":false}}"#
                    .as_bytes(),
            );
        };
        let Some(ctx) = state.invoke_ctx.as_ref() else {
            return write_result_to_guest(
                &mut caller,
                r#"{"type":"result","id":"?","kind":"invoke_result","success":false,"error":{"code":"no_context","message":"invoke context missing","retryable":false}}"#
                    .as_bytes(),
            );
        };
        let msg: WireMessage = match serde_json::from_str(&json) {
            Ok(m) => m,
            Err(e) => {
                let err = format!(
                    r#"{{"type":"result","id":"?","kind":"invoke_result","success":false,"error":{{"code":"invalid_message","message":"{e}","retryable":false}}}}"#
                );
                return write_result_to_guest(&mut caller, err.as_bytes());
            },
        };
        match peer_mtx.handle_inbound_sync(msg, ctx) {
            Ok(resp) => match serde_json::to_string(&resp) {
                Ok(s) => s,
                Err(e) => format!(
                    r#"{{"type":"result","id":"?","kind":"invoke_result","success":false,"error":{{"code":"internal","message":"{e}","retryable":false}}}}"#
                ),
            },
            Err(e) => format!(
                r#"{{"type":"result","id":"?","kind":"invoke_result","success":false,"error":{{"code":"transport","message":"{}","retryable":false}}}}"#,
                e.0
            ),
        }
    };
    write_result_to_guest(&mut caller, resp_json.as_bytes())
}

pub fn create_linker(engine: &wasmtime::Engine) -> Result<Linker<HostState>, String> {
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |state: &mut HostState| &mut state.wasi_ctx)
        .map_err(|e| format!("add wasi to linker: {e}"))?;
    linker
        .func_wrap("env", "host_log", host_log)
        .map_err(|e| format!("register host_log: {e}"))?;
    linker
        .func_wrap("env", "peer_exchange", peer_exchange_import)
        .map_err(|e| format!("register peer_exchange: {e}"))?;
    Ok(linker)
}

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
    let end = start
        .checked_add(data.len())
        .ok_or("ptr+len overflow in write_to_guest")?;
    if end > mem_data.len() {
        return Err("wasm alloc returned out-of-bounds pointer".into());
    }
    mem_data[start..end].copy_from_slice(data);
    Ok((ptr, data.len() as u32))
}
