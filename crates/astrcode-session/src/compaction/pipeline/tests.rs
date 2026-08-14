use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use astrcode_core::{
    compaction::CompactStrategy,
    event::{DurableEvent, DurableEventPayload, EventPayload, LiveEventPayload, StoredEvent},
    tool::ToolResultArtifactSlice,
    types::{Cursor, SessionId, new_message_id, new_session_id},
};
use astrcode_extension_sdk::{
    extension::{
        CompactEvent, CompactResult as HookCompactResult, ExtensionError,
        internal::RuntimeCompactContext,
    },
    runtime_ports::TurnHooks,
};
use astrcode_session_projection::{SessionReadModel, SessionSummary};
use astrcode_storage::{
    EventConsumerCheckpointOutcome, EventConsumerCheckpointReset, EventConsumerState, EventReader,
    SessionEventJournal, SessionPathResolver, SessionReader, SessionStore, StorageError,
    ToolResultArtifactInput, ToolResultArtifactRef, ToolResultArtifactStore,
    in_memory::InMemoryEventStore,
};
use parking_lot::Mutex;
use tokio::sync::mpsc;

use super::{CompactionPipeline, CompactionPipelineOutcome};
use crate::{
    Session, SessionCreateParams, SessionEventSink, SessionRuntimeState,
    test_support::{ChannelObserver, test_runtime_services_with_hooks},
    turn_context::hook_call_context_for_read_model,
};

#[derive(Clone, Copy)]
enum Scenario {
    Success,
    Blocked,
    SyncFailed,
    CheckpointFailed,
}

#[derive(Default)]
struct RecordingStore {
    inner: InMemoryEventStore,
    calls: Arc<Mutex<Vec<&'static str>>>,
    fail_sync: AtomicBool,
    fail_checkpoint: AtomicBool,
}

impl RecordingStore {
    fn fail_next_sync(&self) {
        self.fail_sync.store(true, Ordering::SeqCst);
    }

    fn fail_next_checkpoint(&self) {
        self.fail_checkpoint.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl EventReader for RecordingStore {
    async fn replay_events(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        self.inner.replay_events(session_id).await
    }

    async fn latest_cursor(&self, session_id: &SessionId) -> Result<Option<Cursor>, StorageError> {
        self.inner.latest_cursor(session_id).await
    }

    async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        self.inner.replay_from(session_id, cursor).await
    }

    async fn list_sessions(&self) -> Result<Vec<SessionId>, StorageError> {
        self.inner.list_sessions().await
    }
}

#[async_trait::async_trait]
impl SessionReader for RecordingStore {
    async fn session_read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<SessionReadModel>, StorageError> {
        self.inner.session_read_model(session_id).await
    }

    async fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError> {
        self.inner.list_session_summaries().await
    }
}

#[async_trait::async_trait]
impl SessionPathResolver for RecordingStore {
    async fn session_store_dir(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<PathBuf>, StorageError> {
        self.inner.session_store_dir(session_id).await
    }

    async fn planned_session_store_dir(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        parent_session_id: Option<&SessionId>,
        source_extension: Option<&str>,
    ) -> Result<Option<PathBuf>, StorageError> {
        self.inner
            .planned_session_store_dir(session_id, working_dir, parent_session_id, source_extension)
            .await
    }
}

#[async_trait::async_trait]
impl ToolResultArtifactStore for RecordingStore {
    async fn read_tool_result_artifact(
        &self,
        session_id: &SessionId,
        artifact_id: &str,
        byte_offset: usize,
        max_bytes: usize,
    ) -> Result<ToolResultArtifactSlice, StorageError> {
        self.inner
            .read_tool_result_artifact(session_id, artifact_id, byte_offset, max_bytes)
            .await
    }

    async fn write_tool_result_artifact(
        &self,
        session_id: &SessionId,
        artifact: ToolResultArtifactInput,
    ) -> Result<ToolResultArtifactRef, StorageError> {
        self.inner
            .write_tool_result_artifact(session_id, artifact)
            .await
    }
}

#[async_trait::async_trait]
impl SessionEventJournal for RecordingStore {
    async fn create_session(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
        self.inner.create_session(event).await
    }

    async fn append_events(
        &self,
        events: Vec<DurableEvent>,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        self.calls.lock().push("append");
        self.inner.append_events(events).await
    }

    async fn append_events_and_sync(
        &self,
        events: Vec<DurableEvent>,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        self.calls.lock().push("append");
        self.calls.lock().push("sync");
        if self.fail_sync.swap(false, Ordering::SeqCst) {
            return Err(StorageError::InvalidEvent("injected sync failure".into()));
        }
        self.inner.append_events_and_sync(events).await
    }

    async fn sync_durable_events(&self, session_id: &SessionId) -> Result<(), StorageError> {
        self.calls.lock().push("sync");
        if self.fail_sync.swap(false, Ordering::SeqCst) {
            return Err(StorageError::InvalidEvent("injected sync failure".into()));
        }
        self.inner.sync_durable_events(session_id).await
    }
}

#[async_trait::async_trait]
impl SessionStore for RecordingStore {
    async fn event_consumer_state(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
    ) -> Result<EventConsumerState, StorageError> {
        self.inner
            .event_consumer_state(session_id, consumer_id)
            .await
    }

    async fn checkpoint_event_consumer(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        expected_revision: u64,
        seq: u64,
    ) -> Result<EventConsumerCheckpointOutcome, StorageError> {
        self.inner
            .checkpoint_event_consumer(session_id, consumer_id, expected_revision, seq)
            .await
    }

    async fn set_event_consumer_paused(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        paused: bool,
    ) -> Result<EventConsumerState, StorageError> {
        self.inner
            .set_event_consumer_paused(session_id, consumer_id, paused)
            .await
    }

    async fn reset_event_consumer_checkpoint(
        &self,
        session_id: &SessionId,
        consumer_id: &str,
        reset: EventConsumerCheckpointReset,
    ) -> Result<EventConsumerState, StorageError> {
        self.inner
            .reset_event_consumer_checkpoint(session_id, consumer_id, reset)
            .await
    }

    async fn checkpoint(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<(), StorageError> {
        self.calls.lock().push("checkpoint");
        if self.fail_checkpoint.swap(false, Ordering::SeqCst) {
            return Err(StorageError::InvalidEvent(
                "injected checkpoint failure".into(),
            ));
        }
        self.inner.checkpoint(session_id, cursor).await
    }

    async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        self.inner.delete_session(session_id).await
    }
}

struct RecordingHooks {
    scenario: Scenario,
    calls: Arc<Mutex<Vec<&'static str>>>,
}

#[async_trait::async_trait]
impl TurnHooks for RecordingHooks {
    async fn emit_compact(
        &self,
        event: CompactEvent,
        _ctx: RuntimeCompactContext,
    ) -> Result<HookCompactResult, ExtensionError> {
        match event {
            CompactEvent::PreCompact => {
                self.calls.lock().push("pre");
                if matches!(self.scenario, Scenario::Blocked) {
                    return Ok(HookCompactResult::Block {
                        reason: "blocked by test hook".into(),
                    });
                }
            },
            CompactEvent::PostCompact => self.calls.lock().push("post"),
        }
        Ok(HookCompactResult::Allow)
    }
}

fn record_compaction_event(
    payload: &EventPayload,
    started: &mut usize,
    terminals: &mut Vec<&'static str>,
) {
    match payload {
        EventPayload::Live(LiveEventPayload::CompactionStarted) => *started += 1,
        EventPayload::Live(LiveEventPayload::CompactionCompleted { .. }) => {
            terminals.push("completed");
        },
        EventPayload::Live(LiveEventPayload::CompactionSkipped { .. }) => {
            terminals.push("skipped");
        },
        EventPayload::Live(LiveEventPayload::CompactionFailed { .. }) => {
            terminals.push("failed");
        },
        _ => {},
    }
}

#[tokio::test]
async fn pipeline_orders_durability_hooks_and_exactly_one_terminal_for_each_outcome() {
    for scenario in [
        Scenario::Success,
        Scenario::Blocked,
        Scenario::SyncFailed,
        Scenario::CheckpointFailed,
    ] {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let store = Arc::new(RecordingStore {
            calls: Arc::clone(&calls),
            ..Default::default()
        });
        let hooks: Arc<dyn TurnHooks> = Arc::new(RecordingHooks {
            scenario,
            calls: Arc::clone(&calls),
        });
        let services = test_runtime_services_with_hooks(Arc::clone(&hooks));
        let session_id = new_session_id();
        let store_port: Arc<dyn SessionStore> = store.clone();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let event_sink = Arc::new(SessionEventSink::new(ChannelObserver::new(event_tx)));
        let runtime = Arc::new(SessionRuntimeState::new_with_event_sink(
            session_id, store_port, event_sink,
        ));
        let working_dir = tempfile::tempdir().unwrap();
        let session = Session::create_with_params(SessionCreateParams {
            working_dir: working_dir.path().to_string_lossy().into_owned(),
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
        .unwrap();

        for index in 0..4 {
            session
                .emit_durable(
                    None,
                    DurableEventPayload::UserMessage {
                        message_id: new_message_id(),
                        text: format!("old user {index}"),
                        attachments: Vec::new(),
                        accepted_seq: None,
                    },
                )
                .await
                .unwrap();
            session
                .emit_durable(
                    None,
                    DurableEventPayload::AssistantMessageCompleted {
                        message_id: new_message_id(),
                        text: format!("old answer {index}"),
                        reasoning_content: None,
                    },
                )
                .await
                .unwrap();
        }

        calls.lock().clear();
        while event_rx.try_recv().is_ok() {}
        match scenario {
            Scenario::SyncFailed => store.fail_next_sync(),
            Scenario::CheckpointFailed => store.fail_next_checkpoint(),
            Scenario::Success | Scenario::Blocked => {},
        }

        let model = session.read_model().await.unwrap();
        let hook_call = hook_call_context_for_read_model(
            session.id(),
            &model,
            session.session_store_dir().await,
        );
        let outcome = CompactionPipeline {
            session: &session,
            llm: session.runtime_services().llm(),
            context_assembler: Arc::clone(
                session
                    .runtime_services()
                    .pin_runtime_generation()
                    .context_assembler(),
            ),
            extension_runner: hooks.as_ref(),
            hook_call,
            pre_hook_message_count: model.model_context.messages.len(),
            tools: &[],
            strategy: CompactStrategy::Manual {
                keep_recent_turns: Some(1),
            },
            use_llm: false,
        }
        .run()
        .await;

        let (expected_calls, expected_terminal) = match scenario {
            Scenario::Success => (
                vec!["pre", "append", "sync", "checkpoint", "post"],
                "completed",
            ),
            Scenario::Blocked => (vec!["pre"], "skipped"),
            Scenario::SyncFailed => (vec!["pre", "append", "sync"], "failed"),
            Scenario::CheckpointFailed => (
                vec!["pre", "append", "sync", "checkpoint", "post"],
                "completed",
            ),
        };
        assert_eq!(*calls.lock(), expected_calls);
        assert!(matches!(
            (scenario, &outcome),
            (
                Scenario::Success | Scenario::CheckpointFailed,
                CompactionPipelineOutcome::Compacted { .. }
            ) | (Scenario::Blocked, CompactionPipelineOutcome::Skipped { .. })
                | (
                    Scenario::SyncFailed,
                    CompactionPipelineOutcome::Failed { .. }
                )
        ));

        let mut started = 0;
        let mut terminals = Vec::new();
        while terminals.is_empty() {
            let event = tokio::time::timeout(std::time::Duration::from_secs(1), event_rx.recv())
                .await
                .expect("compaction live event should be published")
                .expect("event observer should remain connected");
            record_compaction_event(&event.payload, &mut started, &mut terminals);
        }
        tokio::task::yield_now().await;
        while let Ok(event) = event_rx.try_recv() {
            record_compaction_event(&event.payload, &mut started, &mut terminals);
        }
        assert_eq!(started, 1);
        assert_eq!(terminals, vec![expected_terminal]);
    }
}
