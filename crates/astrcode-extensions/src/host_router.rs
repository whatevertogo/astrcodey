//! 宿主能力路由 — 唯一实现 `astrcode.*` RPC 与扩展事件发射。

mod capability_groups;
mod network;
mod process;
mod session_inspect;
mod workspace;

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use astrcode_core::{
    event::EventPayload,
    extension::{
        ChildToolPolicy, ExtensionCapability, ExtensionError, ExtensionEventDecl,
        ExtensionHostServices, ExtensionHttpRequest, ExtensionHttpResponse, NetworkRedirectPolicy,
        OutboundNetworkErrorKind, OutboundNetworkRequest, OutboundNetworkService,
    },
    llm::{LlmContent, LlmEvent, LlmMessage, LlmProvider, LlmRole},
    tool::{
        CreateSessionRequest, SessionAccessPair, SessionDeliveryOutcome, SessionOperations,
        SubmitTurnRequest, SubmitTurnResult,
    },
};
use astrcode_extension_sdk::{
    s5r::{CapabilityDescriptor, ErrorPayload, EventMsg, EventPhase, WireMessage},
    state,
    worker::HostNetworkRequest,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use self::capability_groups::{CapabilityGroupKind, HostCapabilityGroups};

const HOST_INVOKE_TIMEOUT: Duration = Duration::from_secs(30);

fn block_on_async<F>(future: F) -> Result<F::Output, ErrorPayload>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    static RUNTIME: std::sync::OnceLock<Result<tokio::runtime::Runtime, String>> =
        std::sync::OnceLock::new();
    let rt = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())
    });
    let rt = rt.as_ref().map_err(|message| {
        ErrorPayload::new(
            "host_runtime_unavailable",
            format!("failed to initialize host runtime: {message}"),
        )
    })?;

    // 从 tokio 异步任务里直接 block_on 会占满 test/runtime worker，嵌套 host invoke 会死锁。
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        if matches!(
            handle.runtime_flavor(),
            tokio::runtime::RuntimeFlavor::MultiThread
        ) {
            return tokio::task::block_in_place(|| run_on_host_runtime(rt, future));
        }
        match std::thread::spawn(move || run_on_host_runtime(rt, future)).join() {
            Ok(output) => return output,
            Err(_) => {
                return Err(ErrorPayload::new(
                    "host_runtime_failed",
                    "host runtime thread panicked",
                ));
            },
        }
    }
    run_on_host_runtime(rt, future)
}

fn run_on_host_runtime<F>(
    runtime: &tokio::runtime::Runtime,
    future: F,
) -> Result<F::Output, ErrorPayload>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    runtime.block_on(async move {
        tokio::spawn(future).await.map_err(|error| {
            ErrorPayload::new(
                "host_runtime_failed",
                format!("host runtime task failed: {error}"),
            )
        })
    })
}

fn run_blocking_io<T>(operation: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle)
            if matches!(
                handle.runtime_flavor(),
                tokio::runtime::RuntimeFlavor::MultiThread
            ) =>
        {
            tokio::task::block_in_place(operation)
        },
        _ => operation(),
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
    pub main_llm: Option<Arc<dyn LlmProvider>>,
    pub small_llm: Option<Arc<dyn LlmProvider>>,
    pub session_read: Option<Arc<dyn astrcode_core::storage::EventReader>>,
    pub default_working_dir: Option<String>,
    pub public_http_dispatcher: Option<Arc<dyn PublicHttpDispatcher>>,
    pub outbound_network: Option<Arc<dyn OutboundNetworkService>>,
}

#[async_trait::async_trait]
pub trait PublicHttpDispatcher: Send + Sync {
    async fn dispatch_public_http(
        &self,
        caller_extension_id: &str,
        request: ExtensionHttpRequest,
    ) -> Result<ExtensionHttpResponse, ExtensionError>;
}

/// 唯一 `astrcode.*` 能力实现。
pub struct HostRouter {
    groups: HostCapabilityGroups,
}

impl HostRouter {
    pub fn new(host_services: &ExtensionHostServices, default_working_dir: Option<String>) -> Self {
        Self {
            groups: HostBackends {
                main_llm: host_services.main_llm.clone(),
                small_llm: host_services.small_llm.clone(),
                session_read: host_services.session_read.clone(),
                default_working_dir,
                public_http_dispatcher: None,
                outbound_network: host_services.outbound_network.clone(),
            }
            .into(),
        }
    }

    pub fn from_backends(backends: HostBackends) -> Self {
        Self {
            groups: backends.into(),
        }
    }

    pub fn with_public_http_dispatcher(
        mut self,
        dispatcher: Arc<dyn PublicHttpDispatcher>,
    ) -> Self {
        self.groups.public_http.dispatcher = Some(dispatcher);
        self
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
        if cap.starts_with("astrcode.session.state") {
            return Ok(());
        }
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

        match CapabilityGroupKind::for_capability(cap, required_capability_for_astrcode(cap)) {
            Some(CapabilityGroupKind::Llm) => self.invoke_llm_capability(cap, &input, ctx),
            Some(CapabilityGroupKind::Session) => self.invoke_session_capability(cap, input, ctx),
            Some(CapabilityGroupKind::Context) => self.invoke_context_capability(cap, &input, ctx),
            Some(CapabilityGroupKind::Workspace) => {
                self.invoke_workspace_capability(cap, &input, ctx)
            },
            Some(CapabilityGroupKind::Process) => self.invoke_process_spawn(input, ctx),
            Some(CapabilityGroupKind::Network) => self.invoke_network_client(input, ctx),
            Some(CapabilityGroupKind::Extension) => self.invoke_public_http_dispatch(input, ctx),
            None => Err(ErrorPayload::new(
                "not_implemented",
                format!("capability not implemented: {cap}"),
            )),
        }
    }

    fn invoke_llm_capability(
        &self,
        cap: &str,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        match cap {
            "astrcode.llm.main_chat" => self.invoke_main_llm(input, false, ctx),
            "astrcode.llm.small_chat" => self.invoke_small_llm(input, false, ctx),
            _ => Err(capability_not_implemented(cap)),
        }
    }

    fn invoke_session_capability(
        &self,
        cap: &str,
        input: Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        match cap {
            "astrcode.session.read_events" => self.invoke_read_events(&input, ctx),
            "astrcode.session.control.create" => self.invoke_session_create(&input, ctx),
            "astrcode.session.control.submit_turn" => self.invoke_session_submit(&input, ctx),
            "astrcode.session.control.interrupt_and_submit" => {
                self.invoke_session_interrupt_and_submit(&input, ctx)
            },
            "astrcode.session.control.inject_input"
            | "astrcode.session.control.inject_or_start" => self.invoke_session_inject(&input, ctx),
            "astrcode.session.control.cancel_turn" => self.invoke_session_cancel(&input, ctx),
            "astrcode.session.control.execution_view" => {
                self.invoke_session_execution_view(&input, ctx)
            },
            "astrcode.session.control.dispose" => self.invoke_session_dispose(&input, ctx),
            "astrcode.session.inspect.list" => self.invoke_session_inspect_list(),
            "astrcode.session.inspect.snapshot" => self.invoke_session_inspect_snapshot(input),
            "astrcode.session.inspect.read_model" => self.invoke_session_inspect_read_model(input),
            "astrcode.session.inspect.provider_messages" => {
                self.invoke_session_inspect_provider_messages(input)
            },
            _ => Err(capability_not_implemented(cap)),
        }
    }

    fn invoke_context_capability(
        &self,
        cap: &str,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        match cap {
            "astrcode.session.state.read" => self.invoke_state_read(input, ctx),
            "astrcode.session.state.write" => self.invoke_state_write(input, ctx),
            "astrcode.event.emit" => self.invoke_emit(input, ctx),
            _ => Err(capability_not_implemented(cap)),
        }
    }

    fn invoke_workspace_capability(
        &self,
        cap: &str,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        match cap {
            "astrcode.workspace.read" => run_blocking_io(|| self.invoke_workspace_read(input, ctx)),
            "astrcode.workspace.list" => {
                run_blocking_io(|| workspace::list(self.workspace_root(ctx)?, input))
            },
            "astrcode.workspace.grep" => {
                run_blocking_io(|| workspace::grep(self.workspace_root(ctx)?, input))
            },
            "astrcode.workspace.glob" => {
                run_blocking_io(|| workspace::glob(self.workspace_root(ctx)?, input))
            },
            "astrcode.workspace.write" => {
                run_blocking_io(|| self.invoke_workspace_write(input, ctx))
            },
            "astrcode.workspace.edit" => run_blocking_io(|| self.invoke_workspace_edit(input, ctx)),
            _ => Err(capability_not_implemented(cap)),
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
            "astrcode.llm.main_chat" | "astrcode.llm.small_chat" => {
                let invoke = if cap == "astrcode.llm.main_chat" {
                    self.invoke_main_llm(&input, true, ctx)
                } else {
                    self.invoke_small_llm(&input, true, ctx)
                };
                let mut events = vec![WireMessage::Event(EventMsg {
                    id: request_id.clone(),
                    phase: EventPhase::Started,
                    data: Value::Null,
                    output: Value::Null,
                    error: None,
                })];
                match invoke {
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

    fn invoke_main_llm(
        &self,
        input: &Value,
        collect_chunks: bool,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let provider =
            self.groups.llm.main.as_ref().ok_or_else(|| {
                ErrorPayload::new("backend_unavailable", "main_llm not configured")
            })?;
        self.invoke_llm_chat(provider, "main_llm", input, collect_chunks, ctx)
    }

    fn invoke_small_llm(
        &self,
        input: &Value,
        collect_chunks: bool,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let provider =
            self.groups.llm.small.as_ref().ok_or_else(|| {
                ErrorPayload::new("backend_unavailable", "small_llm not configured")
            })?;
        self.invoke_llm_chat(provider, "small_llm", input, collect_chunks, ctx)
    }

    fn invoke_llm_chat(
        &self,
        provider: &Arc<dyn LlmProvider>,
        model_label: &'static str,
        input: &Value,
        collect_chunks: bool,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
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
        let label = model_label.to_string();
        block_on_async(async move {
            tokio::time::timeout(
                HOST_INVOKE_TIMEOUT,
                run_host_llm_chat(
                    &*provider,
                    &label,
                    messages,
                    collect_chunks,
                    cancel.as_ref(),
                ),
            )
            .await
            .map_err(|_| ErrorPayload::new("timeout", format!("{label}.chat timed out")))?
        })?
    }

    fn invoke_read_events(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let reader = self.groups.session.reader.as_ref().ok_or_else(|| {
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
        let access = SessionAccessPair::new(caller_session_id, target_session_id);
        let caller_owned = access.caller_session_id.clone();
        let target_owned = access.target_session_id.clone();
        if let Some(ops) = ctx.session_ops.as_ref() {
            let ops = Arc::clone(ops);
            block_on_async(async move {
                ops.query_session(access.as_access())
                    .await
                    .map_err(|e| ErrorPayload::new("permission_denied", e.to_string()))?;
                let sid = astrcode_core::types::SessionId::new(&access.target_session_id);
                reader
                    .replay_events(&sid)
                    .await
                    .map(|events| {
                        let truncated: Vec<_> = events.into_iter().take(limit).collect();
                        serde_json::json!({ "events": truncated })
                    })
                    .map_err(|e| ErrorPayload::new("read_failed", e.to_string()))
            })?
        } else if caller_owned != target_owned {
            Err(ErrorPayload::new(
                "permission_denied",
                "session_history read is limited to the caller session without session_control",
            ))
        } else {
            let sid = astrcode_core::types::SessionId::new(&target_owned);
            block_on_async(async move {
                reader
                    .replay_events(&sid)
                    .await
                    .map(|events| {
                        let truncated: Vec<_> = events.into_iter().take(limit).collect();
                        serde_json::json!({ "events": truncated })
                    })
                    .map_err(|e| ErrorPayload::new("read_failed", e.to_string()))
            })?
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
            tool_policy: parse_child_tool_policy(input)?,
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
        })?
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
        let caller = ctx
            .session_id
            .clone()
            .ok_or_else(|| ErrorPayload::new("invalid_input", "caller session_id required"))?;
        let target_session_id = input["target_session_id"]
            .as_str()
            .ok_or_else(|| ErrorPayload::new("invalid_input", "target_session_id required"))?
            .to_string();
        let user_prompt = input["user_prompt"]
            .as_str()
            .ok_or_else(|| ErrorPayload::new("invalid_input", "user_prompt required"))?
            .to_string();
        let req = SubmitTurnRequest::for_child(caller, target_session_id, user_prompt)
            .wait_for_result(wait_for_result)
            .notify_parent_on_complete(
                input["notify_parent_on_complete"]
                    .as_str()
                    .map(str::to_string),
            )
            .recycle_on_complete(input["recycle_on_complete"].as_bool().unwrap_or(false))
            .tool_call_id(input["tool_call_id"].as_str().map(str::to_string));
        let ops = Arc::clone(ops);
        block_on_async(async move {
            ops.submit_turn(req)
                .await
                .map(submit_turn_result_json)
                .map_err(|e| ErrorPayload::new("session_error", e.to_string()))
        })?
    }

    fn invoke_session_inject(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let ops = required_session_ops(ctx)?;
        let access = session_access_from_input(input, ctx)?;
        let content = input
            .get("content")
            .or_else(|| input.get("user_prompt"))
            .and_then(Value::as_str)
            .filter(|content| !content.is_empty())
            .ok_or_else(|| ErrorPayload::new("invalid_input", "content required"))?
            .to_string();
        block_on_async(async move {
            ops.inject_message(access.as_access(), content)
                .await
                .map(session_delivery_outcome_json)
                .map_err(|error| ErrorPayload::new("session_error", error.to_string()))
        })?
    }

    fn invoke_session_interrupt_and_submit(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let ops = required_session_ops(ctx)?;
        let access = session_access_from_input(input, ctx)?;
        let content = input
            .get("content")
            .or_else(|| input.get("user_prompt"))
            .and_then(Value::as_str)
            .filter(|content| !content.is_empty())
            .ok_or_else(|| ErrorPayload::new("invalid_input", "content required"))?
            .to_string();
        block_on_async(async move {
            ops.interrupt_and_submit(access.as_access(), content)
                .await
                .map(session_delivery_outcome_json)
                .map_err(|error| ErrorPayload::new("session_error", error.to_string()))
        })?
    }

    fn invoke_session_cancel(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let ops = required_session_ops(ctx)?;
        let access = session_access_from_input(input, ctx)?;
        block_on_async(async move {
            ops.cancel_turn(access.as_access())
                .await
                .map(|()| json!({ "ok": true }))
                .map_err(|error| ErrorPayload::new("session_error", error.to_string()))
        })?
    }

    fn invoke_session_execution_view(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let ops = required_session_ops(ctx)?;
        let access = session_access_from_input(input, ctx)?;
        block_on_async(async move {
            ops.execution_view(access.as_access())
                .await
                .map(|view| {
                    json!({
                        "phase": view.phase,
                        "active_turn_id": view.active_turn_id,
                        "queued_inputs": view.queued_inputs,
                    })
                })
                .map_err(|error| ErrorPayload::new("session_error", error.to_string()))
        })?
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
        let access = SessionAccessPair::new(
            ctx.session_id
                .clone()
                .ok_or_else(|| ErrorPayload::new("invalid_input", "caller session_id required"))?,
            session_id,
        );
        block_on_async(async move {
            ops.recycle_session(access.as_access())
                .await
                .map(|()| json!({ "ok": true }))
                .map_err(|e| ErrorPayload::new("session_error", e.to_string()))
        })?
    }

    fn invoke_session_inspect_list(&self) -> Result<Value, ErrorPayload> {
        let reader = self.session_reader()?;
        block_on_async(async move { session_inspect::list(reader).await })?
    }

    fn invoke_session_inspect_snapshot(&self, input: Value) -> Result<Value, ErrorPayload> {
        let reader = self.session_reader()?;
        block_on_async(async move { session_inspect::snapshot(reader, input).await })?
    }

    fn invoke_session_inspect_read_model(&self, input: Value) -> Result<Value, ErrorPayload> {
        let reader = self.session_reader()?;
        block_on_async(async move { session_inspect::read_model(reader, input).await })?
    }

    fn invoke_session_inspect_provider_messages(
        &self,
        input: Value,
    ) -> Result<Value, ErrorPayload> {
        let reader = self.session_reader()?;
        block_on_async(async move { session_inspect::provider_messages(reader, input).await })?
    }

    fn session_reader(&self) -> Result<Arc<dyn astrcode_core::storage::EventReader>, ErrorPayload> {
        self.groups
            .session
            .reader
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| ErrorPayload::new("backend_unavailable", "session_read not configured"))
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
        workspace::read(self.workspace_root(ctx)?, input)
    }

    fn invoke_workspace_write(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        workspace::write(self.workspace_root(ctx)?, input)
    }

    fn invoke_workspace_edit(
        &self,
        input: &Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        workspace::edit(self.workspace_root(ctx)?, input)
    }

    fn workspace_root<'a>(&'a self, ctx: &'a InvokeContext) -> Result<&'a str, ErrorPayload> {
        ctx.working_dir
            .as_deref()
            .or(self.groups.workspace.default_working_dir.as_deref())
            .ok_or_else(|| ErrorPayload::new("backend_unavailable", "working_dir not set"))
    }

    fn invoke_process_spawn(
        &self,
        input: Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let working_dir = ctx
            .working_dir
            .clone()
            .or_else(|| self.groups.process.default_working_dir.clone());
        let cancel_token = ctx.cancel_token.clone();
        let runner = Arc::clone(&self.groups.process.runner);
        block_on_async(async move {
            runner
                .spawn(input, working_dir.as_deref(), cancel_token.as_ref())
                .await
        })?
    }

    fn invoke_network_client(
        &self,
        input: Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let request = serde_json::from_value::<HostNetworkRequest>(input)
            .map_err(|error| ErrorPayload::new("invalid_input", error.to_string()))?;
        let service = self
            .groups
            .network
            .service
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| {
                ErrorPayload::new("backend_unavailable", "outbound network is not configured")
            })?;
        let cancel_token = ctx.cancel_token.clone();
        block_on_async(async move {
            let response = service
                .request(
                    OutboundNetworkRequest {
                        url: request.url,
                        method: request.method.unwrap_or_else(|| "GET".into()),
                        headers: request.headers,
                        body: request.body.unwrap_or_default().into_bytes(),
                        max_bytes: request.max_bytes.unwrap_or(1024 * 1024).min(1024 * 1024)
                            as usize,
                        timeout: Duration::from_millis(request.timeout_ms.unwrap_or(30_000))
                            .min(HOST_INVOKE_TIMEOUT),
                        redirect_policy: NetworkRedirectPolicy::Follow,
                    },
                    cancel_token,
                )
                .await
                .map_err(network_error_payload)?;
            let body = String::from_utf8(response.body).map_err(|error| {
                ErrorPayload::new(
                    "invalid_response_encoding",
                    format!("network response is not valid UTF-8: {error}"),
                )
            })?;
            Ok(json!({
                "status": response.status,
                "headers": response.headers,
                "body": body,
            }))
        })?
    }

    fn invoke_public_http_dispatch(
        &self,
        input: Value,
        ctx: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let dispatcher = self.groups.public_http.dispatcher.as_ref().ok_or_else(|| {
            ErrorPayload::new(
                "backend_unavailable",
                "public HTTP dispatcher is not configured",
            )
        })?;
        let request = serde_json::from_value::<ExtensionHttpRequest>(input)
            .map_err(|error| ErrorPayload::new("invalid_input", error.to_string()))?;
        let dispatcher = Arc::clone(dispatcher);
        let caller_extension_id = ctx.extension_id.clone();
        block_on_async(async move {
            tokio::time::timeout(
                HOST_INVOKE_TIMEOUT,
                dispatcher.dispatch_public_http(&caller_extension_id, request),
            )
            .await
            .map_err(|_| ErrorPayload::new("timeout", "public HTTP dispatch timed out"))?
            .and_then(|response| {
                serde_json::to_value(response)
                    .map_err(|error| ExtensionError::Internal(error.to_string()))
            })
            .map_err(|error| ErrorPayload::new("dispatch_failed", error.to_string()))
        })?
    }
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

fn session_delivery_outcome_json(outcome: SessionDeliveryOutcome) -> Value {
    match outcome {
        SessionDeliveryOutcome::Started { turn_id } => {
            json!({ "status": "started", "turn_id": turn_id })
        },
        SessionDeliveryOutcome::Injected { turn_id } => {
            json!({ "status": "injected", "turn_id": turn_id })
        },
        SessionDeliveryOutcome::Queued { queue_len } => {
            json!({ "status": "queued", "queue_len": queue_len })
        },
    }
}

fn required_session_ops(ctx: &InvokeContext) -> Result<Arc<dyn SessionOperations>, ErrorPayload> {
    ctx.session_ops
        .as_ref()
        .map(Arc::clone)
        .ok_or_else(|| ErrorPayload::new("backend_unavailable", "session_ops not available"))
}

fn session_access_from_input(
    input: &Value,
    ctx: &InvokeContext,
) -> Result<SessionAccessPair, ErrorPayload> {
    let caller = ctx
        .session_id
        .as_deref()
        .ok_or_else(|| ErrorPayload::new("invalid_input", "caller session_id required"))?;
    let target = input
        .get("target_session_id")
        .or_else(|| input.get("session_id"))
        .and_then(Value::as_str)
        .filter(|target| !target.is_empty())
        .ok_or_else(|| ErrorPayload::new("invalid_input", "target_session_id required"))?;
    Ok(SessionAccessPair::new(caller, target))
}

fn capability_wire_name(cap: ExtensionCapability) -> &'static str {
    astrcode_extension_sdk::s5r::capability_to_wire(cap)
}

fn capability_not_implemented(capability: &str) -> ErrorPayload {
    ErrorPayload::new(
        "not_implemented",
        format!("capability not implemented: {capability}"),
    )
}

fn network_error_payload(error: astrcode_core::extension::OutboundNetworkError) -> ErrorPayload {
    let code = match error.kind {
        OutboundNetworkErrorKind::InvalidRequest => "invalid_input",
        OutboundNetworkErrorKind::PermissionDenied => "permission_denied",
        OutboundNetworkErrorKind::Unavailable => "backend_unavailable",
        OutboundNetworkErrorKind::RequestFailed => "network_error",
        OutboundNetworkErrorKind::Timeout => "timeout",
        OutboundNetworkErrorKind::ResponseTooLarge => "response_too_large",
        OutboundNetworkErrorKind::Cancelled => "cancelled",
    };
    ErrorPayload::new(code, error.message)
}

fn required_capability_for_astrcode(cap: &str) -> Option<ExtensionCapability> {
    match cap {
        "astrcode.llm.main_chat" => Some(ExtensionCapability::MainModel),
        "astrcode.llm.small_chat" => Some(ExtensionCapability::SmallModel),
        "astrcode.session.read_events" => Some(ExtensionCapability::SessionHistory),
        c if c.starts_with("astrcode.session.control") => Some(ExtensionCapability::SessionControl),
        c if c.starts_with("astrcode.session.inspect") => Some(ExtensionCapability::SessionInspect),
        "astrcode.event.emit" => Some(ExtensionCapability::EmitEvents),
        "astrcode.workspace.read"
        | "astrcode.workspace.list"
        | "astrcode.workspace.grep"
        | "astrcode.workspace.glob" => Some(ExtensionCapability::WorkspaceRead),
        "astrcode.workspace.write" | "astrcode.workspace.edit" => {
            Some(ExtensionCapability::WorkspaceWrite)
        },
        "astrcode.process.spawn" => Some(ExtensionCapability::ProcessSpawn),
        "astrcode.network.client" => Some(ExtensionCapability::NetworkClient),
        "astrcode.extension.http.public" => Some(ExtensionCapability::PublicHttpDispatch),
        _ => None,
    }
}

fn parse_child_tool_policy(input: &Value) -> Result<Option<ChildToolPolicy>, ErrorPayload> {
    let Some(value) = input.get("tool_policy") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }

    let policy = serde_json::from_value::<ChildToolPolicy>(value.clone()).map_err(|e| {
        ErrorPayload::new("invalid_input", format!("invalid tool_policy: {e}"))
            .with_hint("expected {\"mode\":\"allow|deny\",\"tools\":[\"tool_name\"]}")
    })?;
    validate_child_tool_policy(&policy)?;
    Ok(Some(policy))
}

fn validate_child_tool_policy(policy: &ChildToolPolicy) -> Result<(), ErrorPayload> {
    let tools = match policy {
        ChildToolPolicy::Deny { tools } => tools,
        ChildToolPolicy::Allow { tools } if tools.is_empty() => {
            return Err(ErrorPayload::new(
                "invalid_input",
                "tool_policy allow mode requires at least one tool",
            ));
        },
        ChildToolPolicy::Allow { tools } => tools,
    };

    if tools.iter().any(|tool| tool.trim().is_empty()) {
        return Err(ErrorPayload::new(
            "invalid_input",
            "tool_policy tools must be non-empty strings",
        ));
    }

    Ok(())
}

fn session_inspect_descriptor(
    name: &str,
    description: &str,
    requires_session_id: bool,
) -> CapabilityDescriptor {
    let input_schema = if requires_session_id {
        json!({
            "type": "object",
            "properties": { "session_id": { "type": "string" } },
            "required": ["session_id"]
        })
    } else {
        json!({ "type": "object" })
    };
    CapabilityDescriptor {
        name: name.into(),
        description: description.into(),
        input_schema,
        output_schema: json!({ "type": "object" }),
        supports_stream: false,
        cancelable: false,
    }
}

fn descriptors_for_capability(cap: ExtensionCapability) -> Vec<CapabilityDescriptor> {
    let object_schema = serde_json::json!({ "type": "object" });
    match cap {
        ExtensionCapability::MainModel => vec![CapabilityDescriptor {
            name: "astrcode.llm.main_chat".into(),
            description: "Chat with the host-configured main LLM (session active model)".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "messages": { "type": "array" } }
            }),
            output_schema: object_schema,
            supports_stream: true,
            cancelable: true,
        }],
        ExtensionCapability::SmallModel => vec![CapabilityDescriptor {
            name: "astrcode.llm.small_chat".into(),
            description: "Chat with the host-configured small LLM".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "messages": { "type": "array" } }
            }),
            output_schema: object_schema,
            supports_stream: true,
            cancelable: true,
        }],
        ExtensionCapability::SessionHistory => vec![CapabilityDescriptor {
            name: "astrcode.session.read_events".into(),
            description: "Read session event log".into(),
            input_schema: object_schema.clone(),
            output_schema: object_schema,
            supports_stream: false,
            cancelable: false,
        }],
        ExtensionCapability::SessionInspect => vec![
            session_inspect_descriptor(
                "astrcode.session.inspect.list",
                "List all sessions visible to the host (global privileged access)",
                false,
            ),
            session_inspect_descriptor(
                "astrcode.session.inspect.snapshot",
                "Read any host-visible session snapshot (global privileged access)",
                true,
            ),
            session_inspect_descriptor(
                "astrcode.session.inspect.read_model",
                "Read any host-visible projected session model through a stable wire DTO",
                true,
            ),
            session_inspect_descriptor(
                "astrcode.session.inspect.provider_messages",
                "Read provider-visible messages for any host-visible session",
                true,
            ),
        ],
        ExtensionCapability::SessionControl => vec![
            CapabilityDescriptor {
                name: "astrcode.session.control.create".into(),
                description: "Create a child session".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "working_dir": { "type": "string" },
                        "system_prompt": { "type": "string" },
                        "model_preference": { "type": "string" },
                        "ephemeral": { "type": "boolean" },
                        "tool_call_id": { "type": "string" },
                        "tool_policy": {
                            "type": "object",
                            "description": "Child session tool visibility policy.",
                            "oneOf": [
                                {
                                    "properties": {
                                        "mode": { "const": "deny" },
                                        "tools": {
                                            "type": "array",
                                            "items": { "type": "string" }
                                        }
                                    },
                                    "required": ["mode", "tools"]
                                },
                                {
                                    "properties": {
                                        "mode": { "const": "allow" },
                                        "tools": {
                                            "type": "array",
                                            "items": { "type": "string" },
                                            "minItems": 1
                                        }
                                    },
                                    "required": ["mode", "tools"]
                                }
                            ]
                        }
                    }
                }),
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
                name: "astrcode.session.control.interrupt_and_submit".into(),
                description: "Interrupt the active turn and submit new input".into(),
                input_schema: object_schema.clone(),
                output_schema: object_schema.clone(),
                supports_stream: false,
                cancelable: true,
            },
            CapabilityDescriptor {
                name: "astrcode.session.control.inject_input".into(),
                description: "Inject input into a running turn or start when idle".into(),
                input_schema: object_schema.clone(),
                output_schema: object_schema.clone(),
                supports_stream: false,
                cancelable: false,
            },
            CapabilityDescriptor {
                name: "astrcode.session.control.inject_or_start".into(),
                description: "Inject input into a running turn or start when idle".into(),
                input_schema: object_schema.clone(),
                output_schema: object_schema.clone(),
                supports_stream: false,
                cancelable: false,
            },
            CapabilityDescriptor {
                name: "astrcode.session.control.cancel_turn".into(),
                description: "Cancel the active turn".into(),
                input_schema: object_schema.clone(),
                output_schema: object_schema.clone(),
                supports_stream: false,
                cancelable: true,
            },
            CapabilityDescriptor {
                name: "astrcode.session.control.execution_view".into(),
                description: "Read active turn and queued-input state".into(),
                input_schema: object_schema.clone(),
                output_schema: object_schema.clone(),
                supports_stream: false,
                cancelable: false,
            },
            CapabilityDescriptor {
                name: "astrcode.session.control.dispose".into(),
                description: "Dispose a session".into(),
                input_schema: object_schema.clone(),
                output_schema: object_schema,
                supports_stream: false,
                cancelable: false,
            },
        ],
        ExtensionCapability::EmitEvents => vec![CapabilityDescriptor {
            name: "astrcode.event.emit".into(),
            description: "Emit a declared extension event".into(),
            input_schema: object_schema.clone(),
            output_schema: object_schema,
            supports_stream: false,
            cancelable: false,
        }],
        ExtensionCapability::WorkspaceRead => [
            (
                "astrcode.workspace.read",
                "Read a bounded UTF-8 workspace file",
            ),
            (
                "astrcode.workspace.list",
                "List a bounded workspace directory tree",
            ),
            (
                "astrcode.workspace.grep",
                "Regex-search bounded UTF-8 workspace files",
            ),
            (
                "astrcode.workspace.glob",
                "Match bounded workspace paths by glob",
            ),
        ]
        .into_iter()
        .map(|(name, description)| CapabilityDescriptor {
            name: name.into(),
            description: description.into(),
            input_schema: object_schema.clone(),
            output_schema: object_schema.clone(),
            supports_stream: false,
            cancelable: false,
        })
        .collect(),
        ExtensionCapability::WorkspaceWrite => vec![
            CapabilityDescriptor {
                name: "astrcode.workspace.write".into(),
                description: "Create or replace a non-sensitive file under the working directory"
                    .into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }),
                output_schema: object_schema.clone(),
                supports_stream: false,
                cancelable: false,
            },
            CapabilityDescriptor {
                name: "astrcode.workspace.edit".into(),
                description: "Replace an exact text fragment in a non-sensitive workspace file"
                    .into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "old_text": { "type": "string" },
                        "new_text": { "type": "string" },
                        "replace_all": { "type": "boolean" }
                    },
                    "required": ["path", "old_text", "new_text"]
                }),
                output_schema: object_schema,
                supports_stream: false,
                cancelable: false,
            },
        ],
        ExtensionCapability::ProcessSpawn => vec![CapabilityDescriptor {
            name: "astrcode.process.spawn".into(),
            description: "Run a bounded subprocess with an optional workspace-relative cwd".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "args": { "type": "array", "items": { "type": "string" } },
                    "cwd": { "type": "string" },
                    "stdin": { "type": "string" },
                    "timeout_ms": { "type": "integer", "minimum": 1 }
                },
                "required": ["command"]
            }),
            output_schema: object_schema,
            supports_stream: false,
            cancelable: true,
        }],
        ExtensionCapability::NetworkClient => vec![CapabilityDescriptor {
            name: "astrcode.network.client".into(),
            description: "Send a bounded outbound HTTP or HTTPS request".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "method": { "type": "string" },
                    "url": { "type": "string" },
                    "headers": { "type": "object", "additionalProperties": { "type": "string" } },
                    "body": { "type": "string" },
                    "max_bytes": { "type": "integer", "minimum": 0 },
                    "timeout_ms": { "type": "integer", "minimum": 1 }
                },
                "required": ["url"]
            }),
            output_schema: object_schema,
            supports_stream: false,
            cancelable: true,
        }],
        ExtensionCapability::PublicHttpDispatch => vec![CapabilityDescriptor {
            name: "astrcode.extension.http.public".into(),
            description: "Dispatch a request to another extension's public HTTP route".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "method": { "type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"] },
                    "path": { "type": "string" },
                    "query": { "type": "string" },
                    "body": {}
                },
                "required": ["method", "path"]
            }),
            output_schema: object_schema,
            supports_stream: false,
            cancelable: true,
        }],
        ExtensionCapability::PublicHttp
        | ExtensionCapability::ConsumeEvents
        | ExtensionCapability::ProviderRequest
        | ExtensionCapability::InputDelivery
        | ExtensionCapability::ToolIntercept
        | ExtensionCapability::TurnContinuationControl
        | ExtensionCapability::LiveConversation => Vec::new(),
    }
}

async fn run_host_llm_chat(
    provider: &dyn LlmProvider,
    model_label: &str,
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
            "model": model_label,
            "chunks": chunks
        }))
    } else {
        Ok(serde_json::json!({ "content": text, "model": model_label }))
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

/// 构造 trusted bundled extensions 与 worker 共用的受限出站网络服务。
pub fn default_outbound_network_service() -> Arc<dyn OutboundNetworkService> {
    Arc::new(network::RestrictedNetworkService::default())
}

pub fn build_host_router_with_public_http_dispatcher(
    host_services: Arc<ExtensionHostServices>,
    default_working_dir: Option<String>,
    dispatcher: Arc<dyn PublicHttpDispatcher>,
) -> Arc<HostRouter> {
    Arc::new(
        HostRouter::new(&host_services, default_working_dir)
            .with_public_http_dispatcher(dispatcher),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use astrcode_core::{
        permission::ApprovalDecision,
        storage::{EventReader, EventStore},
        tool::{
            CreateRootSessionRequest, SessionAccess, SessionApiError, SessionHandle, SessionStatus,
        },
    };
    use astrcode_storage::in_memory::InMemoryEventStore;

    use super::*;

    #[test]
    fn host_runtime_contains_extension_task_panics() {
        let result = block_on_async(async {
            panic!("extension task panic");
            #[allow(unreachable_code)]
            42
        });

        let error = result.expect_err("task panic should become a host error");
        assert_eq!(error.code, "host_runtime_failed");
    }

    #[test]
    fn catalog_includes_session_control_subcaps() {
        let caps = HostRouter::catalog_for_grants(&[ExtensionCapability::SessionControl]);
        let names: Vec<_> = caps.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"astrcode.session.control.create"));
    }

    #[test]
    fn catalog_includes_session_inspect_surface() {
        let caps = HostRouter::catalog_for_grants(&[ExtensionCapability::SessionInspect]);
        let names = caps
            .iter()
            .map(|descriptor| descriptor.name.as_str())
            .collect::<Vec<_>>();

        assert!(names.contains(&"astrcode.session.inspect.list"));
        assert!(names.contains(&"astrcode.session.inspect.snapshot"));
        assert!(names.contains(&"astrcode.session.inspect.read_model"));
        assert!(names.contains(&"astrcode.session.inspect.provider_messages"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_inspect_maps_storage_model_to_wire_contract() {
        let store = Arc::new(InMemoryEventStore::new());
        let session_id = astrcode_core::types::SessionId::new("inspect-session");
        store
            .create_session(&session_id, "/workspace", "test-model", None, None, None)
            .await
            .expect("create session");
        let reader: Arc<dyn EventReader> = store;
        let router = HostRouter::from_backends(HostBackends {
            session_read: Some(reader),
            ..Default::default()
        });
        let ctx = InvokeContext {
            declared_capabilities: vec![ExtensionCapability::SessionInspect],
            ..Default::default()
        };

        let list = router
            .invoke_sync("astrcode.session.inspect.list", "{}", &ctx)
            .expect("list sessions");
        assert_eq!(list["sessions"][0]["sessionId"], "inspect-session");

        let model = router
            .invoke_sync(
                "astrcode.session.inspect.read_model",
                &json!({ "session_id": "inspect-session" }).to_string(),
                &ctx,
            )
            .expect("read session model");
        assert_eq!(model["readModel"]["modelId"], "test-model");
        assert_eq!(model["readModel"]["phase"], "idle");
    }

    #[test]
    fn session_control_create_schema_includes_tool_policy() {
        let caps = HostRouter::catalog_for_grants(&[ExtensionCapability::SessionControl]);
        let create = caps
            .iter()
            .find(|cap| cap.name == "astrcode.session.control.create")
            .expect("create capability");

        assert!(create.input_schema["properties"]["tool_policy"].is_object());
    }

    #[test]
    fn catalog_includes_bounded_io_capabilities() {
        let caps = HostRouter::catalog_for_grants(&[
            ExtensionCapability::WorkspaceRead,
            ExtensionCapability::WorkspaceWrite,
            ExtensionCapability::NetworkClient,
            ExtensionCapability::ProcessSpawn,
        ]);
        let names: Vec<_> = caps.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"astrcode.workspace.read"));
        assert!(names.contains(&"astrcode.workspace.list"));
        assert!(names.contains(&"astrcode.workspace.grep"));
        assert!(names.contains(&"astrcode.workspace.glob"));
        assert!(names.contains(&"astrcode.workspace.write"));
        assert!(names.contains(&"astrcode.workspace.edit"));
        assert!(names.contains(&"astrcode.network.client"));
        assert!(names.contains(&"astrcode.process.spawn"));
    }

    #[test]
    fn network_capability_rejects_non_http_urls_when_declared() {
        let router = HostRouter::from_backends(HostBackends {
            outbound_network: Some(default_outbound_network_service()),
            ..Default::default()
        });
        let ctx = InvokeContext {
            declared_capabilities: vec![ExtensionCapability::NetworkClient],
            ..Default::default()
        };
        let err = router
            .invoke_sync(
                "astrcode.network.client",
                &json!({ "url": "file:///etc/passwd" }).to_string(),
                &ctx,
            )
            .unwrap_err();
        assert_eq!(err.code, "permission_denied");
    }

    #[test]
    fn session_state_api_does_not_require_declared_capability() {
        let router = HostRouter::from_backends(HostBackends::default());
        let temp = tempfile::tempdir().expect("tempdir");
        let ctx = InvokeContext {
            extension_id: "stateful-test".into(),
            session_store_dir: Some(temp.path().to_path_buf()),
            declared_capabilities: Vec::new(),
            ..Default::default()
        };

        router
            .invoke_sync(
                "astrcode.session.state.write",
                &json!({ "key": "goal", "content": "active" }).to_string(),
                &ctx,
            )
            .expect("write state without capability");
        let read = router
            .invoke_sync(
                "astrcode.session.state.read",
                &json!({ "key": "goal" }).to_string(),
                &ctx,
            )
            .expect("read state without capability");

        assert_eq!(read["content"], "active");
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
    fn invoke_session_create_forwards_tool_policy() {
        let router = HostRouter::from_backends(HostBackends::default());
        let ops = Arc::new(CapturingSessionOps::default());
        let ctx = InvokeContext {
            extension_id: "test-extension".into(),
            session_id: Some("parent".into()),
            session_ops: Some(ops.clone()),
            declared_capabilities: vec![ExtensionCapability::SessionControl],
            ..Default::default()
        };

        let output = router
            .invoke_sync(
                "astrcode.session.control.create",
                &json!({
                    "name": "worker",
                    "tool_policy": {
                        "mode": "deny",
                        "tools": ["agent"]
                    }
                })
                .to_string(),
                &ctx,
            )
            .expect("create child session");

        assert_eq!(output["session_id"], "child-1");
        let requests = ops.creates.lock().expect("creates lock");
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].tool_policy,
            Some(ChildToolPolicy::Deny {
                tools: vec!["agent".into()]
            })
        );
    }

    #[test]
    fn invoke_session_create_rejects_invalid_tool_policy() {
        let router = HostRouter::from_backends(HostBackends::default());
        let ctx = InvokeContext {
            session_id: Some("parent".into()),
            session_ops: Some(Arc::new(CapturingSessionOps::default())),
            declared_capabilities: vec![ExtensionCapability::SessionControl],
            ..Default::default()
        };

        let err = router
            .invoke_sync(
                "astrcode.session.control.create",
                &json!({
                    "tool_policy": {
                        "mode": "allow",
                        "tools": []
                    }
                })
                .to_string(),
                &ctx,
            )
            .unwrap_err();

        assert_eq!(err.code, "invalid_input");
    }

    #[test]
    fn invoke_session_inject_returns_delivery_outcome() {
        let router = HostRouter::from_backends(HostBackends::default());
        let ctx = InvokeContext {
            session_id: Some("parent".into()),
            session_ops: Some(Arc::new(CapturingSessionOps::default())),
            declared_capabilities: vec![ExtensionCapability::SessionControl],
            ..Default::default()
        };

        let output = router
            .invoke_sync(
                "astrcode.session.control.inject_or_start",
                &json!({
                    "target_session_id": "child",
                    "content": "continue"
                })
                .to_string(),
                &ctx,
            )
            .expect("inject session input");

        assert_eq!(output["status"], "injected");
        assert_eq!(output["turn_id"], "turn-injected");
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

    #[derive(Default)]
    struct CapturingSessionOps {
        creates: Mutex<Vec<CreateSessionRequest>>,
    }

    #[async_trait::async_trait]
    impl SessionOperations for CapturingSessionOps {
        async fn create_root_session(
            &self,
            _request: CreateRootSessionRequest,
        ) -> Result<SessionHandle, SessionApiError> {
            Ok(SessionHandle {
                session_id: "root".into(),
            })
        }

        async fn create_session(
            &self,
            _parent_session_id: &str,
            request: CreateSessionRequest,
        ) -> Result<SessionHandle, SessionApiError> {
            let mut creates = self.creates.lock().expect("creates lock");
            creates.push(request);
            Ok(SessionHandle {
                session_id: format!("child-{}", creates.len()),
            })
        }

        async fn inject_message(
            &self,
            _access: SessionAccess<'_>,
            _content: String,
        ) -> Result<SessionDeliveryOutcome, SessionApiError> {
            Ok(SessionDeliveryOutcome::Injected {
                turn_id: "turn-injected".into(),
            })
        }

        async fn submit_turn(
            &self,
            _request: SubmitTurnRequest,
        ) -> Result<SubmitTurnResult, SessionApiError> {
            Ok(SubmitTurnResult::Backgrounded {
                task_id: "task".into(),
                session_id: "child".into(),
            })
        }

        async fn query_session(
            &self,
            _access: SessionAccess<'_>,
        ) -> Result<SessionStatus, SessionApiError> {
            Ok(SessionStatus {
                alive: true,
                has_active_turn: false,
                last_finish_reason: None,
                message_count: 0,
            })
        }

        async fn recycle_session(&self, _access: SessionAccess<'_>) -> Result<(), SessionApiError> {
            Ok(())
        }

        async fn delete_session(&self, _access: SessionAccess<'_>) -> Result<(), SessionApiError> {
            Ok(())
        }

        async fn restore_session(&self, _access: SessionAccess<'_>) -> Result<(), SessionApiError> {
            Ok(())
        }

        async fn resolve_tool_approval(
            &self,
            _target_session_id: &str,
            _call_id: &str,
            _decision: ApprovalDecision,
        ) -> Result<(), SessionApiError> {
            Ok(())
        }

        async fn resolve_tool_ui_response(
            &self,
            _target_session_id: &str,
            _call_id: &str,
            _answers: std::collections::BTreeMap<String, String>,
        ) -> Result<(), SessionApiError> {
            Ok(())
        }
    }
}
