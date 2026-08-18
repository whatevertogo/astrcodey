use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use astrcode_extension_sdk::wire::{WireErrorCode, protocol::ErrorPayload};
use astrcode_s5r_runtime::InvocationCancellation;
use parking_lot::RwLock;

use crate::{
    extension_manifest::ExtensionRegistration,
    host_router::{HostRouter, InvokeContext, decls_to_map},
};

const MAX_REENTRANCY: u32 = 8;
const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const STDERR_TAIL_BYTES: usize = 16 * 1024;

pub(super) struct StderrTaskGuard {
    task: Option<tokio::task::JoinHandle<()>>,
}

impl StderrTaskGuard {
    pub(super) fn new(task: Option<tokio::task::JoinHandle<()>>) -> Self {
        Self { task }
    }

    pub(super) async fn wait(&mut self) {
        let Some(task) = &mut self.task else {
            return;
        };
        match tokio::time::timeout(STDERR_DRAIN_TIMEOUT, &mut *task).await {
            Ok(Ok(())) => {},
            Ok(Err(error)) if !error.is_cancelled() => {
                tracing::warn!(%error, "S5R stderr drain task failed");
            },
            Ok(Err(_)) => {},
            Err(_) => {
                tracing::warn!("S5R stderr drain timed out after process termination");
                task.abort();
                let _ = task.await;
            },
        }
        self.task = None;
    }
}

impl Drop for StderrTaskGuard {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

pub(super) struct HostInvokeState {
    pub(super) router: Arc<HostRouter>,
    pub(super) registration: ExtensionRegistration,
    pub(super) reentrancy: Arc<AtomicU32>,
    pub(super) invoke_contexts: RwLock<HashMap<String, InvokeContext>>,
    pub(super) detached_invoke_context: RwLock<Option<InvokeContext>>,
}

pub(super) struct ReentrancyGuard {
    counter: Arc<AtomicU32>,
}

impl ReentrancyGuard {
    fn enter(counter: &Arc<AtomicU32>) -> Result<Self, ErrorPayload> {
        let depth = counter.fetch_add(1, Ordering::SeqCst);
        if depth >= MAX_REENTRANCY {
            counter.fetch_sub(1, Ordering::SeqCst);
            return Err(ErrorPayload::new(
                WireErrorCode::ReentrancyExceeded,
                "reentrancy depth exceeded",
            ));
        }
        Ok(Self {
            counter: Arc::clone(counter),
        })
    }
}

impl Drop for ReentrancyGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(super) fn prepare_host_invoke(
    state: &HostInvokeState,
    operation: &str,
    parent_invoke_id: Option<&str>,
    cancellation: &InvocationCancellation,
) -> Result<(ReentrancyGuard, InvokeContext), ErrorPayload> {
    if cancellation.is_cancelled() {
        return Err(ErrorPayload::new(
            WireErrorCode::Cancelled,
            "host invoke cancelled",
        ));
    }
    if !operation.starts_with("astrcode.") {
        return Err(ErrorPayload::new(
            WireErrorCode::UnknownCapability,
            format!("host does not provide capability {operation}"),
        ));
    }
    let reentrancy = ReentrancyGuard::enter(&state.reentrancy)?;
    let parent_context =
        parent_invoke_id.and_then(|parent_id| state.invoke_contexts.read().get(parent_id).cloned());
    let mut context = resolve_host_invoke_context(
        parent_invoke_id,
        parent_context,
        state.detached_invoke_context.read().clone(),
    )?;
    context.extension_id = state.registration.extension_id.clone();
    context.declared_capabilities = state.registration.capabilities.clone();
    context.event_declarations = decls_to_map(&state.registration.custom_events);
    // 有父调用 ⇒ 调用发生在 handler 上下文,可能持有 admission permit(Sequential handler
    // 占满全部 permit),此时 wait_for_result 等待的 turn 若需回调本扩展会形成互等;
    // 无父调用 ⇒ 来自扩展后台任务,不持有 permit,允许同步等待。
    context.on_peer_io_thread = parent_invoke_id.is_some();
    context.cancel_token = Some(cancellation.cancellation_token());
    Ok((reentrancy, context))
}

fn resolve_host_invoke_context(
    parent_invoke_id: Option<&str>,
    parent_context: Option<InvokeContext>,
    detached_context: Option<InvokeContext>,
) -> Result<InvokeContext, ErrorPayload> {
    match parent_invoke_id {
        Some(parent_id) => parent_context.ok_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::UnknownParentInvoke,
                format!("parent invoke {parent_id} is no longer active"),
            )
        }),
        None => detached_context.ok_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::BackendUnavailable,
                "extension host context is not ready until startup completes",
            )
        }),
    }
}

pub(super) async fn drain_stderr(mut stderr: tokio::process::ChildStderr, extension_id: String) {
    use tokio::io::AsyncReadExt as _;

    let mut tail = Vec::new();
    let mut dropped_bytes = 0usize;
    let mut buffer = [0_u8; 4096];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => append_stderr_tail(&mut tail, &mut dropped_bytes, &buffer[..read]),
            Err(error) => {
                tracing::warn!(%extension_id, %error, "S5R stderr read failed");
                break;
            },
        }
    }
    if !tail.is_empty() {
        tracing::warn!(
            %extension_id,
            dropped_bytes,
            stderr = %String::from_utf8_lossy(&tail),
            "S5R extension stderr"
        );
    }
}

fn append_stderr_tail(tail: &mut Vec<u8>, dropped_bytes: &mut usize, chunk: &[u8]) {
    tail.extend_from_slice(chunk);
    if tail.len() > STDERR_TAIL_BYTES {
        let overflow = tail.len() - STDERR_TAIL_BYTES;
        tail.drain(..overflow);
        *dropped_bytes = dropped_bytes.saturating_add(overflow);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> HostInvokeState {
        HostInvokeState {
            router: Arc::new(HostRouter::from_backends(
                crate::host_router::HostBackends::default(),
            )),
            registration: ExtensionRegistration {
                extension_id: "ext".into(),
                version: "0.0.0".into(),
                required_transport_features: vec![],
                capabilities: vec![],
                tools: vec![],
                commands: vec![],
                subscriptions: vec![],
                http_routes: vec![],
                custom_events: vec![],
                custom_event_subscriptions: vec![],
            },
            reentrancy: Arc::new(AtomicU32::new(0)),
            invoke_contexts: RwLock::new(HashMap::new()),
            detached_invoke_context: RwLock::new(Some(InvokeContext::default())),
        }
    }

    #[test]
    fn handler_context_marker_tracks_parent_invoke_presence() {
        let state = test_state();
        let cancellation = InvocationCancellation::default();

        let (_guard, context) =
            prepare_host_invoke(&state, "astrcode.session.root.state", None, &cancellation)
                .expect("detached invoke");
        assert!(
            !context.on_peer_io_thread,
            "background invoke holds no admission permit"
        );

        state
            .invoke_contexts
            .write()
            .insert("parent-1".into(), InvokeContext::default());
        let (_guard, context) = prepare_host_invoke(
            &state,
            "astrcode.session.root.state",
            Some("parent-1"),
            &cancellation,
        )
        .expect("nested invoke");
        assert!(
            context.on_peer_io_thread,
            "handler invoke may hold an admission permit"
        );
    }
}
