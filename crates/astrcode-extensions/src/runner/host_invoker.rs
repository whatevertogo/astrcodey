use std::{
    path::PathBuf,
    sync::{Arc, RwLock as StdRwLock},
};

use astrcode_core::{
    event::{EventDeliveryReceipt, EventSendError, EventSender},
    tool::SessionOperations,
    types::SessionId,
};
use astrcode_extension_sdk::{
    extension::{
        CustomEventDeclaration, ExtensionCallContext, ExtensionCapability, ExtensionError,
        ExtensionPaths, ExtensionTasks, RuntimeHookCallContext,
        internal::{CustomEventSink, custom_event_emitter},
    },
    host::{
        ExtensionHost, HostError, HostOperation,
        internal::{HostInvoker, HostScope, extension_host},
    },
    model_stream::ModelStream,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{ExtensionRunner, ExtensionView, HandlerIndex};
use crate::host_router::{HostRouter, InvokeContext, decls_to_map};

pub(super) struct ExtensionCallContextInput {
    pub(super) session_id: Option<SessionId>,
    pub(super) tool_call_id: Option<String>,
    pub(super) working_dir: Option<PathBuf>,
    pub(super) session_store_dir: Option<PathBuf>,
    pub(super) event_tx: Option<EventSender>,
    pub(super) event_causation: Option<(astrcode_core::types::EventId, u8)>,
    pub(super) cancellation: CancellationToken,
}

/// Event emitter with the extension identity fixed by the runtime call-context factory.
struct BoundCustomEventSink {
    extension_id: String,
    event_tx: EventSender,
    causation: Option<(astrcode_core::types::EventId, u8)>,
}

fn bind_custom_event_sink(
    extension_id: &str,
    declarations: &[CustomEventDeclaration],
    event_tx: EventSender,
    causation: Option<(astrcode_core::types::EventId, u8)>,
) -> Option<Arc<dyn CustomEventSink>> {
    if declarations.is_empty() {
        return None;
    }
    Some(Arc::new(BoundCustomEventSink {
        extension_id: extension_id.to_owned(),
        event_tx,
        causation,
    }))
}

#[async_trait::async_trait]
impl CustomEventSink for BoundCustomEventSink {
    async fn emit(
        &self,
        event_type: &str,
        schema_version: u32,
        durable: bool,
        payload: serde_json::Value,
    ) -> Result<EventDeliveryReceipt, EventSendError> {
        self.event_tx
            .send_confirmed(crate::host_router::custom_event_payload(
                &self.extension_id,
                event_type,
                schema_version,
                durable,
                self.causation.clone(),
                payload,
            ))
            .await
    }

    fn try_emit(
        &self,
        event_type: &str,
        schema_version: u32,
        durable: bool,
        payload: serde_json::Value,
    ) -> Result<(), EventSendError> {
        self.event_tx.send(crate::host_router::custom_event_payload(
            &self.extension_id,
            event_type,
            schema_version,
            durable,
            self.causation.clone(),
            payload,
        ))
    }
}

impl ExtensionCallContextInput {
    pub(super) fn unscoped(cancellation: CancellationToken) -> Self {
        Self {
            session_id: None,
            tool_call_id: None,
            working_dir: None,
            session_store_dir: None,
            event_tx: None,
            event_causation: None,
            cancellation,
        }
    }

    pub(super) fn from_hook(
        runtime: &RuntimeHookCallContext,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            session_id: Some(runtime.session_id().clone()),
            tool_call_id: None,
            working_dir: Some(runtime.working_dir().to_path_buf()),
            session_store_dir: runtime
                .session_store_dir()
                .map(std::path::Path::to_path_buf),
            event_tx: runtime.event_tx().cloned(),
            event_causation: None,
            cancellation,
        }
    }
}

#[derive(Clone)]
pub(super) struct ExtensionCallContextFactory {
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

    pub(super) fn make_extension_call_context(
        &self,
        extension_id: &str,
        capabilities: &[ExtensionCapability],
        declarations: &[CustomEventDeclaration],
        tasks: ExtensionTasks,
        input: ExtensionCallContextInput,
    ) -> ExtensionCallContext {
        let ExtensionCallContextInput {
            session_id,
            tool_call_id,
            working_dir,
            session_store_dir,
            event_tx,
            event_causation,
            cancellation,
        } = input;
        let cancellation = linked_call_cancellation(&tasks, cancellation);
        let event_tx = if capabilities.contains(&ExtensionCapability::EmitCustomEvents) {
            event_tx
        } else {
            None
        };
        let event_sink = event_tx.as_ref().and_then(|event_tx| {
            bind_custom_event_sink(
                extension_id,
                declarations,
                event_tx.clone(),
                event_causation.clone(),
            )
        });
        let session_ops = if capabilities.iter().any(|capability| {
            matches!(
                capability,
                ExtensionCapability::SessionControl
                    | ExtensionCapability::SessionHistory
                    | ExtensionCapability::InputDelivery
            )
        }) {
            // 不变式：锁保护的 Option<Arc> 只在绑定时单次赋值，赋值本身不会 panic，
            // 因此 poison 不可能留下不一致状态，直接取回内部值是安全的。
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
            event_causation,
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
        let events = custom_event_emitter(declarations.iter().cloned(), event_sink);

        ExtensionCallContext::from_runtime(extension_id, paths, host, events, tasks, cancellation)
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
    pub(super) fn make_registered_extension_call_context(
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
        let generation = index.extensions.get(extension_id).ok_or_else(|| {
            ExtensionError::Internal(format!(
                "missing generation entry for extension {extension_id}"
            ))
        })?;
        Ok(self.call_context_factory.make_extension_call_context(
            extension_id,
            &generation.capabilities,
            &generation.custom_event_declarations,
            generation.tasks.clone(),
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

    async fn invoke_stream(
        &self,
        operation: HostOperation,
        input: Value,
    ) -> Result<ModelStream, HostError> {
        let stream = self
            .router
            .invoke_event_stream(operation.wire_name(), input, &self.invoke_context)
            .await
            .map_err(HostError::from)?;
        Ok(ModelStream::from_stream(
            stream,
            self.invoke_context.cancel_token.clone().unwrap_or_default(),
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
    use std::time::Duration;

    use astrcode_core::llm::{LlmEvent, LlmMessage, LlmProvider, ModelLimits};
    use astrcode_extension_contract::WireErrorCode;

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
                    tool_call_id: Some("call-1".into()),
                    working_dir: Some(PathBuf::from("/workspace")),
                    session_store_dir: Some(session_store_dir.clone()),
                    event_tx: None,
                    event_causation: None,
                    cancellation: cancellation.clone(),
                },
            );

        assert_eq!(context.extension_id(), "review-extension");
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
        assert_eq!(error.code_enum(), Some(WireErrorCode::BackendUnavailable));
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
        let input = serde_json::to_value(astrcode_extension_sdk::host::internal::llm_chat_request(
            vec![LlmMessage::user("hello")],
        ))
        .unwrap();

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
