//! 单条宿主 ↔ WASM 扩展的对等协议会话。

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use astrcode_core::extension::{ExtensionCapability, ExtensionError, ExtensionEventDecl};
use astrcode_extension_sdk::s5r::{
    self, CAP_HANDLER_INVOKE, EventPhase, HandlerResult, InitializeMsg, InitializeOutput, PeerInfo,
    ResultKind, ResultMsg, S5R_STACK, S5R_VERSION, WireMessage, capability_from_wire,
    is_reserved_capability_prefix,
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    host_router::{HostRouter, InvokeContext, decls_to_map},
    wasm_peer_transport::TransportError,
};

const MAX_REENTRANCY: u32 = 8;
const MAX_CONTINUATION_DEPTH: u32 = 16;

static REQUEST_COUNTER: AtomicU32 = AtomicU32::new(0);

fn new_request_id() -> String {
    format!("req-{}", REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// 握手完成后从 guest Initialize 解析出的注册信息。
#[derive(Debug, Clone)]
pub struct PeerRegistration {
    pub extension_id: String,
    pub version: String,
    pub capabilities: Vec<ExtensionCapability>,
    pub tools: Vec<ManifestTool>,
    pub commands: Vec<ManifestCommand>,
    pub hooks: Vec<ManifestHook>,
    pub extension_events: Vec<ExtensionEventDecl>,
}

/// s5r manifest 形状（guest Initialize 的 handlers/tools 等）。
pub mod manifest_types {
    use serde::{Deserialize, Serialize};
    use serde_json::Value;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ManifestTool {
        pub name: String,
        pub description: String,
        pub parameters: Value,
        #[serde(default = "sequential_mode")]
        pub mode: String,
    }

    fn sequential_mode() -> String {
        "sequential".into()
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ManifestCommand {
        pub name: String,
        #[serde(default)]
        pub description: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ManifestHook {
        pub on: String,
        pub mode: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ManifestExtensionEvent {
        pub event_type: String,
        #[serde(default = "default_schema_version")]
        pub schema_version: u32,
        #[serde(default = "default_durable")]
        pub durable: bool,
        #[serde(default = "default_max_payload")]
        pub max_payload_bytes: usize,
    }

    fn default_schema_version() -> u32 {
        1
    }
    fn default_durable() -> bool {
        true
    }
    fn default_max_payload() -> usize {
        64 * 1024
    }
}

use manifest_types::{ManifestCommand, ManifestExtensionEvent, ManifestHook, ManifestTool};

/// 每条 WASM 链路的 peer 状态机。
pub struct ExtensionPeer {
    job_tx: std::sync::mpsc::Sender<crate::wasm_peer_transport::TransportJob>,
    router: Arc<HostRouter>,
    remote_initialized: std::sync::atomic::AtomicBool,
    registration: std::sync::RwLock<Option<PeerRegistration>>,
    reentrancy: std::sync::atomic::AtomicU32,
    pending_cancels: Mutex<HashMap<String, CancellationToken>>,
}

impl ExtensionPeer {
    pub fn new(
        job_tx: std::sync::mpsc::Sender<crate::wasm_peer_transport::TransportJob>,
        router: Arc<HostRouter>,
    ) -> Self {
        Self {
            job_tx,
            router,
            remote_initialized: std::sync::atomic::AtomicBool::new(false),
            registration: std::sync::RwLock::new(None),
            reentrancy: std::sync::atomic::AtomicU32::new(0),
            pending_cancels: Mutex::new(HashMap::new()),
        }
    }

    pub fn extension_id(&self) -> String {
        self.registration
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|r| r.extension_id.clone()))
            .unwrap_or_default()
    }

    pub fn declared_capabilities(&self) -> Vec<ExtensionCapability> {
        self.registration
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|r| r.capabilities.clone()))
            .unwrap_or_default()
    }

    pub fn event_decls(&self) -> HashMap<String, ExtensionEventDecl> {
        self.registration
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|r| decls_to_map(&r.extension_events)))
            .unwrap_or_default()
    }

    pub fn router(&self) -> &Arc<HostRouter> {
        &self.router
    }

    pub fn registration(&self) -> Option<PeerRegistration> {
        self.registration.read().ok().and_then(|g| g.clone())
    }

    /// 处理 guest 发来的 `peer_exchange` 入站消息（在 guest 线程上同步调用）。
    pub fn handle_inbound_sync(
        &self,
        msg: WireMessage,
        invoke_ctx: &InvokeContext,
    ) -> Result<WireMessage, TransportError> {
        match msg {
            WireMessage::Initialize(init) => self.handle_initialize_sync(init),
            WireMessage::Invoke(inv) => self.handle_guest_invoke_sync(inv, invoke_ctx),
            WireMessage::Cancel(cancel) => {
                let cancelled = if let Some(token) = self.pending_cancels.lock().remove(&cancel.id)
                {
                    token.cancel();
                    true
                } else {
                    false
                };
                Ok(WireMessage::Result(ResultMsg {
                    id: cancel.id,
                    kind: ResultKind::InvokeResult,
                    success: cancelled,
                    output: cancelled.then(|| json!({ "cancelled": true })),
                    error: if cancelled {
                        None
                    } else {
                        Some(s5r::ErrorPayload::new(
                            "not_found",
                            "no in-flight invoke with this id",
                        ))
                    },
                }))
            },
            other => Err(TransportError(format!(
                "unexpected inbound message during guest call: {:?}",
                std::mem::discriminant(&other)
            ))),
        }
    }

    fn handle_initialize_sync(&self, init: InitializeMsg) -> Result<WireMessage, TransportError> {
        if self
            .remote_initialized
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(TransportError("already initialized".into()));
        }
        let reg = parse_initialize(&init)?;
        if let Ok(mut g) = self.registration.write() {
            *g = Some(reg.clone());
        }
        let granted = HostRouter::catalog_for_grants(&reg.capabilities);
        let output = InitializeOutput {
            peer: PeerInfo {
                name: "astrcode".into(),
                role: "core".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            protocol_version: S5R_VERSION.into(),
            capabilities: granted,
        };
        Ok(WireMessage::Result(ResultMsg {
            id: init.id,
            kind: ResultKind::InitializeResult,
            success: true,
            output: Some(serde_json::to_value(output).map_err(|e| TransportError(e.to_string()))?),
            error: None,
        }))
    }

    fn handle_guest_invoke_sync(
        &self,
        inv: astrcode_extension_sdk::s5r::InvokeMsg,
        base_ctx: &InvokeContext,
    ) -> Result<WireMessage, TransportError> {
        let depth = self
            .reentrancy
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if depth >= MAX_REENTRANCY {
            self.reentrancy
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            return Err(TransportError("reentrancy depth exceeded".into()));
        }

        let cancel_token = CancellationToken::new();
        self.pending_cancels
            .lock()
            .insert(inv.id.clone(), cancel_token.clone());

        let mut ctx = base_ctx.clone();
        ctx.extension_id = self.extension_id();
        ctx.declared_capabilities = self.declared_capabilities();
        ctx.event_declarations = self.event_decls();
        ctx.cancel_token = Some(cancel_token);

        let result = if inv.stream {
            match self.router.invoke_stream_sync(
                &inv.capability,
                &inv.input.to_string(),
                &inv.id,
                &ctx,
            ) {
                Ok(events) => match terminal_stream_event(&events) {
                    Some(event) => Ok(event.clone()),
                    None => Err(TransportError("stream missing terminal event".into())),
                },
                Err(e) => Ok(WireMessage::Result(ResultMsg {
                    id: inv.id.clone(),
                    kind: ResultKind::InvokeResult,
                    success: false,
                    output: None,
                    error: Some(e),
                })),
            }
        } else {
            match self
                .router
                .invoke_sync(&inv.capability, &inv.input.to_string(), &ctx)
            {
                Ok(output) => Ok(WireMessage::Result(ResultMsg {
                    id: inv.id.clone(),
                    kind: ResultKind::InvokeResult,
                    success: true,
                    output: Some(output),
                    error: None,
                })),
                Err(e) => Ok(WireMessage::Result(ResultMsg {
                    id: inv.id.clone(),
                    kind: ResultKind::InvokeResult,
                    success: false,
                    output: None,
                    error: Some(e),
                })),
            }
        };

        self.pending_cancels.lock().remove(&inv.id);
        self.reentrancy
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        result
    }

    /// 宿主调用 guest `handler.invoke`（异步，经 transport）。
    pub async fn invoke_handler(
        &self,
        handler_id: &str,
        event: Value,
        invoke_ctx: &InvokeContext,
    ) -> Result<HandlerResult, ExtensionError> {
        let input = json!({
            "handler_id": handler_id,
            "event": event
        });
        let inv = astrcode_extension_sdk::s5r::InvokeMsg {
            id: new_request_id(),
            capability: CAP_HANDLER_INVOKE.to_string(),
            input,
            stream: false,
            caller_extension_id: Some(self.extension_id()),
        };
        let resp = self
            .exchange_async(WireMessage::Invoke(inv), invoke_ctx)
            .await?;
        parse_handler_result(resp)
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

    async fn exchange_async(
        &self,
        msg: WireMessage,
        invoke_ctx: &InvokeContext,
    ) -> Result<WireMessage, ExtensionError> {
        let json = serde_json::to_string(&msg)
            .map_err(|e| ExtensionError::Internal(format!("serialize WireMessage: {e}")))?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.job_tx
            .send(crate::wasm_peer_transport::TransportJob {
                request_json: json,
                invoke_ctx: invoke_ctx.clone(),
                reply: tx,
            })
            .map_err(|_| ExtensionError::Internal("wasm peer thread exited".into()))?;
        rx.await
            .map_err(|_| ExtensionError::Internal("wasm peer reply dropped".into()))?
            .map_err(|e| ExtensionError::Internal(e.0))
    }
}

fn parse_initialize(init: &InitializeMsg) -> Result<PeerRegistration, TransportError> {
    let version = init
        .metadata
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0")
        .to_string();

    let id = init.peer.name.clone();
    if id.trim().is_empty() {
        return Err(TransportError(
            "initialize peer name (extension id) is empty".into(),
        ));
    }
    let proto = init
        .metadata
        .get("protocol")
        .and_then(|p| p.get("s5r"))
        .and_then(|v| v.as_str());
    match proto {
        Some(S5R_VERSION) => {},
        Some(other) => {
            return Err(TransportError(format!(
                "unsupported protocol.s5r version: {other} (expected {S5R_VERSION})"
            )));
        },
        None => {
            let stack = init.metadata.get("stack").and_then(|s| s.as_str());
            if stack != Some(S5R_STACK) {
                return Err(TransportError(format!(
                    "metadata.protocol.s5r must be \"{S5R_VERSION}\" (or metadata.stack \
                     \"{S5R_STACK}\")"
                )));
            }
        },
    }

    let capabilities: Vec<ExtensionCapability> = init
        .requested_capabilities
        .iter()
        .filter_map(|c| capability_from_wire(c))
        .collect();

    let tools: Vec<ManifestTool> = init
        .metadata
        .get("tools")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let commands: Vec<ManifestCommand> = init
        .metadata
        .get("commands")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let hooks: Vec<ManifestHook> = init
        .metadata
        .get("hooks")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let extension_events: Vec<ExtensionEventDecl> = init
        .metadata
        .get("extension_events")
        .and_then(|v| serde_json::from_value::<Vec<ManifestExtensionEvent>>(v.clone()).ok())
        .map(|evs| {
            evs.into_iter()
                .map(|e| ExtensionEventDecl {
                    event_type: e.event_type,
                    schema_version: e.schema_version,
                    durable: e.durable,
                    max_payload_bytes: e.max_payload_bytes,
                })
                .collect()
        })
        .unwrap_or_default();

    for cap in &init.provided_capabilities {
        if let Some(tool_name) = cap.name.strip_prefix(&format!("{id}.tool.")) {
            if !tools.iter().any(|t| t.name == tool_name) {
                // tools also listed in metadata; provided_capabilities is authoritative for LLM
            }
        }
    }

    for h in &init.handlers {
        if let Some(rest) = h.handler_id.strip_prefix(&format!("{id}:hook:")) {
            if !hooks.iter().any(|x| x.on == rest) {
                // handler catalog duplicates metadata hooks
                let _ = rest;
            }
        }
    }

    for cap in &init.provided_capabilities {
        if !is_reserved_capability_prefix(&cap.name) && !cap.name.starts_with(&format!("{id}.")) {
            return Err(TransportError(format!(
                "invalid provided capability name: {}",
                cap.name
            )));
        }
    }

    Ok(PeerRegistration {
        extension_id: id,
        version,
        capabilities,
        tools,
        commands,
        hooks,
        extension_events,
    })
}

fn parse_handler_result(msg: WireMessage) -> Result<HandlerResult, ExtensionError> {
    match msg {
        WireMessage::Result(r) if r.kind == ResultKind::InvokeResult => {
            if !r.success {
                let err = r
                    .error
                    .map(|e| e.message)
                    .unwrap_or_else(|| "invoke failed".into());
                return Ok(HandlerResult::err(err));
            }
            let output = r.output.unwrap_or(Value::Null);
            serde_json::from_value(output)
                .map_err(|e| ExtensionError::Internal(format!("parse HandlerResult: {e}")))
        },
        WireMessage::Event(e) if e.phase == s5r::EventPhase::Completed => {
            serde_json::from_value(e.data).map_err(|e| {
                ExtensionError::Internal(format!("parse completed HandlerResult: {e}"))
            })
        },
        other => Err(ExtensionError::Internal(format!(
            "expected invoke_result, got {:?}",
            std::mem::discriminant(&other)
        ))),
    }
}

/// 从流式 Event 序列中取出终态 `completed` / `failed`（`peer_exchange` 单次往返仅回一条）。
fn terminal_stream_event(events: &[WireMessage]) -> Option<&WireMessage> {
    events.iter().rev().find(|msg| {
        matches!(
            msg,
            WireMessage::Event(e)
                if e.phase == EventPhase::Completed || e.phase == EventPhase::Failed
        )
    })
}

#[cfg(test)]
mod tests {
    use astrcode_extension_sdk::s5r::{InitializeMsg, PeerInfo};

    use super::*;

    fn sample_initialize(metadata: serde_json::Value) -> InitializeMsg {
        InitializeMsg {
            id: "init-1".into(),
            peer: PeerInfo {
                name: "demo-ext".into(),
                role: "extension".into(),
                version: "0.1.0".into(),
            },
            handlers: vec![],
            provided_capabilities: vec![],
            requested_capabilities: vec![],
            metadata,
        }
    }

    #[test]
    fn parse_initialize_rejects_unsupported_protocol_version() {
        let init = sample_initialize(json!({ "protocol": { "s5r": "0.9" } }));
        let err = parse_initialize(&init).unwrap_err();
        assert!(err.0.contains("unsupported protocol.s5r"));
    }

    #[test]
    fn parse_initialize_accepts_s5r_version() {
        let init = sample_initialize(json!({
            "protocol": { "s5r": S5R_VERSION },
            "tools": [],
            "commands": [],
            "hooks": [],
            "extension_events": []
        }));
        let reg = parse_initialize(&init).unwrap();
        assert_eq!(reg.extension_id, "demo-ext");
    }

    #[test]
    fn terminal_stream_event_picks_completed() {
        let events = vec![
            WireMessage::Event(s5r::EventMsg {
                id: "r1".into(),
                phase: EventPhase::Started,
                data: Value::Null,
                error: None,
            }),
            WireMessage::Event(s5r::EventMsg {
                id: "r1".into(),
                phase: EventPhase::Completed,
                data: json!({ "ok": true }),
                error: None,
            }),
        ];
        let terminal = terminal_stream_event(&events).unwrap();
        assert!(matches!(
            terminal,
            WireMessage::Event(e) if e.phase == EventPhase::Completed
        ));
    }
}

// Manifest types used in Initialize - guest sends tools in provided_capabilities OR we parse from
// handlers metadata For s5r-guest we'll send structured metadata with tools/hooks arrays
