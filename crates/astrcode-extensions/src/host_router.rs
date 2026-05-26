//! 宿主能力路由 — 唯一实现 `astrcode.*` RPC 与扩展事件发射。

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use astrcode_core::{
    event::EventPayload,
    extension::{ExtensionCapability, ExtensionError, ExtensionEventDecl, ExtensionHostServices},
    llm::{LlmContent, LlmEvent, LlmMessage, LlmProvider, LlmRole},
    tool::{CreateSessionRequest, SessionOperations, SubmitTurnRequest, SubmitTurnResult},
};
use astrcode_extension_sdk::{
    s5r::{CapabilityDescriptor, ErrorPayload, EventMsg, EventPhase, WireMessage},
    state,
};
use astrcode_support::hostpaths::{WorkspacePathError, resolve_under_workspace_root};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const HOST_INVOKE_TIMEOUT: Duration = Duration::from_secs(30);
/// `workspace.read` 默认最大读取字节数（1 MiB）。
const DEFAULT_WORKSPACE_READ_MAX_BYTES: u64 = 1024 * 1024;

fn block_on_async<F: std::future::Future + Send + 'static>(future: F) -> F::Output
where
    F::Output: Send + 'static,
{
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    static BLOCK_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let rt = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("host router tokio runtime")
    });

    // 从 tokio 异步任务里直接 block_on 会占满 test/runtime worker，嵌套 host invoke 会死锁。
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::spawn(move || {
            let _guard = BLOCK_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            rt.block_on(future)
        })
        .join()
        .expect("block_on_async worker thread panicked")
    } else {
        let _guard = BLOCK_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        rt.block_on(future)
    }
}

/// 单次 guest→host invoke 的运行时上下文。
#[derive(Clone, Default)]
pub struct InvokeContext {
    pub extension_id: String,
    pub session_id: Option<String>,
    pub session_store_dir: Option<PathBuf>,
    pub session_ops: Option<Arc<dyn SessionOperations>>,
    pub event_tx: Option<mpsc::UnboundedSender<EventPayload>>,
    pub working_dir: Option<String>,
    pub cancel_token: Option<CancellationToken>,
    pub event_declarations: HashMap<String, ExtensionEventDecl>,
    pub declared_capabilities: Vec<ExtensionCapability>,
    /// 当前调用是否在 peer 专用 I/O 线程上（同步 host import；IPC 子进程共用）。
    pub on_peer_io_thread: bool,
}

const MAX_READ_EVENTS_LIMIT: usize = 500;

/// 宿主后端依赖。
#[derive(Default)]
pub struct HostBackends {
    pub small_llm: Option<Arc<dyn LlmProvider>>,
    pub session_read: Option<Arc<dyn astrcode_core::storage::EventReader>>,
    pub default_working_dir: Option<String>,
}

/// 唯一 `astrcode.*` 能力实现。
pub struct HostRouter {
    backends: HostBackends,
}

impl HostRouter {
    pub fn new(host_services: &ExtensionHostServices, default_working_dir: Option<String>) -> Self {
        Self {
            backends: HostBackends {
                small_llm: host_services.small_llm.clone(),
                session_read: host_services.session_read.clone(),
                default_working_dir,
            },
        }
    }

    pub fn from_backends(backends: HostBackends) -> Self {
        Self { backends }
    }

    /// 根据已声明能力生成握手 catalog。
    pub fn catalog_for_grants(caps: &[ExtensionCapability]) -> Vec<CapabilityDescriptor> {
        let mut out = Vec::new();
        for cap in caps {
            out.extend(descriptors_for_capability(*cap));
        }
        out
    }

    pub fn authorize_astrcode(
        cap: &str,
        declared: &[ExtensionCapability],
    ) -> Result<(), ErrorPayload> {
        let required = required_capability_for_astrcode(cap);
        let Some(required) = required else {
            return Err(ErrorPayload::new(
                "unknown_capability",
                format!("unknown astrcode capability: {cap}"),
            ));
        };
        if declared.contains(&required) {
            Ok(())
        } else {
            Err(ErrorPayload::new(
                "permission_denied",
                format!(
                    "{} requires declared capability {}",
                    cap,
                    capability_wire_name(required)
                ),
            ))
        }
    }

    /// 同步 invoke（IPC guest 线程调用）。流式能力在内部收集后一次性返回。
    pub fn invoke_sync(
        &self,
        cap: &str,
        input: &str,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        if let Some(token) = &ctx.cancel_token {
            if token.is_cancelled() {
                return Err(ErrorPayload::new("cancelled", "invoke cancelled"));
            }
        }
        Self::authorize_astrcode(cap, &ctx.declared_capabilities)?;

        let input: Value = serde_json::from_str(input)
            .map_err(|e| ErrorPayload::new("invalid_input", e.to_string()))?;

        match cap {
            "astrcode.llm.small_chat" => self.invoke_small_llm(&input, false, ctx),
            "astrcode.session.read_events" => self.invoke_read_events(&input, ctx),
            "astrcode.session.control.create" => self.invoke_session_create(&input, ctx),
            "astrcode.session.control.submit_turn" => self.invoke_session_submit(&input, ctx),
            "astrcode.session.control.dispose" => self.invoke_session_dispose(&input, ctx),
            "astrcode.session.state.read" => self.invoke_state_read(&input, ctx),
            "astrcode.session.state.write" => self.invoke_state_write(&input, ctx),
            "astrcode.event.emit" => self.invoke_emit(&input, ctx),
            "astrcode.workspace.read" => self.invoke_workspace_read(&input, ctx),
            "astrcode.process.spawn" => Err(ErrorPayload::new(
                "not_implemented",
                "astrcode.process.spawn is reserved and not implemented in this host build",
            )),
            "astrcode.network.client" => Err(ErrorPayload::new(
                "not_implemented",
                "astrcode.network.client is reserved and not implemented in this host build",
            )),
            _ => Err(ErrorPayload::new(
                "not_implemented",
                format!("capability not implemented: {cap}"),
            )),
        }
    }

    /// 流式 invoke：返回 Event 序列（`started` + `delta*` + `completed`/`failed`）。
    pub fn invoke_stream_sync(
        &self,
        cap: &str,
        input: &str,
        request_id: &str,
        ctx: &InvokeContext,
    ) -> Result<Vec<WireMessage>, ErrorPayload> {
        if let Some(token) = &ctx.cancel_token {
            if token.is_cancelled() {
                return Err(ErrorPayload::new("cancelled", "invoke cancelled"));
            }
        }
        Self::authorize_astrcode(cap, &ctx.declared_capabilities)?;
        let input: Value = serde_json::from_str(input)
            .map_err(|e| ErrorPayload::new("invalid_input", e.to_string()))?;
        let request_id = request_id.to_string();

        match cap {
            "astrcode.llm.small_chat" => {
                let mut events = vec![WireMessage::Event(EventMsg {
                    id: request_id.clone(),
                    phase: EventPhase::Started,
                    data: Value::Null,
                    output: Value::Null,
                    error: None,
                })];
                match self.invoke_small_llm(&input, true, ctx) {
                    Ok(output) => {
                        if let Some(chunks) = output.get("chunks").and_then(|c| c.as_array()) {
                            for chunk in chunks {
                                events.push(WireMessage::Event(EventMsg {
                                    id: request_id.clone(),
                                    phase: EventPhase::Delta,
                                    data: chunk.clone(),
                                    output: Value::Null,
                                    error: None,
                                }));
                            }
                        }
                        events.push(WireMessage::Event(EventMsg {
                            id: request_id,
                            phase: EventPhase::Completed,
                            data: output.clone(),
                            output,
                            error: None,
                        }));
                        Ok(events)
                    },
                    Err(e) => {
                        events.push(WireMessage::Event(EventMsg {
                            id: request_id,
                            phase: EventPhase::Failed,
                            data: Value::Null,
                            output: Value::Null,
                            error: Some(e),
                        }));
                        Ok(events)
                    },
                }
            },
            _ => Err(ErrorPayload::new(
                "stream_not_supported",
                format!("stream not supported for {cap}"),
            )),
        }
    }

    fn invoke_small_llm(
        &self,
        input: &Value,
        collect_chunks: bool,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let provider =
            self.backends.small_llm.as_ref().ok_or_else(|| {
                ErrorPayload::new("backend_unavailable", "small_llm not configured")
            })?;

        let messages = input["messages"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let role = match m["role"].as_str()? {
                            "user" => LlmRole::User,
                            "assistant" => LlmRole::Assistant,
                            "system" => LlmRole::System,
                            _ => LlmRole::User,
                        };
                        let content = m["content"].as_str().unwrap_or("").to_string();
                        Some(LlmMessage {
                            role,
                            content: vec![LlmContent::Text { text: content }],
                            name: None,
                            reasoning_content: None,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if messages.is_empty() {
            return Err(ErrorPayload::new(
                "invalid_input",
                "messages array is empty or missing",
            ));
        }

        let cancel = ctx.cancel_token.clone();
        let provider = Arc::clone(provider);
        block_on_async(async move {
            tokio::time::timeout(
                HOST_INVOKE_TIMEOUT,
                run_small_llm(&*provider, messages, collect_chunks, cancel.as_ref()),
            )
            .await
            .map_err(|_| ErrorPayload::new("timeout", "small_llm.chat timed out"))?
        })
    }

    fn invoke_read_events(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let reader = self.backends.session_read.as_ref().ok_or_else(|| {
            ErrorPayload::new("backend_unavailable", "session_read not configured")
        })?;
        let target_session_id = input["session_id"]
            .as_str()
            .ok_or_else(|| ErrorPayload::new("invalid_input", "session_id required"))?;
        let limit = input["limit"]
            .as_u64()
            .unwrap_or(100)
            .clamp(1, MAX_READ_EVENTS_LIMIT as u64) as usize;
        let caller_session_id = ctx.session_id.as_deref().ok_or_else(|| {
            ErrorPayload::new(
                "invalid_input",
                "caller session_id required in invoke context",
            )
        })?;
        let reader = Arc::clone(reader);
        let target = target_session_id.to_string();
        let caller = caller_session_id.to_string();
        if let Some(ops) = ctx.session_ops.as_ref() {
            let ops = Arc::clone(ops);
            block_on_async(async move {
                ops.query_session(&caller, &target)
                    .await
                    .map_err(|e| ErrorPayload::new("permission_denied", e.to_string()))?;
                let sid = astrcode_core::types::SessionId::new(&target);
                reader
                    .replay_events(&sid)
                    .await
                    .map(|events| {
                        let truncated: Vec<_> = events.into_iter().take(limit).collect();
                        serde_json::json!({ "events": truncated })
                    })
                    .map_err(|e| ErrorPayload::new("read_failed", e.to_string()))
            })
        } else if caller != target {
            Err(ErrorPayload::new(
                "permission_denied",
                "session_history read is limited to the caller session without session_control",
            ))
        } else {
            let sid = astrcode_core::types::SessionId::new(&target);
            block_on_async(async move {
                reader
                    .replay_events(&sid)
                    .await
                    .map(|events| {
                        let truncated: Vec<_> = events.into_iter().take(limit).collect();
                        serde_json::json!({ "events": truncated })
                    })
                    .map_err(|e| ErrorPayload::new("read_failed", e.to_string()))
            })
        }
    }

    fn invoke_session_create(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let ops = ctx.session_ops.as_ref().ok_or_else(|| {
            ErrorPayload::new(
                "backend_unavailable",
                "session_ops not available in context",
            )
        })?;
        let req = CreateSessionRequest {
            name: input["name"].as_str().unwrap_or("child").to_string(),
            working_dir: input["working_dir"].as_str().map(str::to_string),
            system_prompt: input["system_prompt"].as_str().map(str::to_string),
            model_preference: input["model_preference"].as_str().map(str::to_string),
            tool_policy: None,
            source_extension: Some(ctx.extension_id.clone()),
            ephemeral: input["ephemeral"].as_bool().unwrap_or(false),
            tool_call_id: input["tool_call_id"].as_str().unwrap_or("").to_string(),
        };
        let parent = ctx
            .session_id
            .clone()
            .ok_or_else(|| ErrorPayload::new("invalid_input", "parent session_id required"))?;
        let ops = Arc::clone(ops);
        block_on_async(async move {
            ops.create_session(&parent, req)
                .await
                .map(|h| json!({ "session_id": h.session_id }))
                .map_err(|e| ErrorPayload::new("session_error", e.to_string()))
        })
    }

    fn invoke_session_submit(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let mut wait_for_result = input["wait_for_result"].as_bool().unwrap_or(true);
        if ctx.on_peer_io_thread && wait_for_result {
            return Err(ErrorPayload::new(
                "invalid_request",
                "wait_for_result cannot be used from peer synchronous host invokes (deadlock \
                 risk); set wait_for_result to false",
            ));
        }
        if ctx.on_peer_io_thread {
            wait_for_result = false;
        }
        let ops = ctx.session_ops.as_ref().ok_or_else(|| {
            ErrorPayload::new(
                "backend_unavailable",
                "session_ops not available in context",
            )
        })?;
        let req = SubmitTurnRequest {
            target_session_id: input["target_session_id"]
                .as_str()
                .ok_or_else(|| ErrorPayload::new("invalid_input", "target_session_id required"))?
                .to_string(),
            user_prompt: input["user_prompt"]
                .as_str()
                .ok_or_else(|| ErrorPayload::new("invalid_input", "user_prompt required"))?
                .to_string(),
            wait_for_result,
            notify_parent_on_complete: input["notify_parent_on_complete"]
                .as_str()
                .map(str::to_string),
            recycle_on_complete: input["recycle_on_complete"].as_bool().unwrap_or(false),
            tool_call_id: input["tool_call_id"].as_str().map(str::to_string),
        };
        let caller = ctx
            .session_id
            .clone()
            .ok_or_else(|| ErrorPayload::new("invalid_input", "caller session_id required"))?;
        let ops = Arc::clone(ops);
        block_on_async(async move {
            ops.submit_turn(&caller, req)
                .await
                .map(submit_turn_result_json)
                .map_err(|e| ErrorPayload::new("session_error", e.to_string()))
        })
    }

    fn invoke_session_dispose(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let ops = ctx.session_ops.as_ref().ok_or_else(|| {
            ErrorPayload::new(
                "backend_unavailable",
                "session_ops not available in context",
            )
        })?;
        let session_id = input["session_id"]
            .as_str()
            .ok_or_else(|| ErrorPayload::new("invalid_input", "session_id required"))?;
        let ops = Arc::clone(ops);
        let caller = ctx
            .session_id
            .clone()
            .ok_or_else(|| ErrorPayload::new("invalid_input", "caller session_id required"))?;
        let target = session_id.to_string();
        block_on_async(async move {
            ops.recycle_session(&caller, &target)
                .await
                .map(|()| json!({ "ok": true }))
                .map_err(|e| ErrorPayload::new("session_error", e.to_string()))
        })
    }

    fn invoke_state_read(&self, input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
        let base = ctx
            .session_store_dir
            .as_ref()
            .ok_or_else(|| ErrorPayload::new("backend_unavailable", "session_store_dir missing"))?;
        let key = input["key"]
            .as_str()
            .ok_or_else(|| ErrorPayload::new("invalid_input", "key required"))?;
        let path = state::session_data_dir(base, &ctx.extension_id).join(safe_filename(key));
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(ErrorPayload::new("io_error", error.to_string()));
            },
        };
        Ok(serde_json::json!({ "content": content }))
    }

    fn invoke_state_write(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let base = ctx
            .session_store_dir
            .as_ref()
            .ok_or_else(|| ErrorPayload::new("backend_unavailable", "session_store_dir missing"))?;
        let key = input["key"]
            .as_str()
            .ok_or_else(|| ErrorPayload::new("invalid_input", "key required"))?;
        let content = input["content"].as_str().unwrap_or("");
        let dir = state::session_data_dir(base, &ctx.extension_id);
        std::fs::create_dir_all(&dir).map_err(|e| ErrorPayload::new("io_error", e.to_string()))?;
        let path = dir.join(safe_filename(key));
        std::fs::write(&path, content).map_err(|e| ErrorPayload::new("io_error", e.to_string()))?;
        Ok(serde_json::json!({ "ok": true }))
    }

    fn invoke_emit(&self, input: &Value, ctx: &InvokeContext) -> Result<Value, ErrorPayload> {
        let event_type = input["event_type"]
            .as_str()
            .ok_or_else(|| ErrorPayload::new("invalid_input", "event_type required"))?;
        let schema_version = input["schema_version"].as_u64().unwrap_or(1) as u32;
        let payload = input.get("payload").cloned().unwrap_or(Value::Null);
        let tx = ctx.event_tx.as_ref().ok_or_else(|| {
            ErrorPayload::new("backend_unavailable", "event_tx not configured in context")
        })?;
        emit_for_sink(
            &ctx.extension_id,
            &ctx.event_declarations,
            tx,
            event_type,
            schema_version,
            payload,
        )
        .map_err(|e| ErrorPayload::new("emit_failed", e.to_string()))?;
        Ok(serde_json::json!({ "ok": true }))
    }

    fn invoke_workspace_read(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let root = ctx
            .working_dir
            .as_deref()
            .or(self.backends.default_working_dir.as_deref())
            .ok_or_else(|| ErrorPayload::new("backend_unavailable", "working_dir not set"))?;
        let rel = input["path"]
            .as_str()
            .ok_or_else(|| ErrorPayload::new("invalid_input", "path required"))?;
        let path = resolve_under_workspace_root(root, rel).map_err(workspace_path_to_payload)?;
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|e| ErrorPayload::new("io_error", e.to_string()))?;
        if metadata.file_type().is_symlink() {
            return Err(ErrorPayload::new(
                "permission_denied",
                "symlink paths are not readable via workspace.read",
            ));
        }
        let max_bytes = input
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_WORKSPACE_READ_MAX_BYTES)
            .min(DEFAULT_WORKSPACE_READ_MAX_BYTES);
        if metadata.len() > max_bytes {
            return Err(ErrorPayload::new(
                "file_too_large",
                format!(
                    "file size {} exceeds max_bytes {}",
                    metadata.len(),
                    max_bytes
                ),
            ));
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|e| ErrorPayload::new("io_error", e.to_string()))?;
        Ok(serde_json::json!({ "content": content }))
    }
}

fn workspace_path_to_payload(err: WorkspacePathError) -> ErrorPayload {
    ErrorPayload::new(err.code, err.message)
}

fn submit_turn_result_json(r: SubmitTurnResult) -> Value {
    match r {
        SubmitTurnResult::Completed { content } => {
            json!({ "status": "completed", "content": content })
        },
        SubmitTurnResult::Backgrounded {
            task_id,
            session_id,
        } => json!({
            "status": "backgrounded",
            "task_id": task_id,
            "session_id": session_id
        }),
    }
}

fn capability_wire_name(cap: ExtensionCapability) -> &'static str {
    astrcode_extension_sdk::s5r::capability_to_wire(cap)
}

fn required_capability_for_astrcode(cap: &str) -> Option<ExtensionCapability> {
    match cap {
        "astrcode.llm.small_chat" => Some(ExtensionCapability::SmallModel),
        "astrcode.session.read_events" => Some(ExtensionCapability::SessionHistory),
        c if c.starts_with("astrcode.session.control") => Some(ExtensionCapability::SessionControl),
        c if c.starts_with("astrcode.session.state") => Some(ExtensionCapability::SessionState),
        "astrcode.event.emit" => Some(ExtensionCapability::EmitEvents),
        "astrcode.workspace.read" => Some(ExtensionCapability::WorkspaceRead),
        _ => None,
    }
}

fn descriptors_for_capability(cap: ExtensionCapability) -> Vec<CapabilityDescriptor> {
    let object_schema = serde_json::json!({ "type": "object" });
    match cap {
        ExtensionCapability::SmallModel => vec![CapabilityDescriptor {
            name: "astrcode.llm.small_chat".into(),
            description: "Chat with the host-configured small LLM".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "messages": { "type": "array" } }
            }),
            output_schema: object_schema.clone(),
            supports_stream: true,
            cancelable: true,
        }],
        ExtensionCapability::SessionHistory => vec![CapabilityDescriptor {
            name: "astrcode.session.read_events".into(),
            description: "Read session event log".into(),
            input_schema: object_schema.clone(),
            output_schema: object_schema.clone(),
            supports_stream: false,
            cancelable: false,
        }],
        ExtensionCapability::SessionControl => vec![
            CapabilityDescriptor {
                name: "astrcode.session.control.create".into(),
                description: "Create a child session".into(),
                input_schema: object_schema.clone(),
                output_schema: object_schema.clone(),
                supports_stream: false,
                cancelable: false,
            },
            CapabilityDescriptor {
                name: "astrcode.session.control.submit_turn".into(),
                description: "Submit a turn to a session".into(),
                input_schema: object_schema.clone(),
                output_schema: object_schema.clone(),
                supports_stream: false,
                cancelable: false,
            },
            CapabilityDescriptor {
                name: "astrcode.session.control.dispose".into(),
                description: "Dispose a session".into(),
                input_schema: object_schema.clone(),
                output_schema: object_schema.clone(),
                supports_stream: false,
                cancelable: false,
            },
        ],
        ExtensionCapability::SessionState => vec![
            CapabilityDescriptor {
                name: "astrcode.session.state.read".into(),
                description: "Read extension namespaced state".into(),
                input_schema: object_schema.clone(),
                output_schema: object_schema.clone(),
                supports_stream: false,
                cancelable: false,
            },
            CapabilityDescriptor {
                name: "astrcode.session.state.write".into(),
                description: "Write extension namespaced state".into(),
                input_schema: object_schema.clone(),
                output_schema: object_schema.clone(),
                supports_stream: false,
                cancelable: false,
            },
        ],
        ExtensionCapability::EmitEvents => vec![CapabilityDescriptor {
            name: "astrcode.event.emit".into(),
            description: "Emit a declared extension event".into(),
            input_schema: object_schema.clone(),
            output_schema: object_schema.clone(),
            supports_stream: false,
            cancelable: false,
        }],
        ExtensionCapability::WorkspaceRead => vec![CapabilityDescriptor {
            name: "astrcode.workspace.read".into(),
            description: "Read a file under the session working directory".into(),
            input_schema: object_schema.clone(),
            output_schema: object_schema.clone(),
            supports_stream: false,
            cancelable: false,
        }],
        ExtensionCapability::ProcessSpawn | ExtensionCapability::NetworkClient => vec![],
    }
}

async fn run_small_llm(
    provider: &dyn LlmProvider,
    messages: Vec<LlmMessage>,
    collect_chunks: bool,
    cancel_token: Option<&CancellationToken>,
) -> Result<Value, ErrorPayload> {
    let mut rx = provider
        .generate(messages, vec![])
        .await
        .map_err(|e| ErrorPayload::new("llm_error", e.to_string()))?;

    let mut text = String::new();
    let mut chunks = Vec::new();
    loop {
        let event = if let Some(token) = cancel_token {
            tokio::select! {
                biased;
                () = token.cancelled() => {
                    return Err(ErrorPayload::new("cancelled", "invoke cancelled"));
                }
                ev = rx.recv() => ev,
            }
        } else {
            rx.recv().await
        };
        let Some(event) = event else {
            break;
        };
        match event {
            LlmEvent::ContentDelta { delta } => {
                if collect_chunks {
                    chunks.push(serde_json::json!({ "delta": delta }));
                }
                text.push_str(&delta);
            },
            LlmEvent::Done { .. } => break,
            LlmEvent::Error { message } => {
                return Err(ErrorPayload::new("llm_error", message));
            },
            _ => {},
        }
    }
    if collect_chunks {
        Ok(serde_json::json!({
            "content": text,
            "model": "small_llm",
            "chunks": chunks
        }))
    } else {
        Ok(serde_json::json!({ "content": text, "model": "small_llm" }))
    }
}

fn safe_filename(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 供 runner 内 `ExtensionEventSink` 与 IPC 路径复用。
pub fn emit_for_sink(
    extension_id: &str,
    declarations: &HashMap<String, ExtensionEventDecl>,
    event_tx: &mpsc::UnboundedSender<EventPayload>,
    event_type: &str,
    schema_version: u32,
    payload: Value,
) -> Result<(), ExtensionError> {
    validate_emit(declarations, event_type, schema_version, &payload)?;
    event_tx
        .send(EventPayload::ExtensionEvent {
            extension_id: extension_id.to_owned(),
            event_type: event_type.to_owned(),
            schema_version,
            payload,
        })
        .map_err(|_| ExtensionError::Internal("event channel closed".into()))
}

fn validate_emit(
    declarations: &HashMap<String, ExtensionEventDecl>,
    event_type: &str,
    schema_version: u32,
    payload: &Value,
) -> Result<(), ExtensionError> {
    let decl = declarations.get(event_type).ok_or_else(|| {
        ExtensionError::Internal(format!("undeclared extension event type: {event_type}"))
    })?;
    if schema_version > decl.schema_version {
        return Err(ExtensionError::Internal(format!(
            "schema_version {schema_version} exceeds declared {} for {event_type}",
            decl.schema_version
        )));
    }
    let serialized =
        serde_json::to_string(payload).map_err(|e| ExtensionError::Internal(e.to_string()))?;
    if serialized.len() > decl.max_payload_bytes {
        return Err(ExtensionError::Internal(format!(
            "payload exceeds {} bytes for {event_type}",
            decl.max_payload_bytes
        )));
    }
    Ok(())
}

pub fn decls_to_map(decls: &[ExtensionEventDecl]) -> HashMap<String, ExtensionEventDecl> {
    decls
        .iter()
        .map(|d| (d.event_type.clone(), d.clone()))
        .collect()
}

/// 从 [`ExtensionHostServices`] 构造共享 [`HostRouter`]。
pub fn build_host_router(
    host_services: Arc<ExtensionHostServices>,
    default_working_dir: Option<String>,
) -> Arc<HostRouter> {
    Arc::new(HostRouter::new(&host_services, default_working_dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_includes_session_control_subcaps() {
        let caps = HostRouter::catalog_for_grants(&[ExtensionCapability::SessionControl]);
        let names: Vec<_> = caps.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"astrcode.session.control.create"));
    }

    #[test]
    fn invoke_sync_rejects_precancelled_token() {
        let router = HostRouter::from_backends(HostBackends::default());
        let token = CancellationToken::new();
        token.cancel();
        let ctx = InvokeContext {
            cancel_token: Some(token),
            declared_capabilities: vec![ExtensionCapability::WorkspaceRead],
            working_dir: Some("/tmp".into()),
            ..Default::default()
        };
        let err = router
            .invoke_sync(
                "astrcode.workspace.read",
                &json!({ "path": "x" }).to_string(),
                &ctx,
            )
            .unwrap_err();
        assert_eq!(err.code, "cancelled");
    }

    #[test]
    fn invoke_session_submit_rejects_wait_for_result_on_peer_io_thread() {
        let router = HostRouter::from_backends(HostBackends::default());
        let ctx = InvokeContext {
            declared_capabilities: vec![ExtensionCapability::SessionControl],
            session_id: Some("parent".into()),
            on_peer_io_thread: true,
            ..Default::default()
        };
        let err = router
            .invoke_sync(
                "astrcode.session.control.submit_turn",
                &json!({
                    "target_session_id": "child",
                    "user_prompt": "hello",
                    "wait_for_result": true
                })
                .to_string(),
                &ctx,
            )
            .unwrap_err();
        assert_eq!(err.code, "invalid_request");
    }

    #[test]
    fn invoke_stream_sync_rejects_precancelled_token() {
        let router = HostRouter::from_backends(HostBackends::default());
        let token = CancellationToken::new();
        token.cancel();
        let ctx = InvokeContext {
            cancel_token: Some(token),
            declared_capabilities: vec![ExtensionCapability::SmallModel],
            ..Default::default()
        };
        let err = router
            .invoke_stream_sync(
                "astrcode.llm.small_chat",
                &json!({ "messages": [] }).to_string(),
                "req-1",
                &ctx,
            )
            .unwrap_err();
        assert_eq!(err.code, "cancelled");
    }
}
