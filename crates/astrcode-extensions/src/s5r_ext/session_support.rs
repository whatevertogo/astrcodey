use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use astrcode_extension_contract::{
    WireErrorCode,
    protocol::{ErrorPayload, HandlerDescriptor},
};
use parking_lot::RwLock;
use tokio_util::sync::CancellationToken;

use crate::{
    extension_manifest::ExtensionRegistration,
    host_router::{HostRouter, InvokeContext, decls_to_map},
};

const MAX_REENTRANCY: u32 = 8;
const STDERR_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

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
    pub(super) registration: Arc<RwLock<Option<ExtensionRegistration>>>,
    pub(super) reentrancy: Arc<AtomicU32>,
    pub(super) invoke_contexts: Arc<RwLock<HashMap<String, InvokeContext>>>,
    pub(super) detached_invoke_context: Arc<RwLock<Option<InvokeContext>>>,
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
    cancellation: &CancellationToken,
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
    let registration = state.registration.read().clone().ok_or_else(|| {
        ErrorPayload::new(WireErrorCode::NotInitialized, "extension not initialized")
    })?;
    let parent_context =
        parent_invoke_id.and_then(|parent_id| state.invoke_contexts.read().get(parent_id).cloned());
    let mut context = resolve_host_invoke_context(
        parent_invoke_id,
        parent_context,
        state.detached_invoke_context.read().clone(),
    )?;
    context.extension_id = registration.extension_id().to_owned();
    context.declared_capabilities = registration.capabilities().to_vec();
    context.event_declarations = decls_to_map(registration.custom_events());
    context.on_peer_io_thread = true;
    context.cancel_token = Some(cancellation.clone());
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

pub(super) fn validate_initialize_handlers(
    registration: &ExtensionRegistration,
    handlers: &[HandlerDescriptor],
) -> Result<(), String> {
    let expected = registration.expected_handler_descriptors()?;
    let extension_id = registration.extension_id();
    let expected_ids = expected
        .iter()
        .map(|descriptor| descriptor.handler_id.as_str())
        .collect::<HashSet<_>>();
    let mut actual_by_id = HashMap::new();
    let mut actual_parts = Vec::with_capacity(handlers.len());
    for handler in handlers {
        let (kind, name) =
            crate::extension_manifest::parse_handler_id(extension_id, &handler.handler_id)?;
        if actual_by_id
            .insert(handler.handler_id.as_str(), handler)
            .is_some()
        {
            return Err(format!(
                "initialize declares duplicate handler {}",
                handler.handler_id
            ));
        }
        actual_parts.push((handler, kind, name));
    }

    for expected_handler in &expected {
        let (expected_kind, expected_name) = crate::extension_manifest::parse_handler_id(
            extension_id,
            &expected_handler.handler_id,
        )?;
        let Some(actual) = actual_by_id.get(expected_handler.handler_id.as_str()) else {
            if let Some((actual, actual_kind, _)) =
                actual_parts.iter().find(|(_, actual_kind, actual_name)| {
                    *actual_name == expected_name && *actual_kind != expected_kind
                })
            {
                return Err(format!(
                    "handler {} has kind {actual_kind}, expected {expected_kind}",
                    actual.handler_id
                ));
            }
            return Err(format!(
                "initialize is missing handler {}",
                expected_handler.handler_id
            ));
        };
        if actual.description != expected_handler.description {
            return Err(format!(
                "handler {} description does not match initialize metadata",
                expected_handler.handler_id
            ));
        }
        if actual.input_schema != expected_handler.input_schema {
            return Err(format!(
                "handler {} input schema does not match initialize metadata",
                expected_handler.handler_id
            ));
        }
    }

    if let Some(extra) = handlers
        .iter()
        .find(|handler| !expected_ids.contains(handler.handler_id.as_str()))
    {
        return Err(format!(
            "initialize declares unexpected handler {}",
            extra.handler_id
        ));
    }
    Ok(())
}

pub(super) async fn drain_stderr(stderr: tokio::process::ChildStderr) {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut reader = BufReader::new(stderr).lines();
    while let Ok(Some(_line)) = reader.next_line().await {}
}
