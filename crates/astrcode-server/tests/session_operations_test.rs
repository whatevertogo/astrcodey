//! 集成测试：ServerSessionOperations 的 submit_turn 同步/异步路径。

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use astrcode_context::context_assembler::LlmContextAssembler;
use astrcode_core::{
    config::{
        EffectiveConfig, ExtensionSettings, LlmSettings, ProviderAuthScheme, ProviderWireFormat,
    },
    event::{DurableEvent, DurableEventPayload, StoredEvent},
    llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
    tool::{
        CreateRootSessionRequest, CreateSessionRequest, SessionAccess, SessionDeliveryOutcome,
        SessionLifecycleState, SessionOperations, SubmitTurnRequest, SubmitTurnResult,
        ToolDefinition,
    },
    types::{Cursor, SessionId, new_session_id, new_turn_id},
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_server::test_support::{
    ChildSessionCoordinator, ServerSessionOperations, SessionManager, TurnRegistry, TurnScheduler,
    finish_and_watch_next_for_test, pause_next_completion_guard_claim_for_test,
    pause_next_completion_guard_registration_for_test, pause_next_sync_completion_settled_for_test,
    registered_completion_guard_count_for_test, session_started_event_for_test,
    start_with_completion_and_hold_operation_for_test, start_with_completion_for_test,
};
use astrcode_session_projection::{AgentSessionStatus, SessionReadModel, SessionSummary, replay};
use astrcode_storage::{
    EventReader, SessionEventJournal, SessionPathResolver, SessionReader, SessionStore,
    StorageError, ToolResultArtifactInput, ToolResultArtifactRef, ToolResultArtifactStore,
    in_memory::InMemoryEventStore,
};
use tokio::sync::{Mutex as AsyncMutex, Semaphore, mpsc, oneshot};

struct StaticTextLlm {
    text: &'static str,
}

/// 在发送 Done 前阻塞，便于在活跃 turn 期间调用 `inject_message`。
struct GateLlm {
    release: Arc<tokio::sync::Notify>,
}

impl GateLlm {
    fn new_pair() -> (Self, Arc<tokio::sync::Notify>) {
        let release = Arc::new(tokio::sync::Notify::new());
        (
            Self {
                release: Arc::clone(&release),
            },
            release,
        )
    }
}

#[async_trait::async_trait]
impl LlmProvider for GateLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let release = Arc::clone(&self.release);
        tokio::spawn(async move {
            let _ = tx.send(LlmEvent::ContentDelta {
                delta: "partial".into(),
            });
            release.notified().await;
            let _ = tx.send(LlmEvent::Done {
                finish_reason: "stop".into(),
            });
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
impl LlmProvider for StaticTextLlm {
    async fn generate(
        &self,
        _messages: Vec<LlmMessage>,
        _tools: Vec<ToolDefinition>,
    ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(LlmEvent::ContentDelta {
            delta: self.text.into(),
        });
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

struct BlockingChildCreateStore {
    inner: InMemoryEventStore,
    block_next_child: AtomicBool,
    block_next_delete: AtomicBool,
    block_next_recycle: AtomicBool,
    block_next_restore: AtomicBool,
    fail_next_delete: AtomicBool,
    fail_next_deleted_event: AtomicBool,
    fail_next_recycled_event: AtomicBool,
    child_create_started: Semaphore,
    release_child_create: Semaphore,
    delete_started: Semaphore,
    release_delete: Semaphore,
    recycle_started: Semaphore,
    release_recycle: Semaphore,
    restore_started: Semaphore,
    release_restore: Semaphore,
    restore_finished: Semaphore,
    recycled: AsyncMutex<HashMap<SessionId, Vec<StoredEvent>>>,
    fail_sync_session: AsyncMutex<Option<SessionId>>,
}

impl BlockingChildCreateStore {
    fn new() -> Self {
        Self {
            inner: InMemoryEventStore::new(),
            block_next_child: AtomicBool::new(false),
            block_next_delete: AtomicBool::new(false),
            block_next_recycle: AtomicBool::new(false),
            block_next_restore: AtomicBool::new(false),
            fail_next_delete: AtomicBool::new(false),
            fail_next_deleted_event: AtomicBool::new(false),
            fail_next_recycled_event: AtomicBool::new(false),
            child_create_started: Semaphore::new(0),
            release_child_create: Semaphore::new(0),
            delete_started: Semaphore::new(0),
            release_delete: Semaphore::new(0),
            recycle_started: Semaphore::new(0),
            release_recycle: Semaphore::new(0),
            restore_started: Semaphore::new(0),
            release_restore: Semaphore::new(0),
            restore_finished: Semaphore::new(0),
            recycled: AsyncMutex::new(HashMap::new()),
            fail_sync_session: AsyncMutex::new(None),
        }
    }

    fn block_next_child_create(&self) {
        self.block_next_child.store(true, Ordering::Release);
    }

    async fn wait_for_child_create(&self) {
        self.child_create_started.acquire().await.unwrap().forget();
    }

    fn release_child_create(&self) {
        self.release_child_create.add_permits(1);
    }

    fn fail_next_recycled_event(&self) {
        self.fail_next_recycled_event.store(true, Ordering::Release);
    }

    fn fail_next_delete(&self) {
        self.fail_next_delete.store(true, Ordering::Release);
    }

    fn block_next_delete(&self) {
        self.block_next_delete.store(true, Ordering::Release);
    }

    async fn wait_for_delete(&self) {
        self.delete_started.acquire().await.unwrap().forget();
    }

    fn release_delete(&self) {
        self.release_delete.add_permits(1);
    }

    fn block_next_recycle(&self) {
        self.block_next_recycle.store(true, Ordering::Release);
    }

    async fn wait_for_recycle(&self) {
        self.recycle_started.acquire().await.unwrap().forget();
    }

    fn release_recycle(&self) {
        self.release_recycle.add_permits(1);
    }

    fn block_next_restore(&self) {
        self.block_next_restore.store(true, Ordering::Release);
    }

    async fn wait_for_restore(&self) {
        self.restore_started.acquire().await.unwrap().forget();
    }

    fn release_restore(&self) {
        self.release_restore.add_permits(1);
    }

    async fn wait_for_restore_finished(&self) {
        self.restore_finished.acquire().await.unwrap().forget();
    }

    fn fail_next_deleted_event(&self) {
        self.fail_next_deleted_event.store(true, Ordering::Release);
    }

    async fn fail_next_sync_for(&self, session_id: SessionId) {
        *self.fail_sync_session.lock().await = Some(session_id);
    }
}

#[async_trait::async_trait]
impl EventReader for BlockingChildCreateStore {
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
impl SessionReader for BlockingChildCreateStore {
    async fn session_read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<SessionReadModel>, StorageError> {
        self.inner.session_read_model(session_id).await
    }

    async fn recycled_session_read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<SessionReadModel>, StorageError> {
        let events = self
            .recycled
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        replay(session_id.clone(), &events)
            .map(Arc::new)
            .map_err(|error| StorageError::CorruptLog(error.to_string()))
    }

    async fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError> {
        self.inner.list_session_summaries().await
    }
}

#[async_trait::async_trait]
impl SessionPathResolver for BlockingChildCreateStore {
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
impl ToolResultArtifactStore for BlockingChildCreateStore {
    async fn read_tool_result_artifact_by_path(
        &self,
        session_id: &SessionId,
        path: &str,
        char_offset: usize,
        max_chars: usize,
    ) -> Result<astrcode_core::tool::ToolResultArtifactSlice, StorageError> {
        self.inner
            .read_tool_result_artifact_by_path(session_id, path, char_offset, max_chars)
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
impl SessionEventJournal for BlockingChildCreateStore {
    async fn create_session(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
        let is_child = matches!(
            &event.payload,
            DurableEventPayload::SessionStarted(started) if started.parent.is_some()
        );
        if is_child && self.block_next_child.swap(false, Ordering::AcqRel) {
            self.child_create_started.add_permits(1);
            self.release_child_create.acquire().await.unwrap().forget();
        }
        self.inner.create_session(event).await
    }

    async fn append_event(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
        if matches!(
            &event.payload,
            DurableEventPayload::AgentSessionFailed { error, .. } if error == "deleted"
        ) && self.fail_next_deleted_event.swap(false, Ordering::AcqRel)
        {
            return Err(StorageError::Unsupported(
                "injected deleted relation failure".into(),
            ));
        }
        if matches!(
            &event.payload,
            DurableEventPayload::AgentSessionRecycled { .. }
        ) && self.fail_next_recycled_event.swap(false, Ordering::AcqRel)
        {
            return Err(StorageError::Unsupported(
                "injected recycled relation failure".into(),
            ));
        }
        self.inner.append_event(event).await
    }

    async fn sync_durable_events(&self, session_id: &SessionId) -> Result<(), StorageError> {
        let mut fail_sync_session = self.fail_sync_session.lock().await;
        if fail_sync_session.as_ref() == Some(session_id) {
            fail_sync_session.take();
            return Err(StorageError::Unsupported(
                "injected durable sync failure".into(),
            ));
        }
        drop(fail_sync_session);
        self.inner.sync_durable_events(session_id).await
    }
}

#[async_trait::async_trait]
impl SessionStore for BlockingChildCreateStore {
    async fn checkpoint(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<(), StorageError> {
        self.inner.checkpoint(session_id, cursor).await
    }

    async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        if self.block_next_delete.swap(false, Ordering::AcqRel) {
            self.delete_started.add_permits(1);
            self.release_delete.acquire().await.unwrap().forget();
        }
        if self.fail_next_delete.swap(false, Ordering::AcqRel) {
            return Err(StorageError::Unsupported(
                "injected session delete failure".into(),
            ));
        }
        self.inner.delete_session(session_id).await
    }

    async fn recycle_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        if self.block_next_recycle.swap(false, Ordering::AcqRel) {
            self.recycle_started.add_permits(1);
            self.release_recycle.acquire().await.unwrap().forget();
        }
        let events = self.inner.replay_events(session_id).await?;
        self.inner.delete_session(session_id).await?;
        self.recycled
            .lock()
            .await
            .insert(session_id.clone(), events);
        Ok(())
    }

    async fn restore_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
        let was_blocked = self.block_next_restore.swap(false, Ordering::AcqRel);
        if was_blocked {
            self.restore_started.add_permits(1);
            self.release_restore.acquire().await.unwrap().forget();
        }
        let events = self
            .recycled
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        let mut events = events.into_iter();
        let first = events
            .next()
            .ok_or_else(|| StorageError::NotFound(session_id.clone()))?;
        self.inner.create_session(first.event).await?;
        for event in events {
            self.inner.append_event(event.event).await?;
        }
        self.recycled.lock().await.remove(session_id);
        if was_blocked {
            self.restore_finished.add_permits(1);
        }
        Ok(())
    }
}

fn build_test_ops_with_llm(
    store: Arc<dyn SessionStore>,
    llm_provider: Arc<dyn LlmProvider>,
) -> Arc<ServerSessionOperations> {
    let extension_runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
    let context_assembler = Arc::new(LlmContextAssembler::new(Default::default()));
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
    let shell_timeout_secs = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
    let capabilities = astrcode_server::test_support::assemble_session_runtime_services_for_test(
        llm_provider.clone(),
        llm_provider,
        effective,
        extension_runner.clone(),
        context_assembler,
        std::sync::Arc::clone(&shell_timeout_secs),
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
    Arc::new(ServerSessionOperations {
        session_manager,
        scheduler,
        child_sessions,
    })
}

fn build_test_ops(
    store: Arc<dyn SessionStore>,
    llm_text: &'static str,
) -> Arc<ServerSessionOperations> {
    build_test_ops_with_llm(store, Arc::new(StaticTextLlm { text: llm_text }))
}

#[tokio::test]
async fn create_root_session_persists_source_extension_attribution() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let ops = build_test_ops(Arc::clone(&store), "unused");

    let created = ops
        .create_root_session(CreateRootSessionRequest {
            working_dir: ".".into(),
            source_extension: Some("channel-a".into()),
        })
        .await
        .expect("create attributed root session");
    let model = store
        .session_read_model(&SessionId::new(created.session_id))
        .await
        .expect("read created root session");

    assert!(model.identity.parent.is_none());
    assert_eq!(
        model.identity.source_extension.as_deref(),
        Some("channel-a")
    );
}

#[tokio::test]
async fn restore_authorizes_against_recycled_ancestor_chain_before_mutation() {
    let blocking_store = Arc::new(BlockingChildCreateStore::new());
    let store: Arc<dyn SessionStore> = blocking_store.clone();
    let root_id = new_session_id();
    store
        .create_session(session_started_event_for_test(root_id.clone(), ".", "mock"))
        .await
        .unwrap();
    let ops = build_test_ops(Arc::clone(&store), "unused");

    let child = ops
        .create_session(
            root_id.as_str(),
            CreateSessionRequest {
                name: "recycled-parent".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let child_id = SessionId::from(child.session_id);
    let grandchild = ops
        .create_session(
            child_id.as_str(),
            CreateSessionRequest {
                name: "recycled-target".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let grandchild_id = SessionId::from(grandchild.session_id);
    let sibling = ops
        .create_session(
            root_id.as_str(),
            CreateSessionRequest {
                name: "unauthorized-sibling".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let sibling_id = SessionId::from(sibling.session_id);

    ops.recycle_session(SessionAccess::new(root_id.as_str(), child_id.as_str()))
        .await
        .unwrap();
    assert!(store.session_read_model(&child_id).await.is_err());
    assert!(store.session_read_model(&grandchild_id).await.is_err());

    let denied = ops
        .restore_session(SessionAccess::new(
            sibling_id.as_str(),
            grandchild_id.as_str(),
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(
            denied,
            astrcode_core::tool::SessionApiError::PermissionDenied(_)
        ),
        "a sibling must not restore a recycled target: {denied}"
    );
    assert!(
        store.session_read_model(&grandchild_id).await.is_err(),
        "authorization must complete before storage is restored"
    );

    blocking_store.block_next_restore();
    let restore_ops = Arc::clone(&ops);
    let restore_root_id = root_id.clone();
    let restore_grandchild_id = grandchild_id.clone();
    let restore_request = tokio::spawn(async move {
        restore_ops
            .restore_session(SessionAccess::new(
                restore_root_id.as_str(),
                restore_grandchild_id.as_str(),
            ))
            .await
    });
    blocking_store.wait_for_restore().await;
    restore_request.abort();
    assert!(restore_request.await.unwrap_err().is_cancelled());
    blocking_store.release_restore();
    tokio::time::timeout(
        Duration::from_secs(5),
        blocking_store.wait_for_restore_finished(),
    )
    .await
    .unwrap();
    let restored_grandchild = store.session_read_model(&grandchild_id).await.unwrap();
    assert_eq!(
        restored_grandchild
            .identity
            .parent
            .as_ref()
            .map(|parent| &parent.session_id),
        Some(&child_id),
        "restore must preserve the durable parent identity"
    );

    ops.restore_session(SessionAccess::new(root_id.as_str(), child_id.as_str()))
        .await
        .unwrap();
    ops.query_session(SessionAccess::new(root_id.as_str(), grandchild_id.as_str()))
        .await
        .unwrap();
}

#[tokio::test]
async fn reactivate_restores_runtime_parent_relation_and_is_idempotent() {
    let blocking_store = Arc::new(BlockingChildCreateStore::new());
    let store: Arc<dyn SessionStore> = blocking_store.clone();
    let root_id = new_session_id();
    store
        .create_session(session_started_event_for_test(root_id.clone(), ".", "mock"))
        .await
        .unwrap();
    let ops = build_test_ops(Arc::clone(&store), "unused");
    let child = ops
        .create_session(
            root_id.as_str(),
            CreateSessionRequest {
                name: "reactivated-child".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let child_id = SessionId::from(child.session_id);
    let access = || SessionAccess::new(root_id.as_str(), child_id.as_str());

    assert_eq!(
        ops.session_state(access()).await.unwrap().lifecycle,
        SessionLifecycleState::Active
    );
    ops.recycle_session(access()).await.unwrap();
    assert_eq!(
        ops.session_state(access()).await.unwrap().lifecycle,
        SessionLifecycleState::Recycled
    );
    assert!(
        store
            .session_read_model(&root_id)
            .await
            .unwrap()
            .agent_sessions
            .is_empty(),
        "recycling must remove the active child relation"
    );

    let reactivated = ops.reactivate_session(access()).await.unwrap();
    assert!(reactivated.reactivated);
    assert_eq!(
        ops.session_state(access()).await.unwrap().lifecycle,
        SessionLifecycleState::Active
    );
    assert!(
        store
            .session_read_model(&root_id)
            .await
            .unwrap()
            .agent_sessions
            .iter()
            .any(|link| link.child_session_id == child_id),
        "reactivation must restore the parent projection link"
    );

    let repeated = ops.reactivate_session(access()).await.unwrap();
    assert!(!repeated.reactivated, "repeat activation should be a no-op");
    assert_eq!(
        store
            .session_read_model(&root_id)
            .await
            .unwrap()
            .agent_sessions
            .iter()
            .filter(|link| link.child_session_id == child_id)
            .count(),
        1,
        "idempotent activation must not duplicate the parent link"
    );

    ops.recycle_session(access()).await.unwrap();
    blocking_store.fail_next_sync_for(root_id.clone()).await;
    let failed = ops.reactivate_session(access()).await.unwrap_err();
    assert!(
        failed.to_string().contains("injected durable sync failure"),
        "reactivation must report the parent relation sync failure: {failed}"
    );
    assert_eq!(
        ops.session_state(access()).await.unwrap().lifecycle,
        SessionLifecycleState::Recycled,
        "a failed relation sync must return the child to recycled storage"
    );
    assert!(
        store
            .session_read_model(&root_id)
            .await
            .unwrap()
            .agent_sessions
            .is_empty(),
        "reactivation rollback must remove the partially restored parent link"
    );
}

#[tokio::test]
async fn same_session_submit_uses_scheduler_without_child_relations() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let session_id = new_session_id();
    store
        .create_session(session_started_event_for_test(
            session_id.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();
    let ops = build_test_ops(Arc::clone(&store), "same-session output");

    let unsupported = ops
        .submit_turn(
            SubmitTurnRequest::for_session(session_id.as_str(), "invalid root cleanup")
                .wait_for_result(false)
                .notify_parent_on_complete(Some("notify".into()))
                .recycle_on_complete(true),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            unsupported,
            astrcode_core::tool::SessionApiError::Unsupported(_)
        ),
        "same-session child-only completion options should be rejected"
    );

    let sync_result = ops
        .submit_turn(SubmitTurnRequest::for_session(
            session_id.as_str(),
            "synchronous root turn",
        ))
        .await
        .unwrap();
    assert!(
        matches!(
            sync_result,
            SubmitTurnResult::Completed { ref content }
                if content == "same-session output"
        ),
        "same-session sync should return the turn output"
    );

    let async_result = ops
        .submit_turn(
            SubmitTurnRequest::for_session(session_id.as_str(), "background root turn")
                .wait_for_result(false),
        )
        .await
        .unwrap();
    assert!(
        matches!(
            async_result,
            SubmitTurnResult::Backgrounded {
                ref task_id,
                session_id: ref returned_session_id,
            } if !task_id.is_empty() && returned_session_id == session_id.as_str()
        ),
        "same-session async should be owned by the scheduler"
    );

    for _ in 0..100 {
        if !ops.scheduler.registry().has_active(&session_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !ops.scheduler.registry().has_active(&session_id),
        "the scheduler watcher should settle the background root turn"
    );

    let follow_up = ops
        .submit_turn(SubmitTurnRequest::for_session(
            session_id.as_str(),
            "follow-up root turn",
        ))
        .await
        .unwrap();
    assert!(matches!(follow_up, SubmitTurnResult::Completed { .. }));
    assert!(
        store
            .session_read_model(&session_id)
            .await
            .unwrap()
            .agent_sessions
            .is_empty(),
        "same-session turns must not create child relation events"
    );
}

#[tokio::test]
async fn cancelled_sync_request_keeps_independent_completion_owner() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let parent_id = new_session_id();
    store
        .create_session(session_started_event_for_test(
            parent_id.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();
    let (gate_llm, release) = GateLlm::new_pair();
    let ops = build_test_ops_with_llm(Arc::clone(&store), Arc::new(gate_llm));

    let root_ops = Arc::clone(&ops);
    let root_id = parent_id.clone();
    let root_request = tokio::spawn(async move {
        root_ops
            .submit_turn(SubmitTurnRequest::for_session(
                root_id.as_str(),
                "cancel root request",
            ))
            .await
    });
    for _ in 0..100 {
        if ops.scheduler.registry().has_active(&parent_id) && ops.scheduler.owned_task_count() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ops.scheduler.registry().has_active(&parent_id));
    root_request.abort();
    assert!(root_request.await.unwrap_err().is_cancelled());
    release.notify_one();
    for _ in 0..100 {
        if !ops.scheduler.registry().has_active(&parent_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !ops.scheduler.registry().has_active(&parent_id),
        "cancelling a same-session request must not cancel its completion owner"
    );
    assert!(
        store
            .session_read_model(&parent_id)
            .await
            .unwrap()
            .agent_sessions
            .is_empty()
    );

    let child = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "cancelled-request-child".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let child_id = SessionId::from(child.session_id);
    let child_ops = Arc::clone(&ops);
    let child_parent = parent_id.clone();
    let child_target = child_id.clone();
    let child_request = tokio::spawn(async move {
        child_ops
            .submit_turn(SubmitTurnRequest::for_child(
                child_parent.as_str(),
                child_target.as_str(),
                "cancel child request",
            ))
            .await
    });
    for _ in 0..100 {
        if ops.scheduler.registry().has_active(&child_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ops.scheduler.registry().has_active(&child_id));
    child_request.abort();
    assert!(child_request.await.unwrap_err().is_cancelled());
    release.notify_one();
    for _ in 0..100 {
        let settled = !ops.scheduler.registry().has_active(&child_id);
        let completed = store
            .session_read_model(&parent_id)
            .await
            .unwrap()
            .agent_sessions
            .iter()
            .any(|link| {
                link.child_session_id == child_id && link.status == AgentSessionStatus::Completed
            });
        if settled && completed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !ops.scheduler.registry().has_active(&child_id),
        "cancelling a child request must not cancel its completion owner"
    );
    assert!(
        store
            .session_read_model(&parent_id)
            .await
            .unwrap()
            .agent_sessions
            .iter()
            .any(|link| {
                link.child_session_id == child_id && link.status == AgentSessionStatus::Completed
            }),
        "the independent child owner must persist the parent terminal"
    );
}

#[tokio::test]
async fn cancelled_child_create_keeps_parent_gate_until_creation_settles() {
    let blocking_store = Arc::new(BlockingChildCreateStore::new());
    let store: Arc<dyn SessionStore> = blocking_store.clone();
    let parent_id = new_session_id();
    store
        .create_session(session_started_event_for_test(
            parent_id.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();
    let ops = build_test_ops(store, "unused");

    blocking_store.block_next_child_create();
    let caller_ops = Arc::clone(&ops);
    let caller_parent_id = parent_id.clone();
    let child_caller = tokio::spawn(async move {
        caller_ops
            .create_session(
                caller_parent_id.as_str(),
                CreateSessionRequest {
                    name: "cancelled-caller-child".into(),
                    source_extension: Some("test".into()),
                    ..Default::default()
                },
            )
            .await
    });
    blocking_store.wait_for_child_create().await;
    child_caller.abort();
    assert!(matches!(
        child_caller.await,
        Err(error) if error.is_cancelled()
    ));

    let delete_scheduler = Arc::clone(&ops.scheduler);
    let delete_parent_id = parent_id.clone();
    let delete_parent =
        tokio::spawn(async move { delete_scheduler.delete_session(&delete_parent_id).await });
    tokio::task::yield_now().await;
    assert!(
        !delete_parent.is_finished(),
        "the detached child owner must retain the parent operation gate"
    );

    blocking_store.release_child_create();
    delete_parent.await.unwrap().unwrap();
    assert!(
        blocking_store.session_read_model(&parent_id).await.is_err(),
        "parent deletion must finish after the child creation transaction settles"
    );
}

#[tokio::test]
async fn session_tree_close_serializes_child_creation_and_parent_links() {
    let blocking_store = Arc::new(BlockingChildCreateStore::new());
    let store: Arc<dyn SessionStore> = blocking_store.clone();
    let parent_id = new_session_id();
    store
        .create_session(session_started_event_for_test(
            parent_id.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();
    let ops = build_test_ops(store, "unused");

    let recycled_child = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "recycled-child".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let recycled_child_id = SessionId::from(recycled_child.session_id);
    blocking_store.fail_next_recycled_event();
    let recycle_error = ops
        .recycle_session(SessionAccess::new(
            parent_id.as_str(),
            recycled_child_id.as_str(),
        ))
        .await
        .unwrap_err();
    assert!(
        recycle_error.to_string().contains("event emit failed"),
        "relation append failure should be returned: {recycle_error}"
    );
    assert!(
        blocking_store
            .session_read_model(&recycled_child_id)
            .await
            .is_ok(),
        "failed relation append should restore the recycled child for retry"
    );
    assert!(
        blocking_store
            .session_read_model(&parent_id)
            .await
            .unwrap()
            .agent_sessions
            .iter()
            .any(|link| link.child_session_id == recycled_child_id),
        "rollback should preserve the parent relation"
    );

    ops.recycle_session(SessionAccess::new(
        parent_id.as_str(),
        recycled_child_id.as_str(),
    ))
    .await
    .unwrap();
    let parent_after_child_recycle = blocking_store.session_read_model(&parent_id).await.unwrap();
    assert!(
        parent_after_child_recycle
            .agent_sessions
            .iter()
            .all(|link| link.child_session_id != recycled_child_id),
        "recycle should remove the child link from its real parent"
    );
    assert_eq!(
        blocking_store
            .replay_events(&parent_id)
            .await
            .unwrap()
            .iter()
            .filter(|event| {
                matches!(
                    &event.payload,
                    DurableEventPayload::AgentSessionRecycled {
                        child_session_id,
                    } if child_session_id == &recycled_child_id
                )
            })
            .count(),
        1,
        "tree close should write the recycled relation exactly once"
    );

    let deleted_child = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "deleted-child".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let deleted_child_id = SessionId::from(deleted_child.session_id);
    ops.scheduler
        .delete_session(&deleted_child_id)
        .await
        .unwrap();
    let parent_after_child_delete = blocking_store.session_read_model(&parent_id).await.unwrap();
    let deleted_link = parent_after_child_delete
        .agent_sessions
        .iter()
        .find(|link| link.child_session_id == deleted_child_id)
        .expect("deleted child should retain an auditable terminal link");
    assert_eq!(deleted_link.status, AgentSessionStatus::Failed);
    assert_eq!(deleted_link.error.as_deref(), Some("deleted"));

    let partially_deleted_child = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "partially-deleted-child".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let partially_deleted_child_id = SessionId::from(partially_deleted_child.session_id);
    blocking_store.fail_next_deleted_event();
    let delete_error = ops
        .scheduler
        .delete_session(&partially_deleted_child_id)
        .await
        .unwrap_err();
    assert!(
        delete_error
            .to_string()
            .contains("was deleted, but updating its parent"),
        "irreversible delete should report the partial relation failure: {delete_error}"
    );
    assert!(
        blocking_store
            .session_read_model(&partially_deleted_child_id)
            .await
            .is_err(),
        "the partial failure must not imply that deleted child storage was restored"
    );
    let parent_after_partial_delete = blocking_store.session_read_model(&parent_id).await.unwrap();
    let partial_link = parent_after_partial_delete
        .agent_sessions
        .iter()
        .find(|link| link.child_session_id == partially_deleted_child_id)
        .expect("failed relation write should leave the existing parent link intact");
    assert_eq!(partial_link.status, AgentSessionStatus::Running);

    let cancelled_delete_child = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "cancelled-delete-owner".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let cancelled_delete_id = SessionId::from(cancelled_delete_child.session_id);
    blocking_store.block_next_delete();
    let delete_scheduler = Arc::clone(&ops.scheduler);
    let delete_id = cancelled_delete_id.clone();
    let delete_request =
        tokio::spawn(async move { delete_scheduler.delete_session(&delete_id).await });
    blocking_store.wait_for_delete().await;
    delete_request.abort();
    assert!(delete_request.await.unwrap_err().is_cancelled());
    blocking_store.release_delete();
    for _ in 0..100 {
        if blocking_store
            .session_read_model(&cancelled_delete_id)
            .await
            .is_err()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        blocking_store
            .session_read_model(&cancelled_delete_id)
            .await
            .is_err(),
        "the admitted delete owner must survive request cancellation"
    );

    let cancelled_recycle_child = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "cancelled-recycle-owner".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let cancelled_recycle_id = SessionId::from(cancelled_recycle_child.session_id);
    blocking_store.block_next_recycle();
    let recycle_scheduler = Arc::clone(&ops.scheduler);
    let recycle_id = cancelled_recycle_id.clone();
    let recycle_request =
        tokio::spawn(async move { recycle_scheduler.recycle_session(&recycle_id).await });
    blocking_store.wait_for_recycle().await;
    recycle_request.abort();
    assert!(recycle_request.await.unwrap_err().is_cancelled());
    blocking_store.release_recycle();
    for _ in 0..100 {
        if blocking_store
            .session_read_model(&cancelled_recycle_id)
            .await
            .is_err()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        blocking_store
            .session_read_model(&cancelled_recycle_id)
            .await
            .is_err(),
        "the admitted recycle owner must survive request cancellation"
    );

    let guarded_start_child = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "guarded-start-child".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let guarded_start_child_id = SessionId::from(guarded_start_child.session_id);
    let held_scheduler = Arc::clone(&ops.scheduler);
    let held_child_id = guarded_start_child_id.clone();
    let (start_held_tx, start_held_rx) = oneshot::channel();
    let (release_start_tx, release_start_rx) = oneshot::channel();
    let held_start = tokio::spawn(async move {
        start_with_completion_and_hold_operation_for_test(
            held_scheduler.as_ref(),
            held_child_id,
            "guard registration window".into(),
            start_held_tx,
            release_start_rx,
        )
        .await
    });
    start_held_rx.await.unwrap();

    let close_during_start_scheduler = Arc::clone(&ops.scheduler);
    let close_during_start_id = guarded_start_child_id.clone();
    let close_during_start = tokio::spawn(async move {
        close_during_start_scheduler
            .delete_session(&close_during_start_id)
            .await
    });
    tokio::task::yield_now().await;
    assert!(
        !close_during_start.is_finished(),
        "tree close must wait until background start registers its completion owner"
    );
    let _ = release_start_tx.send(());
    let held_execution = held_start.await.unwrap().unwrap();
    close_during_start.await.unwrap().unwrap();
    drop(held_execution);

    blocking_store.block_next_child_create();
    let first_ops = Arc::clone(&ops);
    let first_parent_id = parent_id.clone();
    let first_child = tokio::spawn(async move {
        first_ops
            .create_session(
                first_parent_id.as_str(),
                CreateSessionRequest {
                    name: "inflight-child".into(),
                    source_extension: Some("test".into()),
                    ..Default::default()
                },
            )
            .await
    });
    blocking_store.wait_for_child_create().await;

    let delete_scheduler = Arc::clone(&ops.scheduler);
    let delete_parent_id = parent_id.clone();
    let (delete_started_tx, delete_started_rx) = oneshot::channel();
    let delete_parent = tokio::spawn(async move {
        let _ = delete_started_tx.send(());
        delete_scheduler.delete_session(&delete_parent_id).await
    });
    delete_started_rx.await.unwrap();
    tokio::task::yield_now().await;

    let late_ops = Arc::clone(&ops);
    let late_parent_id = parent_id.clone();
    let late_child = tokio::spawn(async move {
        late_ops
            .create_session(
                late_parent_id.as_str(),
                CreateSessionRequest {
                    name: "late-child".into(),
                    source_extension: Some("test".into()),
                    ..Default::default()
                },
            )
            .await
    });

    blocking_store.fail_next_delete();
    blocking_store.release_child_create();
    let first_child = first_child.await.unwrap().unwrap();
    let delete_error = delete_parent.await.unwrap().unwrap_err();
    let late_error = late_child.await.unwrap().unwrap_err();

    assert!(
        delete_error
            .to_string()
            .contains("injected session delete failure"),
        "the descendant storage failure should be returned: {delete_error}"
    );
    assert!(
        late_error.to_string().contains("session is closing"),
        "late child creation should be rejected by the parent gate: {late_error}"
    );
    let first_child_id = SessionId::from(first_child.session_id);
    assert!(
        blocking_store.session_read_model(&parent_id).await.is_ok(),
        "a descendant delete failure must leave its ancestors retryable"
    );
    assert!(
        blocking_store
            .session_read_model(&first_child_id)
            .await
            .is_ok(),
        "the failed descendant must remain available for retry"
    );

    ops.scheduler.delete_session(&parent_id).await.unwrap();
    assert!(
        blocking_store.session_read_model(&parent_id).await.is_err(),
        "retry should remove the parent"
    );
    assert!(
        blocking_store
            .session_read_model(&first_child_id)
            .await
            .is_err(),
        "retry should remove the child persisted before the gate closed"
    );
}

#[tokio::test]
async fn recycle_repairs_stale_durable_turn_without_registry_ownership() {
    let blocking_store = Arc::new(BlockingChildCreateStore::new());
    let store: Arc<dyn SessionStore> = blocking_store.clone();
    let session_id = new_session_id();
    store
        .create_session(session_started_event_for_test(
            session_id.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();
    let stale_turn_id = new_turn_id();
    store
        .append_event(DurableEvent::new(
            session_id.clone(),
            Some(stale_turn_id),
            DurableEventPayload::TurnStarted,
        ))
        .await
        .unwrap();
    let ops = build_test_ops(Arc::clone(&store), "unused");

    ops.scheduler.recycle_session(&session_id).await.unwrap();
    blocking_store.restore_session(&session_id).await.unwrap();

    let restored = store.session_read_model(&session_id).await.unwrap();
    assert_eq!(restored.execution.phase, astrcode_core::event::Phase::Idle);
    assert!(
        store
            .replay_events(&session_id)
            .await
            .unwrap()
            .iter()
            .any(|event| {
                matches!(
                    &event.payload,
                    DurableEventPayload::TurnCompleted { finish_reason }
                        if finish_reason == "interrupted"
                )
            }),
        "destructive close must persist a stale-turn terminal before recycling"
    );
}

#[tokio::test]
async fn completion_guard_registration_and_claim_serialize_tree_close() {
    let blocking_store = Arc::new(BlockingChildCreateStore::new());
    let store: Arc<dyn SessionStore> = blocking_store.clone();
    let parent_id = new_session_id();
    store
        .create_session(session_started_event_for_test(
            parent_id.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();
    let ops = build_test_ops(Arc::clone(&store), "completed before close");

    let registration_child = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "registration-child".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let registration_child_id = SessionId::from(registration_child.session_id);
    let (registration_reached, release_registration) =
        pause_next_completion_guard_registration_for_test(&ops.child_sessions);
    let registration_ops = Arc::clone(&ops);
    let registration_parent = parent_id.clone();
    let registration_target = registration_child_id.clone();
    let registration_submit = tokio::spawn(async move {
        registration_ops
            .submit_turn(
                SubmitTurnRequest::for_child(
                    registration_parent.as_str(),
                    registration_target.as_str(),
                    "finish immediately",
                )
                .wait_for_result(false),
            )
            .await
    });
    registration_reached.await.unwrap();

    let registration_delete_scheduler = Arc::clone(&ops.scheduler);
    let registration_delete_target = registration_child_id.clone();
    let registration_delete = tokio::spawn(async move {
        registration_delete_scheduler
            .delete_session(&registration_delete_target)
            .await
    });
    tokio::task::yield_now().await;
    assert!(
        !registration_delete.is_finished(),
        "tree close must wait while a registered guard is starting its watcher"
    );
    let _ = release_registration.send(());
    registration_submit.await.unwrap().unwrap();
    registration_delete.await.unwrap().unwrap();

    let claim_child = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "claim-child".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let claim_child_id = SessionId::from(claim_child.session_id);
    let (claim_reached, release_claim) =
        pause_next_completion_guard_claim_for_test(&ops.child_sessions);
    let claim_ops = Arc::clone(&ops);
    let claim_parent = parent_id.clone();
    let claim_target = claim_child_id.clone();
    let claim_submit = tokio::spawn(async move {
        claim_ops
            .submit_turn(
                SubmitTurnRequest::for_child(
                    claim_parent.as_str(),
                    claim_target.as_str(),
                    "finish immediately",
                )
                .wait_for_result(false),
            )
            .await
    });
    claim_reached.await.unwrap();

    let claim_delete_scheduler = Arc::clone(&ops.scheduler);
    let claim_delete_target = claim_child_id.clone();
    let claim_delete = tokio::spawn(async move {
        claim_delete_scheduler
            .delete_session(&claim_delete_target)
            .await
    });
    tokio::task::yield_now().await;
    assert!(
        !claim_delete.is_finished(),
        "tree close must wait while completion drain owns the child operation"
    );
    let _ = release_claim.send(());
    claim_submit.await.unwrap().unwrap();
    claim_delete.await.unwrap().unwrap();

    let abort_child = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "abort-race-child".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let abort_child_id = SessionId::from(abort_child.session_id);
    let (abort_claim_reached, release_abort_claim) =
        pause_next_completion_guard_claim_for_test(&ops.child_sessions);
    let abort_submit_ops = Arc::clone(&ops);
    let abort_submit_parent = parent_id.clone();
    let abort_submit_target = abort_child_id.clone();
    let abort_submit = tokio::spawn(async move {
        abort_submit_ops
            .submit_turn(
                SubmitTurnRequest::for_child(
                    abort_submit_parent.as_str(),
                    abort_submit_target.as_str(),
                    "finish before parent abort",
                )
                .wait_for_result(false),
            )
            .await
    });
    abort_claim_reached.await.unwrap();
    let abort_scheduler = Arc::clone(&ops.scheduler);
    let abort_parent = parent_id.clone();
    let parent_abort = tokio::spawn(async move { abort_scheduler.abort(&abort_parent).await });
    tokio::task::yield_now().await;
    assert!(
        !parent_abort.is_finished(),
        "parent abort must wait for the drain that owns the child operation"
    );
    let _ = release_abort_claim.send(());
    abort_submit.await.unwrap().unwrap();
    parent_abort.await.unwrap().unwrap();

    let sync_child = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "sync-race-child".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let sync_child_id = SessionId::from(sync_child.session_id);
    let (sync_settled, release_sync) =
        pause_next_sync_completion_settled_for_test(&ops.child_sessions);
    let sync_ops = Arc::clone(&ops);
    let sync_parent = parent_id.clone();
    let sync_target = sync_child_id.clone();
    let sync_submit = tokio::spawn(async move {
        sync_ops
            .submit_turn(SubmitTurnRequest::for_child(
                sync_parent.as_str(),
                sync_target.as_str(),
                "finish synchronously",
            ))
            .await
    });
    sync_settled.await.unwrap();
    let sync_delete_scheduler = Arc::clone(&ops.scheduler);
    let sync_delete_target = sync_child_id.clone();
    let sync_delete = tokio::spawn(async move {
        sync_delete_scheduler
            .delete_session(&sync_delete_target)
            .await
    });
    tokio::task::yield_now().await;
    assert!(
        !sync_delete.is_finished(),
        "sync completion must hold the child operation through its parent terminal"
    );
    blocking_store.fail_next_sync_for(parent_id.clone()).await;
    let _ = release_sync.send(());
    let sync_error = sync_submit.await.unwrap().unwrap_err();
    assert!(
        sync_error
            .to_string()
            .contains("injected durable sync failure"),
        "sync completion should surface the required parent fsync failure: {sync_error}"
    );
    sync_delete.await.unwrap().unwrap();

    let parent_events = store.replay_events(&parent_id).await.unwrap();
    for child_id in [
        &registration_child_id,
        &claim_child_id,
        &abort_child_id,
        &sync_child_id,
    ] {
        let terminal_count = parent_events
            .iter()
            .filter(|event| {
                matches!(
                    &event.payload,
                    DurableEventPayload::AgentSessionCompleted {
                        child_session_id,
                        ..
                    } | DurableEventPayload::AgentSessionFailed {
                        child_session_id,
                        ..
                    } if child_session_id == child_id
                )
            })
            .count();
        assert_eq!(
            terminal_count, 1,
            "guard ownership must write exactly one parent terminal for {child_id}"
        );
    }
    for child_id in [&abort_child_id, &sync_child_id] {
        assert!(
            parent_events.iter().any(|event| {
                matches!(
                    &event.payload,
                    DurableEventPayload::AgentSessionCompleted {
                        child_session_id,
                        ..
                    } if child_session_id == child_id
                )
            }),
            "the winning completion owner should keep {child_id} completed"
        );
    }
}

#[tokio::test]
async fn cancelled_completion_drain_restores_claim_for_retry() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let parent_id = new_session_id();
    store
        .create_session(session_started_event_for_test(
            parent_id.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();
    let (gate_llm, release) = GateLlm::new_pair();
    let ops = build_test_ops_with_llm(Arc::clone(&store), Arc::new(gate_llm));
    ops.child_sessions.shutdown_completion_watcher().await;

    let child = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "cancelled-drain".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let child_id = SessionId::from(child.session_id);
    ops.submit_turn(
        SubmitTurnRequest::for_child(parent_id.as_str(), child_id.as_str(), "complete later")
            .wait_for_result(false),
    )
    .await
    .unwrap();
    release.notify_one();
    for _ in 0..100 {
        if ops.scheduler.registry().active_is_finished(&child_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ops.scheduler.registry().active_is_finished(&child_id));

    let (claim_reached, release_claim) =
        pause_next_completion_guard_claim_for_test(&ops.child_sessions);
    let drain_ops = Arc::clone(&ops);
    let drain_parent = parent_id.clone();
    let drain = tokio::spawn(async move {
        drain_ops
            .child_sessions
            .drain_completed(drain_ops.scheduler.as_ref(), &drain_parent)
            .await;
    });
    claim_reached.await.unwrap();
    drain.abort();
    assert!(drain.await.unwrap_err().is_cancelled());
    let _ = release_claim.send(());
    assert_eq!(
        registered_completion_guard_count_for_test(&ops.child_sessions, &parent_id),
        1,
        "claim Drop must restore the guard synchronously"
    );

    ops.child_sessions
        .drain_completed(ops.scheduler.as_ref(), &parent_id)
        .await;
    assert_eq!(
        registered_completion_guard_count_for_test(&ops.child_sessions, &parent_id),
        0
    );
    let terminal_count = store
        .replay_events(&parent_id)
        .await
        .unwrap()
        .iter()
        .filter(|event| {
            matches!(
                &event.payload,
                DurableEventPayload::AgentSessionCompleted {
                    child_session_id,
                    ..
                } if child_session_id == &child_id
            )
        })
        .count();
    assert_eq!(terminal_count, 1);
}

#[tokio::test]
async fn cascade_terminal_failure_restores_every_claimed_guard() {
    let blocking_store = Arc::new(BlockingChildCreateStore::new());
    let store: Arc<dyn SessionStore> = blocking_store.clone();
    let parent_id = new_session_id();
    store
        .create_session(session_started_event_for_test(
            parent_id.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();
    let (gate_llm, _release) = GateLlm::new_pair();
    let ops = build_test_ops_with_llm(Arc::clone(&store), Arc::new(gate_llm));

    let mut child_ids = Vec::new();
    for name in ["first-guarded-child", "second-guarded-child"] {
        let child = ops
            .create_session(
                parent_id.as_str(),
                CreateSessionRequest {
                    name: name.into(),
                    source_extension: Some("test".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let child_id = SessionId::from(child.session_id);
        ops.submit_turn(
            SubmitTurnRequest::for_child(parent_id.as_str(), child_id.as_str(), "wait for abort")
                .wait_for_result(false),
        )
        .await
        .unwrap();
        child_ids.push(child_id);
    }
    for child_id in &child_ids {
        for _ in 0..50 {
            if ops.scheduler.registry().has_active(child_id) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(ops.scheduler.registry().has_active(child_id));
    }

    blocking_store.fail_next_sync_for(parent_id.clone()).await;
    let abort_error = ops.scheduler.abort(&parent_id).await.unwrap_err();
    assert!(
        abort_error
            .to_string()
            .contains("injected durable sync failure"),
        "cascade should surface the required terminal sync failure: {abort_error}"
    );
    assert_eq!(
        blocking_store
            .session_read_model(&parent_id)
            .await
            .unwrap()
            .agent_sessions
            .len(),
        2,
        "terminal failure must not lose the current or remaining guard owners"
    );

    ops.scheduler.abort(&parent_id).await.unwrap();
    assert!(
        blocking_store
            .session_read_model(&parent_id)
            .await
            .unwrap()
            .agent_sessions
            .is_empty(),
        "retry should reconcile the uncertain terminal and recycle every child"
    );
}

#[tokio::test]
async fn inject_message_during_active_turn_binds_turn_id() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let parent_id = new_session_id();
    store
        .create_session(session_started_event_for_test(
            parent_id.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();

    let (gate_llm, release) = GateLlm::new_pair();
    let ops = build_test_ops_with_llm(Arc::clone(&store), Arc::new(gate_llm));

    let handle = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "inject-child".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let child_id = SessionId::from(handle.session_id.as_str());

    let _bg = ops
        .submit_turn(
            SubmitTurnRequest::for_child(parent_id.as_str(), &handle.session_id, "start turn")
                .wait_for_result(false),
        )
        .await
        .unwrap();

    for _ in 0..50 {
        if ops.scheduler.registry().has_active(&child_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        ops.scheduler.registry().has_active(&child_id),
        "child turn should be active before inject"
    );

    let outcome = ops
        .inject_message(
            SessionAccess::new(parent_id.as_str(), child_id.as_str()),
            "mid-turn inject".into(),
        )
        .await
        .unwrap();

    let events = store.replay_events(&child_id).await.unwrap();
    let injected = events
        .iter()
        .find(|e| {
            matches!(
                &e.payload,
                DurableEventPayload::UserMessage { text, .. } if text == "mid-turn inject"
            )
        })
        .expect("injected UserMessage must be durable");
    assert!(
        injected.turn_id.is_some(),
        "active-turn inject must bind turn_id (same as TurnScheduler::inject)"
    );
    assert_eq!(
        outcome,
        SessionDeliveryOutcome::Injected {
            turn_id: injected.turn_id.as_ref().unwrap().to_string(),
        }
    );

    release.notify_one();
    for _ in 0..100 {
        if !ops.scheduler.registry().has_active(&child_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn inject_message_when_idle_starts_turn() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let session_id = new_session_id();
    store
        .create_session(session_started_event_for_test(
            session_id.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();

    let (gate_llm, release) = GateLlm::new_pair();
    let ops = build_test_ops_with_llm(Arc::clone(&store), Arc::new(gate_llm));

    assert!(
        !ops.scheduler.registry().has_active(&session_id),
        "session should start idle"
    );

    let outcome = ops
        .inject_message(
            SessionAccess::same(session_id.as_str()),
            "mid-turn injected user message".into(),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, SessionDeliveryOutcome::Started { .. }));

    for _ in 0..50 {
        if ops.scheduler.registry().has_active(&session_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        ops.scheduler.registry().has_active(&session_id),
        "idle inject must start a turn"
    );

    release.notify_one();
    for _ in 0..100 {
        if !ops.scheduler.registry().has_active(&session_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn inject_message_after_turn_task_finished_starts_new_turn() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let session_id = new_session_id();
    store
        .create_session(session_started_event_for_test(
            session_id.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();

    let ops = build_test_ops(Arc::clone(&store), "handled notification");

    let started = start_with_completion_for_test(
        ops.scheduler.as_ref(),
        session_id.clone(),
        "initial".into(),
    )
    .await
    .unwrap();
    let first_turn_id = started.turn_id.clone();
    let result = started.handle.wait().await.unwrap();
    assert!(result.output.is_ok(), "{:?}", result.output);
    assert!(
        ops.scheduler.registry().has_active(&session_id),
        "start_with_completion caller has not finalized registry cleanup yet"
    );

    for _ in 0..50 {
        if ops.scheduler.registry().active_is_finished(&session_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        ops.scheduler.registry().active_is_finished(&session_id),
        "test requires a finished task that is still registered active"
    );

    ops.inject_message(
        SessionAccess::same(session_id.as_str()),
        "late injected user message".into(),
    )
    .await
    .unwrap();
    assert!(
        finish_and_watch_next_for_test(
            ops.scheduler.as_ref(),
            &session_id,
            &first_turn_id,
            Some(&result.finalization),
        )
        .await
        .unwrap(),
        "the original completion owner should settle the turn"
    );

    for _ in 0..100 {
        let events = store.replay_events(&session_id).await.unwrap();
        let injected = events.iter().find(|event| {
            matches!(
                &event.payload,
                DurableEventPayload::UserMessage { text, .. }
                    if text.contains("late injected user message")
            )
        });
        if injected.is_some_and(|event| event.turn_id.as_ref() != Some(&first_turn_id)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let events = store.replay_events(&session_id).await.unwrap();
    let injected = events
        .iter()
        .find(|event| {
            matches!(
                &event.payload,
                DurableEventPayload::UserMessage { text, .. }
                    if text.contains("late injected user message")
            )
        })
        .expect("late injected user message should be written");
    assert_ne!(
        injected.turn_id.as_ref(),
        Some(&first_turn_id),
        "late injected user message must start a fresh turn, not attach to the completed one"
    );
}

#[tokio::test]
async fn submit_turn_sync_returns_llm_output() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let parent_id = new_session_id();
    store
        .create_session(session_started_event_for_test(
            parent_id.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();

    let ops = build_test_ops(Arc::clone(&store), "hello from child");

    let handle = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "test-child".into(),
                system_prompt: Some("be helpful".into()),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let result = ops
        .submit_turn(SubmitTurnRequest::for_child(
            parent_id.as_str(),
            &handle.session_id,
            "say hello",
        ))
        .await
        .unwrap();

    match result {
        SubmitTurnResult::Completed { content } => {
            assert_eq!(content, "hello from child");
        },
        SubmitTurnResult::Backgrounded { .. } => {
            panic!("expected Completed, got Backgrounded");
        },
    }

    // 父 session 应有 AgentSessionCompleted 事件
    let parent_model = store.session_read_model(&parent_id).await.unwrap();
    assert_eq!(parent_model.agent_sessions.len(), 1);
    assert_eq!(
        parent_model.agent_sessions[0].status,
        AgentSessionStatus::Completed
    );
}

#[tokio::test]
async fn submit_turn_async_returns_backgrounded_and_completes() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let parent_id = new_session_id();
    store
        .create_session(session_started_event_for_test(
            parent_id.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();

    let ops = build_test_ops(Arc::clone(&store), "async result");

    let handle = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "async-child".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let result = ops
        .submit_turn(
            SubmitTurnRequest::for_child(parent_id.as_str(), &handle.session_id, "do async work")
                .wait_for_result(false)
                .notify_parent_on_complete(Some("[done]".into())),
        )
        .await
        .unwrap();

    match &result {
        SubmitTurnResult::Backgrounded {
            task_id,
            session_id,
        } => {
            assert!(!task_id.is_empty());
            assert_eq!(session_id, &handle.session_id);
        },
        SubmitTurnResult::Completed { .. } => {
            panic!("expected Backgrounded, got Completed");
        },
    }

    // 给后台任务完成；completion watcher 会自动 drain，无需手动调用
    for _ in 0..100 {
        let parent_model = store.session_read_model(&parent_id).await.unwrap();
        if parent_model.agent_sessions.len() == 1
            && parent_model.agent_sessions[0].status == AgentSessionStatus::Completed
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 父 session 应有 AgentSessionCompleted
    let parent_model = store.session_read_model(&parent_id).await.unwrap();
    assert_eq!(parent_model.agent_sessions.len(), 1);
    assert_eq!(
        parent_model.agent_sessions[0].status,
        AgentSessionStatus::Completed
    );

    // notify_parent_on_complete 消息应存在，且包含子 agent 输出
    let has_notify = parent_model.transcript.messages.iter().any(|m| {
        m.message.content.iter().any(|c| {
            matches!(
                c,
                astrcode_core::llm::LlmContent::Text { text }
                    if text.contains("<background-agent-notification>")
                        && text.contains("async result")
            )
        })
    });
    assert!(
        has_notify,
        "notify_parent_on_complete should inject agent output in notification"
    );
}

#[tokio::test]
async fn submit_turn_async_recycle_on_complete_drains_without_manual_call() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let parent_id = new_session_id();
    store
        .create_session(session_started_event_for_test(
            parent_id.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();

    let ops = build_test_ops(Arc::clone(&store), "recycled child output");

    let handle = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "recycle-child".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let child_id = SessionId::from(handle.session_id.as_str());

    let result = ops
        .submit_turn(
            SubmitTurnRequest::for_child(
                parent_id.as_str(),
                &handle.session_id,
                "work then recycle",
            )
            .wait_for_result(false)
            .recycle_on_complete(true),
        )
        .await
        .unwrap();

    assert!(
        matches!(result, SubmitTurnResult::Backgrounded { .. }),
        "expected Backgrounded"
    );

    for _ in 0..100 {
        let parent_events = store.replay_events(&parent_id).await.unwrap();
        let completed = parent_events.iter().any(|e| {
            matches!(
                &e.payload,
                DurableEventPayload::AgentSessionCompleted {
                    child_session_id: ref sid,
                    ..
                } if sid == &child_id
            )
        });
        let recycled = parent_events.iter().any(|e| {
            matches!(
                &e.payload,
                DurableEventPayload::AgentSessionRecycled {
                    child_session_id: ref sid,
                } if sid == &child_id
            )
        });
        if completed && recycled {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let parent_events = store.replay_events(&parent_id).await.unwrap();
    assert!(
        parent_events.iter().any(|e| {
            matches!(
                &e.payload,
                DurableEventPayload::AgentSessionCompleted {
                    child_session_id: ref sid,
                    ..
                } if sid == &child_id
            )
        }),
        "completion watcher should write AgentSessionCompleted"
    );
    assert!(
        parent_events.iter().any(|e| {
            matches!(
                &e.payload,
                DurableEventPayload::AgentSessionRecycled {
                    child_session_id: ref sid,
                } if sid == &child_id
            )
        }),
        "completion watcher should recycle child session"
    );
    assert_eq!(
        parent_events
            .iter()
            .filter(|event| {
                matches!(
                    &event.payload,
                    DurableEventPayload::AgentSessionRecycled {
                        child_session_id: sid,
                    } if sid == &child_id
                )
            })
            .count(),
        1,
        "tree close should own the recycled event"
    );
    assert!(
        !ops.scheduler.registry().has_active(&child_id),
        "recycle must release registry without leaving a stale active entry"
    );
}

#[tokio::test]
async fn parent_abort_stops_sync_child_and_recycles() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
    let parent_id = new_session_id();
    store
        .create_session(session_started_event_for_test(
            parent_id.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();

    let (gate_llm, release) = GateLlm::new_pair();
    let ops = build_test_ops_with_llm(Arc::clone(&store), Arc::new(gate_llm));

    let handle = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "sync-child".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let child_id = SessionId::from(handle.session_id.as_str());

    let ops_for_turn = Arc::clone(&ops);
    let parent_for_turn = parent_id.clone();
    let child_target = handle.session_id.clone();
    let sync_turn = tokio::spawn(async move {
        ops_for_turn
            .submit_turn(SubmitTurnRequest::for_child(
                parent_for_turn.as_str(),
                child_target,
                "sync work",
            ))
            .await
    });

    for _ in 0..100 {
        if ops.scheduler.registry().has_active(&child_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        ops.scheduler.registry().has_active(&child_id),
        "sync child turn should be active in registry"
    );

    assert!(
        ops.scheduler.abort(&parent_id).await.unwrap(),
        "cancelling an idle parent must report its active descendant"
    );
    release.notify_one();

    let sync_result = sync_turn.await.expect("sync turn task panicked");
    assert!(
        sync_result.is_err(),
        "sync child turn should fail after parent cascade abort"
    );

    for _ in 0..100 {
        if !ops.scheduler.registry().has_active(&child_id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !ops.scheduler.registry().has_active(&child_id),
        "child registry entry must be cleared after cascade abort"
    );

    let parent_events = store.replay_events(&parent_id).await.unwrap();
    assert!(
        parent_events.iter().any(|e| {
            matches!(
                &e.payload,
                DurableEventPayload::AgentSessionFailed {
                    child_session_id: ref sid,
                    ..
                } if sid == &child_id
            )
        }),
        "cascade abort should mark sync child as failed on parent"
    );
    assert!(
        parent_events.iter().any(|e| {
            matches!(
                &e.payload,
                DurableEventPayload::AgentSessionRecycled {
                    child_session_id: ref sid,
                } if sid == &child_id
            )
        }),
        "cascade abort should recycle unguarded sync child session"
    );
    assert!(
        store
            .session_read_model(&parent_id)
            .await
            .unwrap()
            .agent_sessions
            .is_empty(),
        "recycled child should be removed from parent agent_sessions projection"
    );
}

#[tokio::test]
async fn shutdown_drains_active_background_and_sync_child_terminals() {
    let blocking_store = Arc::new(BlockingChildCreateStore::new());
    let store: Arc<dyn SessionStore> = blocking_store.clone();
    let parent_id = new_session_id();
    store
        .create_session(session_started_event_for_test(
            parent_id.clone(),
            ".",
            "mock",
        ))
        .await
        .unwrap();
    let (gate_llm, release) = GateLlm::new_pair();
    let ops = build_test_ops_with_llm(Arc::clone(&store), Arc::new(gate_llm));

    let background_child = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "shutdown-background".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let sync_child = ops
        .create_session(
            parent_id.as_str(),
            CreateSessionRequest {
                name: "shutdown-sync".into(),
                source_extension: Some("test".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let background_id = SessionId::from(background_child.session_id.as_str());
    let sync_id = SessionId::from(sync_child.session_id.as_str());

    let background = ops
        .submit_turn(
            SubmitTurnRequest::for_child(
                parent_id.as_str(),
                &background_child.session_id,
                "background work",
            )
            .wait_for_result(false),
        )
        .await
        .unwrap();
    assert!(matches!(background, SubmitTurnResult::Backgrounded { .. }));

    let sync_ops = Arc::clone(&ops);
    let sync_parent = parent_id.clone();
    let sync_target = sync_child.session_id.clone();
    let sync_request = tokio::spawn(async move {
        sync_ops
            .submit_turn(SubmitTurnRequest::for_child(
                sync_parent.as_str(),
                sync_target,
                "sync work",
            ))
            .await
    });
    for _ in 0..100 {
        if ops.scheduler.registry().has_active(&background_id)
            && ops.scheduler.registry().has_active(&sync_id)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ops.scheduler.registry().has_active(&background_id));
    assert!(ops.scheduler.registry().has_active(&sync_id));

    let (claim_reached, release_claim) =
        pause_next_completion_guard_claim_for_test(&ops.child_sessions);
    let shutdown_scheduler = Arc::clone(&ops.scheduler);
    let shutdown =
        tokio::spawn(async move { shutdown_scheduler.shutdown_background_tasks().await });
    tokio::time::timeout(Duration::from_secs(5), claim_reached)
        .await
        .expect("shutdown should reach child terminal persistence")
        .unwrap();
    blocking_store.fail_next_sync_for(parent_id.clone()).await;
    let _ = release_claim.send(());
    tokio::time::timeout(Duration::from_secs(6), shutdown)
        .await
        .expect("shutdown should retry and durably drain child terminals")
        .unwrap();
    release.notify_waiters();

    assert!(
        sync_request
            .await
            .expect("sync request task should not panic")
            .is_err(),
        "forced shutdown should resolve the sync waiter with a terminal error"
    );
    assert_eq!(
        registered_completion_guard_count_for_test(&ops.child_sessions, &parent_id),
        0
    );
    let parent = store.session_read_model(&parent_id).await.unwrap();
    assert_eq!(parent.agent_sessions.len(), 2);
    assert!(
        parent
            .agent_sessions
            .iter()
            .all(|link| link.status == AgentSessionStatus::Failed)
    );
}
