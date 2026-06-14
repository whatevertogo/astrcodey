//! Session / Turn 行为矩阵回归测试（Phase 0）。

use std::{sync::Arc, time::Duration};

use astrcode_context::context_assembler::LlmContextAssembler;
use astrcode_core::{
    config::{EffectiveConfig, ExtensionSettings, LlmSettings, OpenAiApiMode},
    event::EventPayload,
    llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
    storage::EventStore,
    tool::ToolDefinition,
    types::{SessionId, new_session_id},
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_server::test_support::{
    ChildSessionCoordinator, CompletionParams, ConfigManager, DeliveryOutcome, InputDelivery,
    MAX_PENDING_INPUTS_PER_SESSION, MAX_PROMPT_TEXT_BYTES, SessionManager, TurnRegistry,
    TurnScheduleError, TurnScheduler,
};
use astrcode_session::SessionRuntimeServices;
use astrcode_storage::in_memory::InMemoryEventStore;
use tokio::sync::mpsc;

struct StaticTextLlm;
struct PendingLlm;

#[async_trait::async_trait]
impl LlmProvider for StaticTextLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
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
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
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

async fn seed_session(store: &Arc<dyn EventStore>) -> SessionId {
    let sid = new_session_id();
    store
        .create_session(&sid, ".", "mock", None, None, None)
        .await
        .unwrap();
    sid
}

fn build_scheduler(store: Arc<dyn EventStore>) -> TurnScheduler {
    build_scheduler_with_llm(store, Arc::new(StaticTextLlm))
}

fn build_scheduler_with_llm(
    store: Arc<dyn EventStore>,
    llm: Arc<dyn LlmProvider>,
) -> TurnScheduler {
    let extension_runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
    let context_assembler = Arc::new(LlmContextAssembler::new(Default::default()));
    let effective = EffectiveConfig {
        llm: LlmSettings {
            provider_kind: "mock".into(),
            base_url: String::new(),
            api_key: String::new(),
            api_mode: OpenAiApiMode::ChatCompletions,
            model_id: "mock".into(),
            max_tokens: 1024,
            context_limit: 1024,
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            max_retries: 0,
            retry_base_delay_ms: 0,
            supports_prompt_cache_key: false,
            prompt_cache_retention: None,
            reasoning: false,
            thinking_level: None,
        },
        small_llm: LlmSettings {
            provider_kind: "mock".into(),
            base_url: String::new(),
            api_key: String::new(),
            api_mode: OpenAiApiMode::ChatCompletions,
            model_id: "mock".into(),
            max_tokens: 1024,
            context_limit: 1024,
            connect_timeout_secs: 1,
            read_timeout_secs: 1,
            max_retries: 0,
            retry_base_delay_ms: 0,
            supports_prompt_cache_key: false,
            prompt_cache_retention: None,
            reasoning: false,
            thinking_level: None,
        },
        context: Default::default(),
        agent: Default::default(),
        permissions: Default::default(),
        extensions: ExtensionSettings::default(),
    };
    let shell_timeout_secs = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
    let capabilities = Arc::new(SessionRuntimeServices::new(
        Arc::clone(&llm),
        llm,
        effective,
        astrcode_server::default_host::first_party_host_services(
            extension_runner.clone(),
            context_assembler,
            std::sync::Arc::clone(&shell_timeout_secs),
        ),
    ));
    let config = Arc::new(ConfigManager::new(
        Arc::new(astrcode_storage::config_store::FileConfigStore::new(
            std::path::PathBuf::from("target/turn-behavior-config.json"),
        )),
        Default::default(),
        Arc::clone(&extension_runner),
        shell_timeout_secs,
        Arc::clone(&capabilities),
    ));
    let session_manager = Arc::new(SessionManager::new(
        Arc::clone(&store),
        config,
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
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler(Arc::clone(&store));
    let sid = seed_session(&store).await;

    let started = scheduler
        .start_with_completion(sid.clone(), "hello".into())
        .await
        .unwrap();
    let _ = started.handle.wait().await;
    scheduler
        .finish_and_maybe_start_next(CompletionParams {
            session_id: sid.clone(),
            turn_id: started.turn_id,
        })
        .await;

    let events = store.replay_events(&sid).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e.payload, EventPayload::TurnStarted))
    );
    assert!(events.iter().any(|e| matches!(
        &e.payload,
        EventPayload::UserMessage { text, .. } if text == "hello"
    )));
}

#[tokio::test]
async fn running_submit_returns_busy() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
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
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
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
        .filter(|event| matches!(event.payload, EventPayload::UserMessage { .. }))
        .count();
    assert_eq!(user_messages, 1);
}

#[tokio::test]
async fn running_inject_writes_user_message_under_active_turn() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler(Arc::clone(&store));
    let sid = seed_session(&store).await;

    let started = scheduler
        .start_with_completion(sid.clone(), "first".into())
        .await
        .unwrap();
    let turn_id = started.turn_id.clone();
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

    let events = store.replay_events(&sid).await.unwrap();
    let injected = events
        .iter()
        .find(|e| {
            matches!(
                &e.payload,
                EventPayload::UserMessage { text, .. } if text == "inject me"
            )
        })
        .expect("injected message");
    assert_eq!(injected.turn_id.as_ref(), Some(&turn_id));
}

#[tokio::test]
async fn running_queue_does_not_start_second_turn() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
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
async fn running_queue_rejects_when_pending_limit_is_reached() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
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
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
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
            .any(|event| matches!(event.payload, EventPayload::TurnStarted))
    );
}

fn turn_completed_reasons(events: &[astrcode_core::event::Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.payload {
            EventPayload::TurnCompleted { finish_reason } => Some(finish_reason.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn release_completed_execution_does_not_emit_aborted_turn() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler(Arc::clone(&store));
    let sid = seed_session(&store).await;

    let started = scheduler
        .start_with_completion(sid.clone(), "done".into())
        .await
        .unwrap();
    let _ = started.handle.wait().await;

    let before = turn_completed_reasons(&store.replay_events(&sid).await.unwrap());
    assert_eq!(before, vec!["stop"]);

    scheduler.release_completed_execution(&sid).await;

    let after = turn_completed_reasons(&store.replay_events(&sid).await.unwrap());
    assert_eq!(
        after,
        vec!["stop"],
        "release_completed_execution must not append TurnCompleted(aborted)"
    );
    assert!(!scheduler.registry().has_active(&sid));
}

#[tokio::test]
async fn cleanup_after_finished_registry_entry_does_not_emit_duplicate_terminal() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler(Arc::clone(&store));
    let sid = seed_session(&store).await;

    let started = scheduler
        .start_with_completion(sid.clone(), "done".into())
        .await
        .unwrap();
    let _ = started.handle.wait().await;

    scheduler.abort_and_cleanup(&sid).await;

    let reasons = turn_completed_reasons(&store.replay_events(&sid).await.unwrap());
    assert_eq!(
        reasons,
        vec!["stop"],
        "cleanup of a finished registry entry must not append a second terminal event"
    );
}

#[tokio::test]
async fn execution_view_uses_registry_for_active_turn() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
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
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler_with_llm(Arc::clone(&store), Arc::new(PendingLlm));
    let sid = seed_session(&store).await;

    let started = scheduler
        .start_with_completion(sid.clone(), "run".into())
        .await
        .unwrap();
    let turn_id = started.turn_id.clone();

    scheduler.abort(&sid).await.unwrap();
    assert!(
        scheduler.registry().has_active(&sid),
        "cooperative abort keeps the registry entry until the runner exits"
    );

    let result = started.handle.wait().await.expect("turn result");
    assert!(matches!(
        result.output,
        Err(astrcode_session::TurnError::Aborted)
    ));

    scheduler
        .finish_and_maybe_start_next(CompletionParams {
            session_id: sid.clone(),
            turn_id,
        })
        .await;
    assert!(!scheduler.registry().has_active(&sid));

    let reasons = turn_completed_reasons(&store.replay_events(&sid).await.unwrap());
    assert_eq!(reasons, vec!["aborted"]);
}

#[tokio::test]
async fn detached_task_tracking_prunes_finished_handles() {
    let store: Arc<dyn EventStore> = Arc::new(InMemoryEventStore::new());
    let scheduler = build_scheduler(Arc::clone(&store));
    let sid = seed_session(&store).await;

    scheduler
        .deliver_input(sid.clone(), "first".into(), InputDelivery::StartNew)
        .await
        .unwrap();
    for _ in 0..50 {
        if scheduler.tracked_detached_task_count() == 0
            && scheduler.tracked_detached_task_slots() == 1
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(scheduler.tracked_detached_task_count(), 0);
    assert_eq!(scheduler.tracked_detached_task_slots(), 1);

    scheduler
        .deliver_input(sid, "second".into(), InputDelivery::StartNew)
        .await
        .unwrap();
    assert_eq!(
        scheduler.tracked_detached_task_slots(),
        1,
        "tracking a new detached task should prune finished handles first"
    );
}
