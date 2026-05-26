//! IPC 子进程 JSON-RPC 会话（stdio JSONL）。

use std::{
    collections::HashMap,
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};

use astrcode_core::extension::{ExtensionCapability, ExtensionError, ExtensionEventDecl};
use astrcode_extension_sdk::s5r::effects::HandlerResult;
use astrcode_protocol::framing::{JsonRpcMessage, from_jsonl_line, to_jsonl_line};
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex as AsyncMutex, oneshot},
    task::JoinHandle,
};

use crate::{
    extension_manifest::ExtensionRegistration,
    host_router::{HostRouter, InvokeContext, decls_to_map},
    ipc_ext::protocol::{
        IPC_VERSION, METHOD_HANDLER_INVOKE, METHOD_HOST_INVOKE, METHOD_INITIALIZE, METHOD_PING,
        METHOD_SHUTDOWN,
    },
};

const MAX_REENTRANCY: u32 = 8;
const MAX_CONTINUATION_DEPTH: u32 = 16;
const IPC_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const HOST_INVOKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

type PendingMap = HashMap<u64, oneshot::Sender<Result<Value, IpcSessionError>>>;

struct ReadLoopContext {
    writer: Arc<AsyncMutex<tokio::process::ChildStdin>>,
    pending: Arc<AsyncMutex<PendingMap>>,
    router: Arc<HostRouter>,
    registration: Arc<RwLock<Option<ExtensionRegistration>>>,
    reentrancy: Arc<AtomicU32>,
    active_invoke: Arc<RwLock<Option<InvokeContext>>>,
    default_working_dir: Arc<RwLock<Option<String>>>,
}

/// 在 `handler.invoke` 请求期间保持 `active_invoke`，供嵌套 `host/invoke` 读取。
///
/// 正确性依赖 `request_lock` 串行化所有 outbound RPC：`read_loop` 只在某个
/// `request().await` 窗口内会看到非空的 `active_invoke`，且同一时刻最多一个此类窗口。
struct ActiveInvokeGuard {
    active_invoke: Arc<RwLock<Option<InvokeContext>>>,
}

impl ActiveInvokeGuard {
    fn set(active_invoke: &Arc<RwLock<Option<InvokeContext>>>, ctx: InvokeContext) -> Self {
        *active_invoke.write() = Some(ctx);
        Self {
            active_invoke: Arc::clone(active_invoke),
        }
    }
}

impl Drop for ActiveInvokeGuard {
    fn drop(&mut self) {
        *self.active_invoke.write() = None;
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum IpcSessionError {
    #[error("{0}")]
    Msg(String),
    #[error("IPC request timed out")]
    Timeout,
    #[error("IPC process exited")]
    ProcessExited,
}

pub(crate) struct IpcSession {
    child: Mutex<Option<Child>>,
    writer: Arc<AsyncMutex<tokio::process::ChildStdin>>,
    pending: Arc<AsyncMutex<PendingMap>>,
    next_id: AtomicU64,
    request_lock: AsyncMutex<()>,
    registration: Arc<RwLock<Option<ExtensionRegistration>>>,
    /// 当前 `handler.invoke` 的上下文，供嵌套 `host/invoke` 使用。
    active_invoke: Arc<RwLock<Option<InvokeContext>>>,
    read_task: JoinHandle<()>,
    stderr_tail: Arc<Mutex<String>>,
}

impl IpcSession {
    pub async fn spawn(
        program: &str,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
        router: Arc<HostRouter>,
        extension_dir: &Path,
        working_dir: Option<&str>,
    ) -> Result<Arc<Self>, String> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn IPC extension {program}: {e}"))?;
        let stdin = child.stdin.take().ok_or("IPC child missing stdin")?;
        let stdout = child.stdout.take().ok_or("IPC child missing stdout")?;
        let stderr = child.stderr.take().ok_or("IPC child missing stderr")?;

        let pending: Arc<AsyncMutex<PendingMap>> = Arc::new(AsyncMutex::new(HashMap::new()));
        let registration = Arc::new(RwLock::new(None::<ExtensionRegistration>));
        let reentrancy = Arc::new(AtomicU32::new(0));
        let default_working_dir = Arc::new(RwLock::new(working_dir.map(str::to_string)));
        let active_invoke = Arc::new(RwLock::new(None));
        let stderr_tail = Arc::new(Mutex::new(String::new()));

        let writer = Arc::new(AsyncMutex::new(stdin));

        let read_task = tokio::spawn({
            let writer = Arc::clone(&writer);
            let pending = Arc::clone(&pending);
            let registration = Arc::clone(&registration);
            let active_invoke = Arc::clone(&active_invoke);
            async move {
                read_loop(
                    stdout,
                    ReadLoopContext {
                        writer,
                        pending,
                        router,
                        registration,
                        reentrancy,
                        active_invoke,
                        default_working_dir,
                    },
                )
                .await;
            }
        });

        tokio::spawn(drain_stderr(stderr, Arc::clone(&stderr_tail)));

        let session = Arc::new(Self {
            child: Mutex::new(Some(child)),
            writer,
            pending,
            next_id: AtomicU64::new(1),
            request_lock: AsyncMutex::new(()),
            registration: Arc::clone(&registration),
            active_invoke: Arc::clone(&active_invoke),
            read_task,
            stderr_tail,
        });

        let init_params = json!({
            "protocol_version": IPC_VERSION,
            "extension_dir": extension_dir.to_string_lossy(),
            "working_dir": working_dir,
        });
        let init_result = session
            .request(METHOD_INITIALIZE, init_params, None)
            .await
            .map_err(|e| format!("extension/initialize: {e}"))?;
        let reg = crate::extension_manifest::registration_from_ipc(&init_result, IPC_VERSION)
            .map_err(|e| format!("parse initialize result: {e}"))?;
        {
            let mut g = session.registration.write();
            *g = Some(reg);
        }

        Ok(session)
    }

    pub fn registration(&self) -> Option<ExtensionRegistration> {
        self.registration.read().clone()
    }

    pub fn extension_id(&self) -> String {
        self.registration
            .read()
            .as_ref()
            .map(|r| r.extension_id.clone())
            .unwrap_or_default()
    }

    pub fn declared_capabilities(&self) -> Vec<ExtensionCapability> {
        self.registration
            .read()
            .as_ref()
            .map(|r| r.capabilities.clone())
            .unwrap_or_default()
    }

    pub fn event_decls(&self) -> HashMap<String, ExtensionEventDecl> {
        self.registration
            .read()
            .as_ref()
            .map(|r| decls_to_map(&r.extension_events))
            .unwrap_or_default()
    }

    pub async fn ping(&self) -> Result<(), IpcSessionError> {
        self.request(METHOD_PING, json!({}), None).await?;
        Ok(())
    }

    pub async fn shutdown(&self) {
        let _ = self.notify(METHOD_SHUTDOWN, json!({})).await;
        let child = self.child.lock().take();
        if let Some(mut child) = child {
            let _ = child.kill().await;
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), IpcSessionError> {
        let msg = JsonRpcMessage {
            jsonrpc: "2.0".into(),
            id: None,
            method: Some(method.into()),
            params: Some(params),
            result: None,
            error: None,
        };
        let line = to_jsonl_line(&msg).map_err(|e| IpcSessionError::Msg(e.to_string()))?;
        let mut writer = self.writer.lock().await;
        writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| IpcSessionError::Msg(format!("write notify: {e}")))?;
        writer
            .flush()
            .await
            .map_err(|e| IpcSessionError::Msg(format!("flush notify: {e}")))?;
        Ok(())
    }

    pub async fn request(
        &self,
        method: &str,
        params: Value,
        invoke_ctx: Option<&InvokeContext>,
    ) -> Result<Value, IpcSessionError> {
        let _guard = self.request_lock.lock().await;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let msg = JsonRpcMessage {
            jsonrpc: "2.0".into(),
            id: Some(id),
            method: Some(method.into()),
            params: Some(params),
            result: None,
            error: None,
        };
        let line = to_jsonl_line(&msg).map_err(|e| IpcSessionError::Msg(e.to_string()))?;
        {
            let mut writer = self.writer.lock().await;
            writer
                .write_all(line.as_bytes())
                .await
                .map_err(|e| IpcSessionError::Msg(format!("write request: {e}")))?;
            writer
                .flush()
                .await
                .map_err(|e| IpcSessionError::Msg(format!("flush request: {e}")))?;
        }

        let _active_invoke_guard =
            invoke_ctx.map(|ctx| ActiveInvokeGuard::set(&self.active_invoke, ctx.clone()));

        match tokio::time::timeout(IPC_REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&id);
                Err(self.process_exited_error())
            },
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(IpcSessionError::Timeout)
            },
        }
    }

    pub async fn invoke_handler(
        &self,
        handler_id: &str,
        event: Value,
        invoke_ctx: &InvokeContext,
    ) -> Result<HandlerResult, ExtensionError> {
        let params = json!({
            "handler_id": handler_id,
            "event": event,
            "caller_extension_id": self.extension_id(),
        });
        let output = self
            .request(METHOD_HANDLER_INVOKE, params, Some(invoke_ctx))
            .await
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        serde_json::from_value(output)
            .map_err(|e| ExtensionError::Internal(format!("parse HandlerResult: {e}")))
    }

    fn process_exited_error(&self) -> IpcSessionError {
        let tail = self.stderr_tail.lock().clone();
        if tail.is_empty() {
            IpcSessionError::ProcessExited
        } else {
            IpcSessionError::Msg(format!("IPC process exited; stderr tail:\n{tail}"))
        }
    }

    pub async fn invoke_handler_with_continuations(
        &self,
        handler_id: &str,
        event: Value,
        invoke_ctx: &InvokeContext,
    ) -> Result<HandlerResult, ExtensionError> {
        let mut stack = vec![(handler_id.to_string(), event, 0u32)];
        let mut first: Option<HandlerResult> = None;

        while let Some((hid, ev, depth)) = stack.pop() {
            if depth > MAX_CONTINUATION_DEPTH {
                return Err(ExtensionError::Internal(format!(
                    "continuation depth exceeded (max {MAX_CONTINUATION_DEPTH})"
                )));
            }
            let resp = self.invoke_handler(&hid, ev, invoke_ctx).await?;
            if first.is_none() {
                first = Some(resp.clone());
            }
            for cont in resp.continuations.iter().rev() {
                let (nh, ne) = cont.handler_id_for_extension(&self.extension_id());
                stack.push((nh, ne, depth + 1));
            }
        }
        first.ok_or_else(|| ExtensionError::Internal("empty handler chain".into()))
    }
}

impl Drop for IpcSession {
    fn drop(&mut self) {
        self.read_task.abort();
        if let Some(mut child) = self.child.lock().take() {
            let _ = child.start_kill();
        }
    }
}

async fn read_loop(stdout: tokio::process::ChildStdout, ctx: ReadLoopContext) {
    let ReadLoopContext {
        writer,
        pending,
        router,
        registration,
        reentrancy,
        active_invoke,
        default_working_dir,
    } = ctx;
    let mut reader = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: JsonRpcMessage = match from_jsonl_line(line) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "IPC: invalid JSONL from extension");
                continue;
            },
        };

        if let Some(id) = msg.id {
            if msg.method.is_none() {
                let result = if let Some(err) = msg.error {
                    Err(IpcSessionError::Msg(format!(
                        "JSON-RPC {}: {}",
                        err.code, err.message
                    )))
                } else {
                    Ok(msg.result.unwrap_or(Value::Null))
                };
                if let Some(tx) = pending.lock().await.remove(&id) {
                    let _ = tx.send(result);
                }
                continue;
            }
        }

        if msg.method.as_deref() == Some(METHOD_HOST_INVOKE) {
            if let Some(id) = msg.id {
                let router = Arc::clone(&router);
                let registration = Arc::clone(&registration);
                let reentrancy = Arc::clone(&reentrancy);
                let active_invoke = Arc::clone(&active_invoke);
                let default_working_dir = Arc::clone(&default_working_dir);
                let params = msg.params;
                let blocking = tokio::task::spawn_blocking(move || {
                    handle_host_invoke(
                        &router,
                        &registration,
                        &reentrancy,
                        &active_invoke,
                        &default_working_dir,
                        params,
                    )
                });
                let response = match tokio::time::timeout(HOST_INVOKE_TIMEOUT, blocking).await {
                    Ok(Ok(result)) => result,
                    Ok(Err(e)) => Err(format!("host/invoke task: {e}")),
                    Err(_) => Err(format!(
                        "host/invoke timed out after {}s",
                        HOST_INVOKE_TIMEOUT.as_secs()
                    )),
                };
                let resp_msg = match response {
                    Ok(result) => JsonRpcMessage {
                        jsonrpc: "2.0".into(),
                        id: Some(id),
                        method: None,
                        params: None,
                        result: Some(result),
                        error: None,
                    },
                    Err(e) => JsonRpcMessage {
                        jsonrpc: "2.0".into(),
                        id: Some(id),
                        method: None,
                        params: None,
                        result: None,
                        error: Some(astrcode_protocol::framing::JsonRpcError {
                            code: -32000,
                            message: e,
                            data: None,
                        }),
                    },
                };
                if let Ok(line) = to_jsonl_line(&resp_msg) {
                    let mut w = writer.lock().await;
                    let _ = w.write_all(line.as_bytes()).await;
                    let _ = w.flush().await;
                }
            }
        }
    }

    let mut guard = pending.lock().await;
    for (_, tx) in guard.drain() {
        let _ = tx.send(Err(IpcSessionError::ProcessExited));
    }
}

fn handle_host_invoke(
    router: &HostRouter,
    registration: &Arc<RwLock<Option<ExtensionRegistration>>>,
    reentrancy: &Arc<AtomicU32>,
    active_invoke: &Arc<RwLock<Option<InvokeContext>>>,
    default_working_dir: &Arc<RwLock<Option<String>>>,
    params: Option<Value>,
) -> Result<Value, String> {
    let params = params.ok_or("host/invoke missing params")?;
    let capability = params
        .get("capability")
        .and_then(|v| v.as_str())
        .ok_or("host/invoke missing capability")?;
    let input = params.get("input").cloned().unwrap_or(Value::Null);
    let stream = params
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let depth = reentrancy.fetch_add(1, Ordering::SeqCst);
    if depth >= MAX_REENTRANCY {
        reentrancy.fetch_sub(1, Ordering::SeqCst);
        return Err("reentrancy depth exceeded".into());
    }

    let reg = registration.read().clone();
    let Some(reg) = reg else {
        reentrancy.fetch_sub(1, Ordering::SeqCst);
        return Err("extension not initialized".into());
    };

    let mut ctx = active_invoke.read().clone().unwrap_or_default();
    if ctx.working_dir.is_none() {
        ctx.working_dir = default_working_dir.read().clone();
    }
    ctx.extension_id = reg.extension_id.clone();
    ctx.declared_capabilities = reg.capabilities.clone();
    ctx.event_declarations = decls_to_map(&reg.extension_events);
    // 嵌套 host/invoke 发生在 handler.invoke 等待窗口内；此时 outbound RPC 被
    // request_lock 串行化，禁止 wait_for_result 以免与同一子进程形成死锁。
    ctx.on_peer_io_thread = true;

    let result = if stream {
        router
            .invoke_stream_sync(capability, &input.to_string(), "ipc-stream", &ctx)
            .map(|events| json!({ "events": events }))
            .map_err(|e| e.message)
    } else {
        router
            .invoke_sync(capability, &input.to_string(), &ctx)
            .map(|v| json!({ "output": v }))
            .map_err(|e| e.message)
    };

    reentrancy.fetch_sub(1, Ordering::SeqCst);
    result
}

async fn drain_stderr(stderr: tokio::process::ChildStderr, tail: Arc<Mutex<String>>) {
    let mut reader = BufReader::new(stderr).lines();
    const MAX_TAIL: usize = 8192;
    while let Ok(Some(line)) = reader.next_line().await {
        let mut guard = tail.lock();
        if !guard.is_empty() {
            guard.push('\n');
        }
        guard.push_str(&line);
        if guard.len() > MAX_TAIL {
            let drop_len = guard.len() - MAX_TAIL;
            guard.drain(..drop_len);
        }
    }
}
