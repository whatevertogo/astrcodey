use std::{
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use astrcode_core::{
    event::DurableEventPayload,
    llm::{LlmError, LlmEvent, LlmProvider, LlmRequest, LlmTokenUsage, ModelLimits},
    types::{new_session_id, new_turn_id},
};
use astrcode_extension_sdk::{
    extension::{
        ExtensionError, PreparedProviderContribution, ProviderContext, ProviderContributionHandler,
        ProviderContributionId, ProviderResult, ProviderSettlementContext,
    },
    runtime_ports::{
        NoopRuntimePorts, ProviderRequestAcknowledgements, ProviderRequestPreparation, TurnHooks,
    },
};
use astrcode_session::{Session, SessionCreateParams, SessionExtensionPorts, SessionRuntimeState};
use astrcode_storage::{EventReader, SessionStore, in_memory::InMemoryEventStore};
use tokio::sync::{Mutex, Notify, mpsc};

mod common;

struct InertContributionHandler;

#[async_trait::async_trait]
impl ProviderContributionHandler for InertContributionHandler {
    async fn prepare(
        &self,
        _ctx: ProviderContext,
    ) -> Result<Option<PreparedProviderContribution>, ExtensionError> {
        unreachable!("the test runtime prepares its opaque acknowledgement directly")
    }

    async fn acknowledge(&self, _ctx: ProviderSettlementContext) -> Result<(), ExtensionError> {
        unreachable!("the test runtime observes settlement at the runtime port")
    }
}

struct RecordingHooks {
    store: Arc<InMemoryEventStore>,
    original_handler: Arc<dyn ProviderContributionHandler>,
    current_handler: RwLock<Arc<dyn ProviderContributionHandler>>,
    prepares: AtomicUsize,
    acknowledgements: AtomicUsize,
    acknowledged_after_durable_facts: AtomicBool,
    acknowledged_original_handler: AtomicBool,
}

impl RecordingHooks {
    fn new(store: Arc<InMemoryEventStore>) -> Arc<Self> {
        let original_handler: Arc<dyn ProviderContributionHandler> =
            Arc::new(InertContributionHandler);
        Arc::new(Self {
            store,
            original_handler: Arc::clone(&original_handler),
            current_handler: RwLock::new(original_handler),
            prepares: AtomicUsize::new(0),
            acknowledgements: AtomicUsize::new(0),
            acknowledged_after_durable_facts: AtomicBool::new(false),
            acknowledged_original_handler: AtomicBool::new(false),
        })
    }

    fn reload_handler(&self) {
        *self.current_handler.write().unwrap() = Arc::new(InertContributionHandler);
    }
}

#[async_trait::async_trait]
impl TurnHooks for RecordingHooks {
    async fn prepare_provider_request(
        &self,
        _ctx: astrcode_extension_sdk::extension::internal::RuntimeProviderContext,
    ) -> Result<ProviderRequestPreparation, ExtensionError> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        let mut acknowledgements = ProviderRequestAcknowledgements::default();
        acknowledgements.push_runtime(
            "test-contribution".into(),
            self.current_handler.read().unwrap().clone(),
            ProviderContributionId::new("pending-1"),
        );
        Ok(ProviderRequestPreparation::from_runtime(
            ProviderResult::Allow,
            acknowledgements,
        ))
    }

    async fn acknowledge_provider_request(
        &self,
        ctx: astrcode_extension_sdk::extension::internal::RuntimeProviderSettlementContext,
        acknowledgements: ProviderRequestAcknowledgements,
    ) -> Result<(), ExtensionError> {
        let entries = acknowledgements.into_runtime_entries().collect::<Vec<_>>();
        self.acknowledged_original_handler.store(
            entries.len() == 1 && Arc::ptr_eq(&entries[0].1, &self.original_handler),
            Ordering::SeqCst,
        );

        let events = self
            .store
            .replay_events(ctx.call().session_id())
            .await
            .map_err(|error| ExtensionError::Internal(error.to_string()))?;
        let assistant = events.iter().rposition(|event| {
            matches!(
                event.payload,
                DurableEventPayload::AssistantMessageCompleted { .. }
            )
        });
        let usage = events.iter().rposition(|event| {
            matches!(
                event.payload,
                DurableEventPayload::TokenUsageRecorded { .. }
            )
        });
        self.acknowledged_after_durable_facts.store(
            self.store.sync_count() > 0
                && matches!((assistant, usage), (Some(assistant), Some(usage)) if assistant < usage),
            Ordering::SeqCst,
        );
        self.acknowledgements.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct FailThenSucceedLlm {
    calls: AtomicUsize,
    hooks: Arc<RecordingHooks>,
}

#[async_trait::async_trait]
impl LlmProvider for FailThenSucceedLlm {
    async fn generate_request(
        &self,
        _request: LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(LlmError::ClientError {
                status: 503,
                message: "retry later".into(),
            });
        }
        self.hooks.reload_handler();
        Ok(successful_stream())
    }

    fn model_limits(&self) -> ModelLimits {
        test_model_limits()
    }
}

struct SuccessfulLlm;

#[async_trait::async_trait]
impl LlmProvider for SuccessfulLlm {
    async fn generate_request(
        &self,
        _request: LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        Ok(successful_stream())
    }

    fn model_limits(&self) -> ModelLimits {
        test_model_limits()
    }
}

struct OverflowLlm;

#[async_trait::async_trait]
impl LlmProvider for OverflowLlm {
    async fn generate_request(
        &self,
        _request: LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        Err(LlmError::ContextWindowExceeded {
            message: "injected overflow".into(),
        })
    }

    fn model_limits(&self) -> ModelLimits {
        test_model_limits()
    }
}

struct BlockingLlm {
    started: Notify,
    senders: Mutex<Vec<mpsc::UnboundedSender<LlmEvent>>>,
}

#[async_trait::async_trait]
impl LlmProvider for BlockingLlm {
    async fn generate_request(
        &self,
        _request: LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.senders.lock().await.push(tx);
        self.started.notify_one();
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        test_model_limits()
    }
}

fn successful_stream() -> mpsc::UnboundedReceiver<LlmEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    tx.send(LlmEvent::Usage {
        usage: LlmTokenUsage {
            input_tokens: Some(10),
            cached_input_tokens: None,
            cache_creation_input_tokens: None,
            input_accounting: None,
            output_tokens: Some(2),
            reasoning_output_tokens: None,
            total_tokens: Some(12),
            source: None,
        },
    })
    .unwrap();
    tx.send(LlmEvent::ContentDelta { delta: "ok".into() })
        .unwrap();
    tx.send(LlmEvent::Done {
        finish_reason: "stop".into(),
    })
    .unwrap();
    rx
}

fn test_model_limits() -> ModelLimits {
    ModelLimits {
        max_input_tokens: 200_000,
        max_output_tokens: 4096,
    }
}

async fn spawn_session(
    store: Arc<InMemoryEventStore>,
    llm: Arc<dyn LlmProvider>,
    hooks: Arc<RecordingHooks>,
) -> Session {
    let noop = Arc::new(NoopRuntimePorts);
    let extension_ports =
        SessionExtensionPorts::from_immutable_ports(noop.clone(), noop.clone(), hooks, noop);
    let services = common::test_runtime_services_with_extensions(llm, extension_ports);
    let session_id = new_session_id();
    let store_port: Arc<dyn SessionStore> = store;
    let runtime = Arc::new(SessionRuntimeState::new(session_id.clone(), store_port));
    let working_dir = std::env::temp_dir().join(session_id.as_str());
    std::fs::create_dir_all(&working_dir).unwrap();
    Session::create_with_params(SessionCreateParams {
        working_dir: working_dir.to_string_lossy().into_owned(),
        model_id: "mock-model".into(),
        parent_session_id: None,
        tool_selection: None,
        source_extension: None,
        extra_system_prompt: None,
        initial_system_prompt: None,
        runtime,
        runtime_services: services,
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn contribution_settlement_requires_durable_success_and_keeps_exact_handler_pairing() {
    let retry_store = Arc::new(InMemoryEventStore::new());
    let retry_hooks = RecordingHooks::new(Arc::clone(&retry_store));
    let retry_session = spawn_session(
        Arc::clone(&retry_store),
        Arc::new(FailThenSucceedLlm {
            calls: AtomicUsize::new(0),
            hooks: Arc::clone(&retry_hooks),
        }),
        Arc::clone(&retry_hooks),
    )
    .await;
    let failed = retry_session
        .submit("fail".into(), new_turn_id(), None)
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert!(failed.output.is_err());
    assert_eq!(retry_hooks.acknowledgements.load(Ordering::SeqCst), 0);
    let succeeded = retry_session
        .submit("retry".into(), new_turn_id(), None)
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert!(succeeded.output.is_ok(), "{:?}", succeeded.output);
    assert_eq!(retry_hooks.prepares.load(Ordering::SeqCst), 2);
    assert_eq!(retry_hooks.acknowledgements.load(Ordering::SeqCst), 1);
    assert!(
        retry_hooks
            .acknowledged_after_durable_facts
            .load(Ordering::SeqCst)
    );
    assert!(
        retry_hooks
            .acknowledged_original_handler
            .load(Ordering::SeqCst),
        "reload after prepare must not redirect settlement to the new handler"
    );
    assert_eq!(retry_store.sync_count(), 1);

    let sync_failure_store = Arc::new(InMemoryEventStore::new());
    let sync_failure_hooks = RecordingHooks::new(Arc::clone(&sync_failure_store));
    let sync_failure_session = spawn_session(
        Arc::clone(&sync_failure_store),
        Arc::new(SuccessfulLlm),
        Arc::clone(&sync_failure_hooks),
    )
    .await;
    sync_failure_store.fail_next_sync();
    let sync_failed = sync_failure_session
        .submit("sync fails".into(), new_turn_id(), None)
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert!(sync_failed.output.is_err());
    assert_eq!(
        sync_failure_hooks.acknowledgements.load(Ordering::SeqCst),
        0
    );

    let overflow_store = Arc::new(InMemoryEventStore::new());
    let overflow_hooks = RecordingHooks::new(Arc::clone(&overflow_store));
    let overflow_session = spawn_session(
        overflow_store,
        Arc::new(OverflowLlm),
        Arc::clone(&overflow_hooks),
    )
    .await;
    let overflow = tokio::time::timeout(
        Duration::from_secs(5),
        overflow_session
            .submit("overflow".into(), new_turn_id(), None)
            .await
            .unwrap()
            .wait(),
    )
    .await
    .expect("overflow recovery should terminate")
    .unwrap();
    assert!(overflow.output.is_err());
    assert_eq!(overflow_hooks.acknowledgements.load(Ordering::SeqCst), 0);

    let cancelled_store = Arc::new(InMemoryEventStore::new());
    let cancelled_hooks = RecordingHooks::new(Arc::clone(&cancelled_store));
    let blocking_llm = Arc::new(BlockingLlm {
        started: Notify::new(),
        senders: Mutex::new(Vec::new()),
    });
    let cancelled_session = spawn_session(
        cancelled_store,
        blocking_llm.clone(),
        Arc::clone(&cancelled_hooks),
    )
    .await;
    let started = blocking_llm.started.notified();
    let cancelled = cancelled_session
        .submit("cancel".into(), new_turn_id(), None)
        .await
        .unwrap();
    started.await;
    cancelled.request_shutdown();
    let cancelled = cancelled.wait().await.unwrap();
    assert!(cancelled.output.is_err());
    assert_eq!(cancelled_hooks.acknowledgements.load(Ordering::SeqCst), 0);
}
