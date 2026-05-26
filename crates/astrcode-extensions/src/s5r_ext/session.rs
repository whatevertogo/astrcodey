//! 宿主侧 s5r Peer 会话（子进程 stdio 帧）。

use std::{
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
};

use astrcode_core::extension::{ExtensionCapability, ExtensionError, ExtensionEventDecl};
use astrcode_extension_sdk::{
    runtime::{
        CancelToken, InitializeHandler, InvokeHandler, InvokeReply, OutboundInvokeControl, Peer,
        StdioFrameTransport,
    },
    s5r::{
        CAP_HANDLER_INVOKE, ErrorPayload, EventMsg, EventPhase, InitializeMsg, InitializeOutput,
        InvokeMsg, PeerInfo, S5R_STACK, S5R_VERSION, WireMessage, effects::HandlerResult,
    },
};
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use crate::{
    extension_manifest::ExtensionRegistration,
    host_router::{HostRouter, InvokeContext, decls_to_map},
    s5r_ext::protocol::S5R_PROTOCOL_VERSION,
};

const MAX_REENTRANCY: u32 = 8;
const INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const INVOKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

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
pub(crate) enum S5rSessionError {
    #[error("{0}")]
    Msg(String),
}

pub(crate) struct S5rSession {
    child: Mutex<Option<Child>>,
    peer: Arc<Peer<StdioFrameTransport>>,
    registration: Arc<RwLock<Option<ExtensionRegistration>>>,
    active_invoke: Arc<RwLock<Option<InvokeContext>>>,
    outbound_invoke_id: Arc<RwLock<Option<String>>>,
    in_flight: Arc<tokio::sync::Mutex<()>>,
}

impl S5rSession {
    pub async fn spawn(
        program: &str,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
        router: Arc<HostRouter>,
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
            .map_err(|e| format!("spawn s5r extension {program}: {e}"))?;
        let stdin = child.stdin.take().ok_or("s5r child missing stdin")?;
        let stdout = child.stdout.take().ok_or("s5r child missing stdout")?;
        let stderr = child.stderr.take().ok_or("s5r child missing stderr")?;
        tokio::spawn(drain_stderr(stderr));

        let transport = StdioFrameTransport::new(stdin, stdout);
        let peer = Peer::new(
            transport,
            PeerInfo {
                name: "astrcode-host".into(),
                role: "core".into(),
                version: Some(S5R_STACK.into()),
            },
        );

        let registration = Arc::new(RwLock::new(None::<ExtensionRegistration>));
        let reentrancy = Arc::new(AtomicU32::new(0));
        let default_working_dir = Arc::new(RwLock::new(working_dir.map(str::to_string)));
        let active_invoke = Arc::new(RwLock::new(None));

        let registration_for_init = Arc::clone(&registration);
        let init_handler: InitializeHandler = Arc::new(move |init| {
            let registration_for_init = Arc::clone(&registration_for_init);
            Box::pin(async move { handle_initialize(init, &registration_for_init) })
        });
        peer.set_initialize_handler(init_handler);

        let router_invoke = Arc::clone(&router);
        let registration_invoke = Arc::clone(&registration);
        let reentrancy_invoke = Arc::clone(&reentrancy);
        let active_invoke_invoke = Arc::clone(&active_invoke);
        let default_wd_invoke = Arc::clone(&default_working_dir);
        let invoke_handler: InvokeHandler = Arc::new(move |invoke, token| {
            let router_invoke = Arc::clone(&router_invoke);
            let registration_invoke = Arc::clone(&registration_invoke);
            let reentrancy_invoke = Arc::clone(&reentrancy_invoke);
            let active_invoke_invoke = Arc::clone(&active_invoke_invoke);
            let default_wd_invoke = Arc::clone(&default_wd_invoke);
            Box::pin(async move {
                handle_host_invoke(
                    &router_invoke,
                    &registration_invoke,
                    &reentrancy_invoke,
                    &active_invoke_invoke,
                    &default_wd_invoke,
                    invoke,
                    token,
                )
                .await
            })
        });
        peer.set_invoke_handler(invoke_handler);

        peer.start().await.map_err(|e| e.to_string())?;
        peer.wait_remote_initialized(INIT_TIMEOUT)
            .await
            .map_err(|e| format!("s5r initialize: {e}"))?;

        Ok(Arc::new(Self {
            child: Mutex::new(Some(child)),
            peer,
            registration,
            active_invoke,
            outbound_invoke_id: Arc::new(RwLock::new(None)),
            in_flight: Arc::new(tokio::sync::Mutex::new(())),
        }))
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

    pub fn event_decls(&self) -> std::collections::HashMap<String, ExtensionEventDecl> {
        self.registration
            .read()
            .as_ref()
            .map(|r| decls_to_map(&r.extension_events))
            .unwrap_or_default()
    }

    pub async fn ping(&self) -> Result<(), S5rSessionError> {
        let _ = self
            .invoke_handler(
                &format!("{}:tool:ping", self.extension_id()),
                json!({ "on": "tool", "name": "ping", "input": {} }),
                &InvokeContext::default(),
            )
            .await
            .map_err(|e| S5rSessionError::Msg(e.to_string()))?;
        Ok(())
    }

    pub async fn shutdown(&self) {
        let outbound_id = self.outbound_invoke_id.read().clone();
        if let Some(id) = outbound_id {
            self.peer.cancel(&id, "session_shutdown").await;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        self.peer.stop().await;
        let child = self.child.lock().take();
        if let Some(mut child) = child {
            let _ = child.kill().await;
        }
    }

    pub async fn invoke_handler(
        &self,
        handler_id: &str,
        event: Value,
        invoke_ctx: &InvokeContext,
    ) -> Result<HandlerResult, ExtensionError> {
        let _guard = self.in_flight.lock().await;
        let _active = ActiveInvokeGuard::set(&self.active_invoke, invoke_ctx.clone());
        let control = OutboundInvokeControl {
            external_cancel: invoke_ctx.cancel_token.clone(),
            track_outbound_id: Some(Arc::clone(&self.outbound_invoke_id)),
        };
        let output = tokio::time::timeout(
            INVOKE_TIMEOUT,
            self.peer.invoke(
                CAP_HANDLER_INVOKE,
                json!({
                    "handler_id": handler_id,
                    "event": event,
                    "caller_extension_id": self.extension_id(),
                }),
                Some(&self.extension_id()),
                control,
            ),
        )
        .await
        .map_err(|_| ExtensionError::Internal("s5r handler invoke timed out".into()))?
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        serde_json::from_value(output)
            .map_err(|e| ExtensionError::Internal(format!("parse HandlerResult: {e}")))
    }

    pub async fn invoke_handler_with_continuations(
        &self,
        handler_id: &str,
        event: Value,
        invoke_ctx: &InvokeContext,
    ) -> Result<HandlerResult, ExtensionError> {
        const MAX_CONTINUATION_DEPTH: u32 = 16;
        let mut stack = vec![(handler_id.to_string(), event, 0u32)];
        let mut first: Option<HandlerResult> = None;
        while let Some((hid, ev, depth)) = stack.pop() {
            if depth > MAX_CONTINUATION_DEPTH {
                return Err(ExtensionError::Internal(format!(
                    "continuation depth exceeded (max {MAX_CONTINUATION_DEPTH})"
                )));
            }
            let mut resp = self.invoke_handler(&hid, ev, invoke_ctx).await?;
            let continuations = std::mem::take(&mut resp.continuations);
            if first.is_none() {
                first = Some(resp);
            }
            for cont in continuations.iter().rev() {
                let (nh, ne) = cont.handler_id_for_extension(&self.extension_id());
                stack.push((nh, ne, depth + 1));
            }
        }
        first.ok_or_else(|| ExtensionError::Internal("empty handler chain".into()))
    }
}

impl Drop for S5rSession {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.lock().take() {
            let _ = child.start_kill();
        }
    }
}

fn handle_initialize(
    init: InitializeMsg,
    registration: &Arc<RwLock<Option<ExtensionRegistration>>>,
) -> Result<InitializeOutput, ErrorPayload> {
    let reg = crate::extension_manifest::registration_from_s5r_metadata(
        &init.metadata,
        S5R_PROTOCOL_VERSION,
    )
    .map_err(|e| ErrorPayload::new("invalid_manifest", e))?;
    let caps = HostRouter::catalog_for_grants(&reg.capabilities);
    *registration.write() = Some(reg);
    Ok(InitializeOutput {
        peer: PeerInfo {
            name: "astrcode-host".into(),
            role: "core".into(),
            version: Some(S5R_STACK.into()),
        },
        protocol_version: Some(S5R_VERSION.into()),
        capabilities: caps,
        metadata: json!({ "wire_codec": "json" }),
    })
}

fn bridge_peer_cancel_token(peer_token: &CancelToken) -> CancellationToken {
    let host_token = CancellationToken::new();
    if peer_token.is_cancelled() {
        host_token.cancel();
        return host_token;
    }
    let peer = peer_token.clone();
    let host = host_token.clone();
    tokio::spawn(async move {
        while !peer.is_cancelled() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        host.cancel();
    });
    host_token
}

async fn handle_host_invoke(
    router: &Arc<HostRouter>,
    registration: &Arc<RwLock<Option<ExtensionRegistration>>>,
    reentrancy: &Arc<AtomicU32>,
    active_invoke: &Arc<RwLock<Option<InvokeContext>>>,
    default_working_dir: &Arc<RwLock<Option<String>>>,
    invoke: InvokeMsg,
    token: CancelToken,
) -> Result<InvokeReply, ErrorPayload> {
    token
        .raise_if_cancelled()
        .map_err(|e| ErrorPayload::new("cancelled", e))?;
    if !invoke.capability.starts_with("astrcode.") {
        return Err(ErrorPayload::new(
            "unknown_capability",
            format!("host does not provide capability {}", invoke.capability),
        ));
    }
    let depth = reentrancy.fetch_add(1, Ordering::SeqCst);
    if depth >= MAX_REENTRANCY {
        reentrancy.fetch_sub(1, Ordering::SeqCst);
        return Err(ErrorPayload::new(
            "reentrancy_exceeded",
            "reentrancy depth exceeded",
        ));
    }
    let reg = registration.read().clone();
    let Some(reg) = reg else {
        reentrancy.fetch_sub(1, Ordering::SeqCst);
        return Err(ErrorPayload::new(
            "not_initialized",
            "extension not initialized",
        ));
    };
    let mut ctx = active_invoke.read().clone().unwrap_or_default();
    if ctx.working_dir.is_none() {
        ctx.working_dir = default_working_dir.read().clone();
    }
    ctx.extension_id = reg.extension_id.clone();
    ctx.declared_capabilities = reg.capabilities.clone();
    ctx.event_declarations = decls_to_map(&reg.extension_events);
    ctx.on_peer_io_thread = true;
    ctx.cancel_token = Some(bridge_peer_cancel_token(&token));

    let result = if invoke.stream {
        router
            .invoke_stream_sync(
                &invoke.capability,
                &invoke.input.to_string(),
                &invoke.id,
                &ctx,
            )
            .map(|events| {
                events
                    .into_iter()
                    .filter_map(|wire| match wire {
                        WireMessage::Event(ev) => Some(ev),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
    } else {
        router
            .invoke_sync(&invoke.capability, &invoke.input.to_string(), &ctx)
            .map(|v| {
                vec![EventMsg {
                    id: invoke.id.clone(),
                    phase: EventPhase::Completed,
                    data: Value::Null,
                    output: v,
                    error: None,
                }]
            })
    };
    reentrancy.fetch_sub(1, Ordering::SeqCst);
    match result {
        Ok(events) if invoke.stream => Ok(InvokeReply::Events(events)),
        Ok(events) => Ok(InvokeReply::Value(
            events
                .into_iter()
                .find(|e| e.phase == EventPhase::Completed)
                .map(|e| e.output)
                .unwrap_or(Value::Null),
        )),
        Err(e) => Err(e),
    }
}

async fn drain_stderr(stderr: tokio::process::ChildStderr) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut reader = BufReader::new(stderr).lines();
    while let Ok(Some(_line)) = reader.next_line().await {}
}
