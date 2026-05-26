//! WASM `peer_exchange` 传输层。

use std::sync::{Arc, mpsc};

use astrcode_extension_sdk::s5r::WireMessage;
use parking_lot::Mutex;
use tokio::sync::oneshot;

use crate::{
    host_router::InvokeContext,
    wasm_api::{HostState, read_str_from_memory, write_to_guest},
};

#[derive(Debug)]
pub struct TransportError(pub String);

/// wasmtime 运行时句柄（与 guest 线程共享）。
pub struct WasmGuestRuntime {
    pub store: wasmtime::Store<HostState>,
    pub memory: wasmtime::Memory,
    pub alloc_fn: wasmtime::TypedFunc<i32, i32>,
    pub dealloc_fn: wasmtime::TypedFunc<(i32, i32), ()>,
    pub exchange_fn: wasmtime::TypedFunc<(i32, i32), i64>,
}

/// 发往 guest 线程的交换任务。
pub struct TransportJob {
    pub request_json: String,
    pub invoke_ctx: InvokeContext,
    pub reply: oneshot::Sender<Result<WireMessage, TransportError>>,
}

/// 在专用 OS 线程上串行执行 `peer_exchange`。
pub struct WasmPeerTransport;

impl WasmPeerTransport {
    pub fn spawn(
        inner: Arc<Mutex<WasmGuestRuntime>>,
        extension_id: &str,
    ) -> Result<mpsc::Sender<TransportJob>, String> {
        let (job_tx, job_rx): (mpsc::Sender<TransportJob>, mpsc::Receiver<TransportJob>) =
            mpsc::channel();
        let thread_name = format!("wasm-peer-{extension_id}");
        std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let result = exchange_blocking(&inner, &job.request_json, &job.invoke_ctx);
                    let _ = job.reply.send(result);
                }
            })
            .map_err(|e| format!("spawn wasm peer thread: {e}"))?;
        Ok(job_tx)
    }
}

/// 宿主在 guest 线程上调用 guest 的 `peer_exchange` export。
pub fn exchange_blocking(
    inner: &Mutex<WasmGuestRuntime>,
    request_json: &str,
    invoke_ctx: &InvokeContext,
) -> Result<WireMessage, TransportError> {
    let mut guard = inner.lock();
    let mut wasm_ctx = invoke_ctx.clone();
    wasm_ctx.on_wasm_peer_thread = true;
    guard.store.data_mut().set_invoke_context(&wasm_ctx);
    let fuel_budget = guard.store.data().fuel_budget;
    guard
        .store
        .set_fuel(fuel_budget)
        .map_err(|e| TransportError(format!("set_fuel: {e}")))?;

    let memory = guard.memory;
    let alloc_fn = guard.alloc_fn.clone();
    let dealloc_fn = guard.dealloc_fn.clone();
    let exchange_fn = guard.exchange_fn.clone();

    let (req_ptr, req_len) = write_to_guest(
        &mut guard.store,
        &memory,
        &alloc_fn,
        request_json.as_bytes(),
    )
    .map_err(TransportError)?;

    let packed = exchange_fn
        .call(&mut guard.store, (req_ptr as i32, req_len as i32))
        .map_err(|e| TransportError(format!("peer_exchange trap: {e}")))?;

    let _ = dealloc_fn.call(&mut guard.store, (req_ptr as i32, req_len as i32));

    if packed == 0 {
        guard.store.data_mut().clear_invoke_context();
        return Err(TransportError("peer_exchange returned null".into()));
    }

    let resp_ptr = ((packed >> 32) & 0xFFFF_FFFF) as u32;
    let resp_len = (packed & 0xFFFF_FFFF) as u32;
    let resp_json =
        read_str_from_memory(&guard.store, &memory, resp_ptr, resp_len).map_err(TransportError)?;
    let _ = dealloc_fn.call(&mut guard.store, (resp_ptr as i32, resp_len as i32));

    let result = serde_json::from_str(&resp_json).map_err(|e| TransportError(e.to_string()));
    guard.store.data_mut().clear_invoke_context();
    result
}
