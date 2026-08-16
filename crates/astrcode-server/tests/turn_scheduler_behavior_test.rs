//! Session / Turn 行为矩阵回归测试（Phase 0）。

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use astrcode_core::{
    config::{
        EffectiveConfig, ExtensionSettings, LlmSettings, ProviderAuthScheme, ProviderWireFormat,
    },
    event::{DurableEvent, DurableEventPayload, StoredEvent},
    llm::{LlmContent, LlmError, LlmEvent, LlmProvider, LlmRole, ModelLimits},
    types::{SessionId, ToolCallId, new_message_id, new_session_id, new_turn_id},
};
use astrcode_extension_sdk::{
    builder::manifest,
    extension::{
        Extension, ExtensionCapability, ExtensionError, ExtensionManifest, Registrar,
        UserMessageEnvelopeContext, UserMessageEnvelopeHandler, UserMessageEnvelopeResult,
    },
};
use astrcode_extensions::{runner::ExtensionRunner, testing::extension_runner_with_extensions};
use astrcode_server::test_support::{
    ChildSessionCoordinator, DeliveryOutcome, InputDelivery, MAX_PENDING_INPUTS_PER_SESSION,
    MAX_PROMPT_TEXT_BYTES, SessionManager, StartedExecution, TurnRegistry, TurnScheduleError,
    TurnScheduler, recycle_completed_session_for_test, release_completed_execution_for_test,
    session_started_event_for_test, start_with_completion_for_test,
};
use astrcode_storage::{SessionStore, in_memory::InMemoryEventStore};
use tokio::sync::{Semaphore, mpsc};

#[async_trait::async_trait]
trait UntrackedStartForTest {
    async fn start_with_completion(
        &self,
        session_id: SessionId,
        input: astrcode_core::user_input::UserInput,
    ) -> Result<StartedExecution, TurnScheduleError>;
}

#[async_trait::async_trait]
impl UntrackedStartForTest for TurnScheduler {
    async fn start_with_completion(
        &self,
        session_id: SessionId,
        input: astrcode_core::user_input::UserInput,
    ) -> Result<StartedExecution, TurnScheduleError> {
        start_with_completion_for_test(self, session_id, input).await
    }
}

struct StaticTextLlm;
struct PendingLlm;
struct GateFirstLlm {
    calls: Arc<AtomicUsize>,
    release: Arc<Semaphore>,
}
struct FailSecondEnvelope {
    calls: Arc<AtomicUsize>,
}
struct FailSecondEnvelopeHandler {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl LlmProvider for StaticTextLlm {
    async fn generate_request(
        &self,
        _request: astrcode_core::llm::LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::ContentDelta { delta: "ok".into() });
        let _ = tx.send(LlmEvent::Done {
            finish_reason: "stop".into(),
        });
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 200000,
            max_output_tokens: 1024,
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for PendingLlm {
    async fn generate_request(
        &self,
        _request: astrcode_core::llm::LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        std::future::pending().await
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 200000,
            max_output_tokens: 1024,
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for GateFirstLlm {
    async fn generate_request(
        &self,
        _request: astrcode_core::llm::LlmRequest,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        if self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
            self.release.acquire().await.unwrap().forget();
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::Done {
            finish_reason: "stop".into(),
        });
        Ok(rx)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 200000,
            max_output_tokens: 1024,
        }
    }
}

#[async_trait::async_trait]
impl Extension for FailSecondEnvelope {
    fn manifest(&self) -> ExtensionManifest {
        manifest("fail-second-envelope")
            .version("test")
            .description("Turn scheduler envelope failure test extension")
            .capability(ExtensionCapability::ProviderRequest)
            .build()
    }

    fn register(&self, registrar: &mut Registrar) {
        registrar.on_user_message_envelope(
            0,
            Arc::new(FailSecondEnvelopeHandler {
                calls: Arc::clone(&self.calls),
            }),
        );
    }
}

#[async_trait::async_trait]
impl UserMessageEnvelopeHandler for FailSecondEnvelopeHandler {
    async fn handle(
        &self,
        _ctx: UserMessageEnvelopeContext,
    ) -> Result<UserMessageEnvelopeResult, ExtensionError> {
        if self.calls.fetch_add(1, Ordering::AcqRel) == 1 {
            return Ok(UserMessageEnvelopeResult::Block {
                reason: "transient test failure".into(),
            });
        }
        Ok(UserMessageEnvelopeResult::Allow)
    }
}

async fn seed_session(store: &Arc<dyn SessionStore>) -> SessionId {
    let sid = new_session_id();
    store
        .create_session(session_started_event_for_test(sid.clone(), ".", "mock"))
        .await
        .unwrap();
    sid
}

fn build_scheduler(store: Arc<dyn SessionStore>) -> TurnScheduler {
    build_scheduler_with_llm(store, Arc::new(StaticTextLlm))
}

fn build_scheduler_with_llm(
    store: Arc<dyn SessionStore>,
    llm: Arc<dyn LlmProvider>,
) -> TurnScheduler {
    let extension_runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
    build_scheduler_with_runtime(store, llm, extension_runner)
}

fn build_scheduler_with_runtime(
    store: Arc<dyn SessionStore>,
    llm: Arc<dyn LlmProvider>,
    extension_runner: Arc<ExtensionRunner>,
) -> TurnScheduler {
    let effective = EffectiveConfig {
        llm: LlmSettings {
            provider_kind: "mock".into(),
            base_url: String::new(),
            api_key: String::new(),
            wire_format: ProviderWireFormat::OpenAiChatCompletions,
            auth_scheme: ProviderAuthScheme::Bearer,
            model_id: "mock".into(),
            max_tokens: 1024,
            context_limit: 1024,
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            max_retries: 0,
            retry_base_delay_ms: 0,
            supports_prompt_cache_key: false,
            supports_stream_usage: false,
            supports_strict_tool_use: false,
            prompt_cache_retention: None,
            thinking: Default::default(),
            thinking_capability: None,
            thinking_configured: false,
        },
        small_llm: LlmSettings {
            provider_kind: "mock".into(),
            base_url: String::new(),
            api_key: String::new(),
            wire_format: ProviderWireFormat::OpenAiChatCompletions,
            auth_scheme: ProviderAuthScheme::Bearer,
            model_id: "mock".into(),
            max_tokens: 1024,
            context_limit: 1024,
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            max_retries: 0,
            retry_base_delay_ms: 0,
            supports_prompt_cache_key: false,
            supports_stream_usage: false,
            supports_strict_tool_use: false,
            prompt_cache_retention: None,
            thinking: Default::default(),
            thinking_capability: None,
            thinking_configured: false,
        },
        context: Default::default(),
        agent: Default::default(),
        permissions: Default::default(),
        extensions: ExtensionSettings::default(),
    };
    let capabilities = astrcode_server::test_support::assemble_session_runtime_services_for_test(
        Arc::clone(&llm),
        llm,
        effective,
        extension_runner.clone(),
    );
    let session_manager = Arc::new(SessionManager::new(
        Arc::clone(&store),
        capabilities,
        vec![],
    ));
    let child_sessions = Arc::new(ChildSessionCoordinator::new(Arc::clone(&session_manager)));
    let scheduler = Arc::new(TurnScheduler::new(
        Arc::clone(&session_manager),
        Arc::new(TurnRegistry::new()),
        Arc::clone(&child_sessions),
    ));
    child_sessions.spawn_completion_watcher(Arc::clone(&scheduler));
    scheduler.as_ref().clone()
}

#[tokio::test]
async fn idle_submit_emits_turn_started_and_user_message() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler(Arc::clone(&store));
    let sid = seed_session(&store).await;

    let started = scheduler
        .start_with_completion(sid.clone(), "hello".into())
        .await
        .unwrap();
    let result = started.handle.wait().await.unwrap();
    scheduler
        .finish_and_maybe_start_next(&sid, &started.turn_id, Some(&result.finalization))
        .await
        .unwrap();

    let events = store.replay_events(&sid).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e.payload, DurableEventPayload::TurnStarted))
    );
    assert!(events.iter().any(|e| matches!(
        &e.payload,
        DurableEventPayload::UserMessage { text, .. } if text == "hello"
    )));
}

#[tokio::test]
async fn running_submit_returns_busy() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler(Arc::clone(&store));
    let sid = seed_session(&store).await;

    let _started = scheduler
        .start_with_completion(sid.clone(), "first".into())
        .await
        .unwrap();
    let err = scheduler
        .deliver_input(sid, "second".into(), InputDelivery::StartNew)
        .await
        .unwrap_err();
    assert!(matches!(err, TurnScheduleError::TurnAlreadyRunning));
}

#[tokio::test]
async fn concurrent_start_with_completion_accepts_only_one_turn() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler_with_llm(Arc::clone(&store), Arc::new(PendingLlm));
    let sid = seed_session(&store).await;

    let first_scheduler = scheduler.clone();
    let first_sid = sid.clone();
    let first = tokio::spawn(async move {
        first_scheduler
            .start_with_completion(first_sid, "first".into())
            .await
    });
    let second_scheduler = scheduler.clone();
    let second_sid = sid.clone();
    let second = tokio::spawn(async move {
        second_scheduler
            .start_with_completion(second_sid, "second".into())
            .await
    });

    let outcomes = [first.await.unwrap(), second.await.unwrap()];
    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|result| {
                matches!(result.as_ref(), Err(TurnScheduleError::TurnAlreadyRunning))
            })
            .count(),
        1
    );

    let events = store.replay_events(&sid).await.unwrap();
    let user_messages = events
        .iter()
        .filter(|event| matches!(event.payload, DurableEventPayload::UserMessage { .. }))
        .count();
    assert_eq!(user_messages, 1);
}

#[tokio::test]
async fn running_inject_is_accepted_for_the_active_turn_then_absorbed() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let release = Arc::new(Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let scheduler = build_scheduler_with_llm(
        Arc::clone(&store),
        Arc::new(GateFirstLlm {
            calls: Arc::clone(&calls),
            release: Arc::clone(&release),
        }),
    );
    let sid = seed_session(&store).await;

    let started = scheduler
        .start_with_completion(sid.clone(), "first".into())
        .await
        .unwrap();
    let turn_id = started.turn_id.clone();
    // 等 turn 进入首个 provider 调用：step 0 的吸收点已过，inject 必然落在 step 内部。
    for _ in 0..100 {
        if calls.load(Ordering::Acquire) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(calls.load(Ordering::Acquire) > 0, "turn must be active");

    let outcome = scheduler
        .deliver_input(
            sid.clone(),
            "inject me".into(),
            InputDelivery::InjectIfRunningElseStart,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome,
        DeliveryOutcome::Injected {
            turn_id: turn_id.clone()
        }
    );

    // 接受 ≠ 进入 transcript：只落归属活跃 turn 的 UserInputAccepted。
    let events = store.replay_events(&sid).await.unwrap();
    let accepted = events
        .iter()
        .find(|event| {
            matches!(
                &event.payload,
                DurableEventPayload::UserInputAccepted { input } if input.text == "inject me"
            )
        })
        .expect("inject must durably accept the input");
    assert_eq!(accepted.turn_id.as_ref(), Some(&turn_id));
    assert!(
        !events.iter().any(|event| matches!(
            &event.payload,
            DurableEventPayload::UserMessage { text, .. } if text == "inject me"
        )),
        "accepted input must not enter the transcript before the next step boundary"
    );

    release.add_permits(1);
    let result = started.handle.wait().await.unwrap();
    assert!(result.output.is_ok(), "{:?}", result.output);

    // step 边界吸收：UserMessage 归属同一 turn 并按 accepted_seq 回链。
    let events = store.replay_events(&sid).await.unwrap();
    let absorbed = events
        .iter()
        .find(|event| {
            matches!(
                &event.payload,
                DurableEventPayload::UserMessage { text, .. } if text == "inject me"
            )
        })
        .expect("absorbed UserMessage must be durable after the turn continues");
    assert_eq!(absorbed.turn_id.as_ref(), Some(&turn_id));
    let DurableEventPayload::UserMessage { accepted_seq, .. } = &absorbed.payload else {
        unreachable!()
    };
    assert_eq!(*accepted_seq, Some(accepted.seq));
    assert!(
        store
            .session_read_model(&sid)
            .await
            .unwrap()
            .execution
            .pending_inputs
            .is_empty()
    );
}

#[tokio::test]
async fn running_queue_does_not_start_second_turn() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler(Arc::clone(&store));
    let sid = seed_session(&store).await;

    let _started = scheduler
        .start_with_completion(sid.clone(), "first".into())
        .await
        .unwrap();
    let outcome = scheduler
        .deliver_input(
            sid.clone(),
            "queued".into(),
            InputDelivery::QueueIfRunningElseStart,
        )
        .await
        .unwrap();
    assert!(matches!(outcome, DeliveryOutcome::Queued { queue_len: 1 }));
    assert!(scheduler.registry().has_active(&sid));
}

#[tokio::test]
async fn durable_queue_recovers_fifo_after_scheduler_restart() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler_with_llm(Arc::clone(&store), Arc::new(PendingLlm));
    let sid = seed_session(&store).await;

    let first = scheduler
        .start_with_completion(sid.clone(), "first".into())
        .await
        .unwrap();
    let first_turn_id = first.turn_id.clone();
    for text in ["queued one", "queued two"] {
        scheduler
            .deliver_input(
                sid.clone(),
                text.into(),
                InputDelivery::QueueIfRunningElseStart,
            )
            .await
            .unwrap();
    }

    let queued = store.session_read_model(&sid).await.unwrap();
    assert_eq!(queued.execution.pending_inputs.len(), 2);
    assert_eq!(queued.model_context.messages.len(), 1);

    first.handle.force_kill();
    drop(first.handle);
    drop(scheduler);

    let restarted = build_scheduler(Arc::clone(&store));
    restarted.repair_stale(&sid).await.unwrap();
    for _ in 0..100 {
        let state = store.session_read_model(&sid).await.unwrap();
        if !restarted.registry().has_active(&sid) && state.execution.pending_inputs.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let state = store.session_read_model(&sid).await.unwrap();
    assert!(state.execution.pending_inputs.is_empty());
    let events = store.replay_events(&sid).await.unwrap();
    let user_messages = events
        .iter()
        .filter_map(|event| match &event.payload {
            DurableEventPayload::UserMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(user_messages, ["first", "queued one", "queued two"]);

    let first_turn_attempts = events
        .iter()
        .filter_map(|event| match &event.payload {
            DurableEventPayload::StepStarted {
                step_index,
                attempt,
            } if event.turn_id.as_ref() == Some(&first_turn_id) => Some((*step_index, *attempt)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(first_turn_attempts, [(0, 1), (0, 2)]);
    assert!(events.iter().any(|event| {
        event.turn_id.as_ref() == Some(&first_turn_id)
            && matches!(
                &event.payload,
                DurableEventPayload::TurnCompleted { finish_reason } if finish_reason == "stop"
            )
    }));

    let final_sid = seed_session(&store).await;
    let final_turn_id = new_turn_id();
    for payload in [
        DurableEventPayload::TurnStarted,
        DurableEventPayload::UserMessage {
            message_id: new_message_id(),
            text: "already answered".into(),
            attachments: Vec::new(),
            accepted_seq: None,
        },
        DurableEventPayload::StepStarted {
            step_index: 0,
            attempt: 1,
        },
        DurableEventPayload::AssistantMessageCompleted {
            message_id: new_message_id(),
            text: "done".into(),
            reasoning_content: None,
        },
        DurableEventPayload::StepCompleted {
            step_index: 0,
            attempt: 1,
            finish_reason: Some("stop".into()),
        },
    ] {
        store
            .append_event(DurableEvent::new(
                final_sid.clone(),
                Some(final_turn_id.clone()),
                payload,
            ))
            .await
            .unwrap();
    }
    restarted.repair_stale(&final_sid).await.unwrap();
    let final_events = store.replay_events(&final_sid).await.unwrap();
    assert_eq!(
        final_events
            .iter()
            .filter(|event| matches!(event.payload, DurableEventPayload::StepStarted { .. }))
            .count(),
        1,
        "a completed final step must not call the provider again"
    );
    assert!(final_events.iter().any(|event| {
        matches!(
            &event.payload,
            DurableEventPayload::TurnCompleted { finish_reason } if finish_reason == "stop"
        )
    }));
}

#[tokio::test]
async fn queued_input_retries_after_a_transient_start_failure() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let release = Arc::new(Semaphore::new(0));
    let envelope_calls = Arc::new(AtomicUsize::new(0));
    let extension_runner = extension_runner_with_extensions(
        Duration::from_secs(1),
        None,
        vec![Arc::new(FailSecondEnvelope {
            calls: Arc::clone(&envelope_calls),
        })],
    )
    .await
    .unwrap();
    let scheduler = build_scheduler_with_runtime(
        Arc::clone(&store),
        Arc::new(GateFirstLlm {
            calls: Arc::new(AtomicUsize::new(0)),
            release: Arc::clone(&release),
        }),
        extension_runner,
    );
    let sid = seed_session(&store).await;

    scheduler
        .deliver_input(sid.clone(), "first".into(), InputDelivery::StartNew)
        .await
        .unwrap();
    scheduler
        .deliver_input(
            sid.clone(),
            "queued".into(),
            InputDelivery::QueueIfRunningElseStart,
        )
        .await
        .unwrap();
    release.add_permits(1);

    for _ in 0..100 {
        let state = store.session_read_model(&sid).await.unwrap();
        if !scheduler.registry().has_active(&sid) && state.execution.pending_inputs.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let state = store.session_read_model(&sid).await.unwrap();
    assert!(state.execution.pending_inputs.is_empty());
    assert_eq!(
        state
            .model_context
            .messages
            .iter()
            .filter(|message| message.message.role == astrcode_core::llm::LlmRole::User)
            .filter_map(|message| match message.message.content.as_slice() {
                [astrcode_core::llm::LlmContent::Text { text }] => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        ["first", "queued"]
    );
    assert_eq!(envelope_calls.load(Ordering::Acquire), 3);
}

#[tokio::test]
async fn completion_handoff_never_overtakes_an_older_queued_input() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let release = Arc::new(Semaphore::new(0));
    let scheduler = build_scheduler_with_llm(
        Arc::clone(&store),
        Arc::new(GateFirstLlm {
            calls: Arc::new(AtomicUsize::new(0)),
            release: Arc::clone(&release),
        }),
    );
    let sid = seed_session(&store).await;

    scheduler
        .deliver_input(sid.clone(), "first".into(), InputDelivery::StartNew)
        .await
        .unwrap();
    scheduler
        .deliver_input(
            sid.clone(),
            "older queued".into(),
            InputDelivery::QueueIfRunningElseStart,
        )
        .await
        .unwrap();

    release.add_permits(1);
    scheduler
        .deliver_input(
            sid.clone(),
            "new arrival".into(),
            InputDelivery::QueueIfRunningElseStart,
        )
        .await
        .unwrap();

    for _ in 0..100 {
        let state = store.session_read_model(&sid).await.unwrap();
        if !scheduler.registry().has_active(&sid) && state.execution.pending_inputs.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let user_messages = store
        .replay_events(&sid)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|event| match &event.payload {
            DurableEventPayload::UserMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(user_messages, ["first", "older queued", "new arrival"]);
}

#[tokio::test]
async fn running_queue_rejects_when_pending_limit_is_reached() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler_with_llm(Arc::clone(&store), Arc::new(PendingLlm));
    let sid = seed_session(&store).await;

    let _started = scheduler
        .start_with_completion(sid.clone(), "first".into())
        .await
        .unwrap();
    for index in 0..MAX_PENDING_INPUTS_PER_SESSION {
        let outcome = scheduler
            .deliver_input(
                sid.clone(),
                format!("queued {index}").into(),
                InputDelivery::QueueIfRunningElseStart,
            )
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            DeliveryOutcome::Queued { queue_len } if queue_len == index + 1
        ));
    }

    let err = scheduler
        .deliver_input(
            sid,
            "too many".into(),
            InputDelivery::QueueIfRunningElseStart,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        TurnScheduleError::QueueFull {
            max: MAX_PENDING_INPUTS_PER_SESSION
        }
    ));
}

#[tokio::test]
async fn oversized_prompt_is_rejected_before_turn_starts() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler(Arc::clone(&store));
    let sid = seed_session(&store).await;

    let text = "x".repeat(MAX_PROMPT_TEXT_BYTES + 1);
    let result = scheduler
        .start_with_completion(sid.clone(), text.into())
        .await;
    let err = match result {
        Ok(_) => panic!("oversized prompt should be rejected"),
        Err(err) => err,
    };
    assert!(matches!(
        err,
        TurnScheduleError::InputTooLarge {
            actual,
            max: MAX_PROMPT_TEXT_BYTES
        } if actual == MAX_PROMPT_TEXT_BYTES + 1
    ));

    let events = store.replay_events(&sid).await.unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.payload, DurableEventPayload::TurnStarted))
    );
}

fn turn_completed_reasons(events: &[StoredEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.payload {
            DurableEventPayload::TurnCompleted { finish_reason } => Some(finish_reason.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn release_completed_execution_is_non_destructive() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler(Arc::clone(&store));
    let sid = seed_session(&store).await;

    let started = scheduler
        .start_with_completion(sid.clone(), "done".into())
        .await
        .unwrap();
    let turn_id = started.turn_id;
    let result = started.handle.wait().await.unwrap();

    release_completed_execution_for_test(&scheduler, &sid, &turn_id, Some(&result.finalization))
        .await;

    assert_eq!(
        turn_completed_reasons(&store.replay_events(&sid).await.unwrap()),
        vec!["stop"]
    );
    assert!(!scheduler.registry().has_active(&sid));
}

#[tokio::test]
async fn stale_completion_does_not_recycle_newer_turn() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler(Arc::clone(&store));
    let sid = seed_session(&store).await;

    let first = scheduler
        .start_with_completion(sid.clone(), "first".into())
        .await
        .unwrap();
    let first_turn_id = first.turn_id;
    let first_result = first.handle.wait().await.unwrap();
    scheduler
        .finish_and_maybe_start_next(&sid, &first_turn_id, Some(&first_result.finalization))
        .await
        .unwrap();

    let second = scheduler
        .start_with_completion(sid.clone(), "second".into())
        .await
        .unwrap();

    assert!(
        !recycle_completed_session_for_test(&scheduler, &sid, &first_turn_id)
            .await
            .unwrap()
    );
    assert!(store.list_sessions().await.unwrap().contains(&sid));

    let second_result = second.handle.wait().await.unwrap();
    scheduler
        .finish_and_maybe_start_next(&sid, &second.turn_id, Some(&second_result.finalization))
        .await
        .unwrap();
}

#[tokio::test]
async fn cleanup_after_finished_registry_entry_does_not_emit_duplicate_terminal() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler(Arc::clone(&store));
    let sid = seed_session(&store).await;

    let started = scheduler
        .start_with_completion(sid.clone(), "done".into())
        .await
        .unwrap();
    let _ = started.handle.wait().await;

    scheduler.cleanup_execution(&sid).await;

    let reasons = turn_completed_reasons(&store.replay_events(&sid).await.unwrap());
    assert_eq!(
        reasons,
        vec!["stop"],
        "cleanup of a finished registry entry must not append a second terminal event"
    );
}

#[tokio::test]
async fn execution_view_uses_registry_for_active_turn() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler(Arc::clone(&store));
    let sid = seed_session(&store).await;

    let started = scheduler
        .start_with_completion(sid.clone(), "run".into())
        .await
        .unwrap();
    let turn_id = started.turn_id;
    let view = scheduler.execution_view(&sid).await.unwrap();
    assert_eq!(view.active_turn_id, Some(turn_id));
}

#[tokio::test]
async fn abort_requests_cooperative_cancel_and_registry_waits_for_runner_finish() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler_with_llm(Arc::clone(&store), Arc::new(PendingLlm));
    let sid = seed_session(&store).await;

    let started = scheduler
        .start_with_completion(sid.clone(), "run".into())
        .await
        .unwrap();

    assert!(scheduler.abort(&sid).await.unwrap());
    assert!(
        !scheduler.registry().has_active(&sid),
        "abort must not return before durable finalization releases ownership"
    );

    let result = started.handle.wait().await.expect("turn result");
    assert!(matches!(
        result.output,
        Err(astrcode_session::TurnError::Aborted)
    ));

    let reasons = turn_completed_reasons(&store.replay_events(&sid).await.unwrap());
    assert_eq!(reasons, vec!["aborted"]);
    assert!(!scheduler.abort(&sid).await.unwrap());
}

#[tokio::test]
async fn owned_task_tracking_releases_finished_tasks() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler(Arc::clone(&store));
    let sid = seed_session(&store).await;

    scheduler
        .deliver_input(sid.clone(), "first".into(), InputDelivery::StartNew)
        .await
        .unwrap();
    for _ in 0..50 {
        if scheduler.owned_task_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        scheduler.owned_task_count(),
        1,
        "only the persistent child completion watcher should remain"
    );

    scheduler
        .deliver_input(sid, "second".into(), InputDelivery::StartNew)
        .await
        .unwrap();
    for _ in 0..50 {
        if scheduler.owned_task_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(scheduler.owned_task_count(), 1);
}

#[tokio::test]
async fn shutdown_rejects_new_turns_and_durably_settles_active_owners() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = Arc::new(build_scheduler_with_llm(
        Arc::clone(&store),
        Arc::new(PendingLlm),
    ));
    let active_session = seed_session(&store).await;
    let rejected_session = seed_session(&store).await;

    scheduler
        .deliver_input(
            active_session.clone(),
            "active".into(),
            InputDelivery::StartNew,
        )
        .await
        .unwrap();

    let shutdown_scheduler = Arc::clone(&scheduler);
    let shutdown = tokio::spawn(async move {
        shutdown_scheduler.shutdown_background_tasks().await;
    });
    while scheduler.accepts_owned_tasks() {
        tokio::task::yield_now().await;
    }

    assert!(matches!(
        scheduler
            .deliver_input(rejected_session, "too late".into(), InputDelivery::StartNew,)
            .await,
        Err(TurnScheduleError::BackgroundTasksClosed)
    ));
    tokio::time::timeout(Duration::from_secs(5), shutdown)
        .await
        .expect("shutdown should force and settle the pending turn")
        .unwrap();

    assert!(!scheduler.registry().has_active(&active_session));
    assert_eq!(
        turn_completed_reasons(&store.replay_events(&active_session).await.unwrap()),
        vec!["aborted"]
    );
    assert_eq!(scheduler.owned_task_count(), 0);
}

#[tokio::test]
async fn interrupt_and_start_replaces_active_turn_under_delivery_gate() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler_with_llm(Arc::clone(&store), Arc::new(PendingLlm));
    let sid = seed_session(&store).await;

    let first = scheduler
        .deliver_input(sid.clone(), "first".into(), InputDelivery::StartNew)
        .await
        .unwrap();
    let DeliveryOutcome::Started {
        turn_id: first_turn,
    } = first
    else {
        panic!("first input must start a turn");
    };

    let replacement = tokio::time::timeout(
        Duration::from_secs(3),
        scheduler.deliver_input(
            sid.clone(),
            "replacement".into(),
            InputDelivery::InterruptAndStart,
        ),
    )
    .await
    .expect("interrupt must not deadlock")
    .unwrap();
    let DeliveryOutcome::Started {
        turn_id: replacement_turn,
    } = replacement
    else {
        panic!("replacement input must start a turn");
    };
    assert_ne!(first_turn, replacement_turn);
    assert_eq!(
        scheduler.registry().active_turn_id(&sid),
        Some(replacement_turn)
    );

    scheduler.abort(&sid).await.unwrap();
}

#[tokio::test]
async fn queue_drains_turn_finished_but_not_settled() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler(Arc::clone(&store));
    let sid = seed_session(&store).await;

    // 启动第一个 turn 并等它 durable 完成，但**不**执行完成收尾：
    // 这精确复现“回复已完成、registry 尚未 settle”的窗口（真实场景中完成
    // watcher 可能还卡在 sync/child-drain 或收尾重试中）。
    let started = scheduler
        .start_with_completion(sid.clone(), "first".into())
        .await
        .unwrap();
    started.handle.wait().await.expect("first turn must finish");
    assert!(scheduler.registry().active_is_finished(&sid));
    assert!(scheduler.registry().has_active(&sid));
    assert!(scheduler.registry().active_is_finished(&sid));
    assert!(scheduler.registry().has_active(&sid));

    // 窗口内投递：投递路径应自行收尾 finished turn 并立即启动队首，
    // 而不是把输入留在队列里依赖（可能永远不来的）watcher drain。
    let outcome = scheduler
        .deliver_input(
            sid.clone(),
            "second".into(),
            InputDelivery::QueueIfRunningElseStart,
        )
        .await
        .expect("deliver must not fail");
    assert!(matches!(outcome, DeliveryOutcome::Queued { .. }));

    // 第二个 turn 被立即启动（StaticTextLlm 立即完成），随后被投递路径
    // spawn 的 watcher 收尾；最终无活跃 turn、无 pending 输入。
    for _ in 0..100 {
        if !scheduler.registry().has_active(&sid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !scheduler.registry().has_active(&sid),
        "second turn must be started and settled, not stuck in queue"
    );
    let model = store.session_read_model(&sid).await.unwrap();
    assert!(
        model.execution.pending_inputs.is_empty(),
        "queued input must be drained into a new turn"
    );
    let events = store.replay_events(&sid).await.unwrap();
    let user_messages = events
        .iter()
        .filter(|e| matches!(e.payload, DurableEventPayload::UserMessage { .. }))
        .count();
    assert_eq!(
        user_messages, 2,
        "second input must be consumed into a new turn"
    );
}

#[tokio::test]
async fn repair_stale_repairs_trailing_unanswered_tool_call() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler(Arc::clone(&store));
    let sid = seed_session(&store).await;
    let turn_id = new_turn_id();

    // 崩溃尾部:tool call 已请求但终结事件未落盘,turn 已被终态事件收口(phase 回到 Idle)。
    for payload in [
        DurableEventPayload::TurnStarted,
        DurableEventPayload::UserMessage {
            message_id: new_message_id(),
            text: "do something".into(),
            attachments: Vec::new(),
            accepted_seq: None,
        },
        DurableEventPayload::AssistantMessageCompleted {
            message_id: new_message_id(),
            text: String::new(),
            reasoning_content: None,
        },
        DurableEventPayload::ToolCallRequested {
            call_id: ToolCallId::new("call-orphan"),
            tool_name: "read".into(),
            arguments: serde_json::json!({}),
            raw_arguments: None,
        },
        DurableEventPayload::TurnCompleted {
            finish_reason: "interrupted".into(),
        },
    ] {
        store
            .append_event(DurableEvent::new(
                sid.clone(),
                Some(turn_id.clone()),
                payload,
            ))
            .await
            .unwrap();
    }

    scheduler.repair_stale(&sid).await.unwrap();

    let events = store.replay_events(&sid).await.unwrap();
    assert!(
        events.iter().any(|event| matches!(
            &event.payload,
            DurableEventPayload::ToolCallFailed { call_id, .. } if call_id.as_str() == "call-orphan"
        )),
        "repair must append a durable ToolCallFailed for the trailing unanswered call"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event.payload, DurableEventPayload::TurnAbortedContext)),
        "repair must append TurnAbortedContext alongside the failure"
    );

    // provider 可见上下文恢复合法:assistant tool_calls 后紧跟其 tool 结果。
    let model = store.session_read_model(&sid).await.unwrap();
    let messages = astrcode_core::llm::provider_visible_messages(
        model
            .model_context
            .messages
            .iter()
            .map(|message| (*message.message).clone())
            .collect(),
    );
    let assistant_pos = messages
        .iter()
        .position(|message| {
            message.role == LlmRole::Assistant
                && message.content.iter().any(|content| {
                    matches!(content, LlmContent::ToolCall { call_id, .. } if call_id == "call-orphan")
                })
        })
        .expect("assistant tool call must remain visible");
    assert!(
        matches!(
            messages.get(assistant_pos + 1),
            Some(message) if message.role == LlmRole::Tool
                && message.content.iter().any(|content| matches!(
                    content,
                    LlmContent::ToolResult { tool_call_id, .. } if tool_call_id == "call-orphan"
                ))
        ),
        "tool result must immediately follow the assistant tool call after repair: {messages:?}"
    );
}
