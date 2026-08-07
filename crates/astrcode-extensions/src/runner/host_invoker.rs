use std::{
    path::PathBuf,
    sync::{Arc, RwLock as StdRwLock},
};

use astrcode_core::{event::EventSender, tool::SessionOperations, types::SessionId};
use astrcode_extension_sdk::{
    extension::{
        ExtensionCallContext, ExtensionCapability, ExtensionError, ExtensionEventDecl,
        ExtensionPaths, ExtensionTasks, RuntimeHookCallContext, internal::extension_event_emitter,
    },
    host::{
        ExtensionHost, HOST_ERROR_CODE_INVALID_RESPONSE, HostError, HostOperation,
        internal::{HostInvoker, HostScope, extension_host},
    },
    s5r::{EventPhase, WireMessage},
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{ExtensionRunner, ExtensionView, HandlerIndex, bind_extension_event_sink};
use crate::host_router::{HostRouter, InvokeContext, decls_to_map};

pub(crate) struct ExtensionCallContextInput {
    pub(crate) session_id: Option<SessionId>,
    pub(crate) turn_id: Option<String>,
    pub(crate) tool_call_id: Option<String>,
    pub(crate) working_dir: Option<PathBuf>,
    pub(crate) session_store_dir: Option<PathBuf>,
    pub(crate) event_tx: Option<EventSender>,
    pub(crate) cancellation: CancellationToken,
}

impl ExtensionCallContextInput {
    pub(crate) fn unscoped(cancellation: CancellationToken) -> Self {
        Self {
            session_id: None,
            turn_id: None,
            tool_call_id: None,
            working_dir: None,
            session_store_dir: None,
            event_tx: None,
            cancellation,
        }
    }

    pub(super) fn from_hook(
        runtime: &RuntimeHookCallContext,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            session_id: Some(runtime.session_id().clone()),
            turn_id: runtime.turn_id().map(str::to_owned),
            tool_call_id: None,
            working_dir: Some(runtime.working_dir().to_path_buf()),
            session_store_dir: runtime
                .session_store_dir()
                .map(std::path::Path::to_path_buf),
            event_tx: runtime.event_tx().cloned(),
            cancellation,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ExtensionCallContextFactory {
    router: Arc<HostRouter>,
    session_ops: Arc<StdRwLock<Option<Arc<dyn SessionOperations>>>>,
}

impl ExtensionCallContextFactory {
    pub(super) fn new(
        router: Arc<HostRouter>,
        session_ops: Arc<StdRwLock<Option<Arc<dyn SessionOperations>>>>,
    ) -> Self {
        Self {
            router,
            session_ops,
        }
    }

    pub(crate) fn make_extension_call_context(
        &self,
        extension_id: &str,
        capabilities: &[ExtensionCapability],
        declarations: &[ExtensionEventDecl],
        tasks: ExtensionTasks,
        input: ExtensionCallContextInput,
    ) -> ExtensionCallContext {
        let ExtensionCallContextInput {
            session_id,
            turn_id,
            tool_call_id,
            working_dir,
            session_store_dir,
            event_tx,
            cancellation,
        } = input;
        let cancellation = linked_call_cancellation(&tasks, cancellation);
        let event_tx = if capabilities.contains(&ExtensionCapability::EmitEvents) {
            event_tx
        } else {
            None
        };
        let event_sink = event_tx.as_ref().and_then(|event_tx| {
            bind_extension_event_sink(extension_id, declarations, event_tx.clone())
        });
        let session_ops = if capabilities.iter().any(|capability| {
            matches!(
                capability,
                ExtensionCapability::SessionControl
                    | ExtensionCapability::SessionHistory
                    | ExtensionCapability::InputDelivery
            )
        }) {
            self.session_ops
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        } else {
            None
        };
        let invoke_context = InvokeContext {
            extension_id: extension_id.to_owned(),
            session_id: session_id.as_ref().map(|session_id| session_id.to_string()),
            tool_call_id,
            session_store_dir: session_store_dir.clone(),
            session_ops,
            event_tx,
            working_dir: working_dir
                .as_deref()
                .map(|path| path.to_string_lossy().into_owned()),
            cancel_token: Some(cancellation.clone()),
            tasks: Some(tasks.clone()),
            event_declarations: decls_to_map(declarations),
            declared_capabilities: capabilities.to_vec(),
            on_peer_io_thread: false,
        };
        let scope = HostScope::new(
            capabilities.iter().copied(),
            self.router.available_operations(&invoke_context),
            session_id.is_some(),
            working_dir.is_some(),
        );
        let host = extension_host(
            Arc::new(RouterHostInvoker {
                router: Arc::clone(&self.router),
                invoke_context,
            }),
            scope,
        );
        let global_store_dir = astrcode_core::config::defaults::astrcode_dir();
        let paths = ExtensionPaths::from_runtime(
            extension_id,
            Some(&global_store_dir),
            session_store_dir.as_deref(),
        );
        let events = extension_event_emitter(declarations.iter().cloned(), event_sink);

        ExtensionCallContext::from_runtime(
            extension_id,
            session_id,
            turn_id,
            working_dir,
            paths,
            host,
            events,
            tasks,
            cancellation,
        )
    }
}

fn linked_call_cancellation(
    tasks: &ExtensionTasks,
    caller: CancellationToken,
) -> CancellationToken {
    let combined = tasks.cancellation().child_token();
    if caller.is_cancelled() {
        combined.cancel();
        return combined;
    }

    let linked = combined.clone();
    tasks.spawn("call-cancellation", async move {
        tokio::select! {
            _ = caller.cancelled() => linked.cancel(),
            _ = linked.cancelled() => {},
        }
    });
    combined
}

impl ExtensionRunner {
    /// Binds the single router shared by in-process SDK calls and disk extensions.
    pub fn bind_host_router(&self, router: Arc<HostRouter>) {
        self.bindings.write().host_router = router;
    }

    /// Returns the process-wide restricted network backend retained across reloads.
    pub fn outbound_network_service(
        &self,
    ) -> Option<Arc<dyn astrcode_extension_sdk::host::internal::OutboundNetworkService>> {
        self.bindings.read().host_router.outbound_network_service()
    }

    pub(super) fn extension_call_context_factory(&self) -> ExtensionCallContextFactory {
        let bindings = self.bindings.read();
        ExtensionCallContextFactory::new(
            Arc::clone(&bindings.host_router),
            Arc::clone(&bindings.session_ops),
        )
    }
}

impl ExtensionView {
    pub(crate) fn make_registered_extension_call_context(
        &self,
        extension_id: &str,
        input: ExtensionCallContextInput,
    ) -> Result<ExtensionCallContext, ExtensionError> {
        self.make_registered_extension_call_context_from_index(&self.index, extension_id, input)
    }

    pub(super) fn make_registered_extension_call_context_from_index(
        &self,
        index: &HandlerIndex,
        extension_id: &str,
        input: ExtensionCallContextInput,
    ) -> Result<ExtensionCallContext, ExtensionError> {
        let capabilities = index.capabilities.get(extension_id).ok_or_else(|| {
            ExtensionError::Internal(format!(
                "missing capability attribution for extension {extension_id}"
            ))
        })?;
        let tasks = index
            .extension_tasks
            .get(extension_id)
            .cloned()
            .ok_or_else(|| {
                ExtensionError::Internal(format!("missing task owner for extension {extension_id}"))
            })?;
        let declarations = index
            .extension_event_decls
            .get(extension_id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        Ok(self.call_context_factory.make_extension_call_context(
            extension_id,
            capabilities,
            declarations,
            tasks,
            input,
        ))
    }
}

struct RouterHostInvoker {
    router: Arc<HostRouter>,
    invoke_context: InvokeContext,
}

#[async_trait::async_trait]
impl HostInvoker for RouterHostInvoker {
    async fn invoke(&self, operation: HostOperation, input: Value) -> Result<Value, HostError> {
        self.router
            .invoke(operation.wire_name(), input, &self.invoke_context)
            .await
            .map_err(HostError::from)
    }

    async fn invoke_collected_stream(
        &self,
        operation: HostOperation,
        input: Value,
    ) -> Result<Value, HostError> {
        let events = self
            .router
            .invoke_stream(
                operation.wire_name(),
                input,
                "bundled-collected-stream",
                &self.invoke_context,
            )
            .await
            .map_err(HostError::from)?;
        for message in events {
            let WireMessage::Event(event) = message else {
                continue;
            };
            match event.phase {
                EventPhase::Completed => return Ok(event.output),
                EventPhase::Failed => {
                    return Err(event.error.map(HostError::from).unwrap_or_else(|| {
                        HostError::new(
                            HOST_ERROR_CODE_INVALID_RESPONSE,
                            format!(
                                "{} stream failed without an error payload",
                                operation.wire_name()
                            ),
                        )
                    }));
                },
                EventPhase::Started | EventPhase::Delta => {},
            }
        }
        Err(HostError::new(
            HOST_ERROR_CODE_INVALID_RESPONSE,
            format!(
                "{} stream ended without a terminal event",
                operation.wire_name()
            ),
        ))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

pub(crate) fn transport_invoke_context(host: &ExtensionHost) -> Option<InvokeContext> {
    astrcode_extension_sdk::host::internal::invoker(host)
        .as_any()
        .downcast_ref::<RouterHostInvoker>()
        .map(|invoker| invoker.invoke_context.clone())
}

#[cfg(test)]
mod tests {
    use std::{path::Path, time::Duration};

    use astrcode_core::llm::{LlmEvent, LlmMessage, LlmProvider, ModelLimits};
    use astrcode_extension_sdk::host::HostLlmChatRequest;

    use super::*;
    use crate::host_router::HostBackends;

    #[tokio::test]
    async fn default_factory_preserves_call_attribution_and_reports_unavailable_backends() {
        let runner = ExtensionRunner::new(Duration::from_secs(1));
        let session_id = SessionId::new("session-1");
        let session_store_dir = PathBuf::from("/sessions/session-1");
        let cancellation = CancellationToken::new();
        let context = runner
            .extension_call_context_factory()
            .make_extension_call_context(
                "review-extension",
                &[ExtensionCapability::SessionControl],
                &[],
                ExtensionTasks::new("review-extension"),
                ExtensionCallContextInput {
                    session_id: Some(session_id.clone()),
                    turn_id: Some("turn-1".into()),
                    tool_call_id: Some("call-1".into()),
                    working_dir: Some(PathBuf::from("/workspace")),
                    session_store_dir: Some(session_store_dir.clone()),
                    event_tx: None,
                    cancellation: cancellation.clone(),
                },
            );

        assert_eq!(context.extension_id(), "review-extension");
        assert_eq!(context.session_id(), Some(&session_id));
        assert_eq!(context.turn_id(), Some("turn-1"));
        assert_eq!(context.working_dir(), Some(Path::new("/workspace")));
        let expected_global_dir =
            astrcode_core::config::defaults::astrcode_dir().join("extension_data/review-extension");
        assert_eq!(
            context.paths().global_data_dir(),
            Some(expected_global_dir.as_path())
        );
        assert_eq!(
            context.paths().session_data_dir().unwrap(),
            session_store_dir.join("extension_data/review-extension")
        );
        cancellation.cancel();
        context.cancellation().cancelled().await;
        assert!(context.cancellation().is_cancelled());
        let transport = transport_invoke_context(context.host())
            .expect("runner host should retain its internal S5R transport context");
        assert_eq!(transport.session_id.as_deref(), Some("session-1"));
        assert_eq!(transport.tool_call_id.as_deref(), Some("call-1"));
        assert_eq!(transport.session_store_dir, Some(session_store_dir));
        assert!(
            transport
                .cancel_token
                .is_some_and(|token| token.is_cancelled())
        );

        let error = match context.host().session_control() {
            Ok(_) => panic!("an unbound default router must not expose session control"),
            Err(error) => error,
        };
        assert!(error.is_backend_unavailable());
    }

    struct DelayedLlm;

    #[async_trait::async_trait]
    impl LlmProvider for DelayedLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<astrcode_core::tool::ToolDefinition>,
        ) -> Result<tokio::sync::mpsc::UnboundedReceiver<LlmEvent>, astrcode_core::llm::LlmError>
        {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let _ = tx.send(LlmEvent::Done {
                    finish_reason: "stop".into(),
                });
            });
            Ok(rx)
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 1_024,
                max_output_tokens: 1_024,
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_host_invoke_yields_to_outer_timeout() {
        let router = Arc::new(HostRouter::from_backends(HostBackends {
            main_llm: Some(Arc::new(DelayedLlm)),
            ..Default::default()
        }));
        let cancellation = CancellationToken::new();
        let invoker = RouterHostInvoker {
            router,
            invoke_context: InvokeContext {
                declared_capabilities: vec![ExtensionCapability::MainModel],
                cancel_token: Some(cancellation.clone()),
                ..Default::default()
            },
        };
        let input =
            serde_json::to_value(HostLlmChatRequest::new(vec![LlmMessage::user("hello")])).unwrap();

        let result = tokio::time::timeout(
            Duration::from_millis(30),
            invoker.invoke(HostOperation::LlmMainChat, input),
        )
        .await;

        assert!(
            result.is_err(),
            "the async caller must retain timeout control"
        );
        cancellation.cancel();
    }
}
