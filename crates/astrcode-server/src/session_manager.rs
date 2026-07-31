use std::{collections::HashMap, future::Future, panic::AssertUnwindSafe, sync::Arc};

use astrcode_core::{
    event::{DurableEventPayload, Event, PersistedSystemPrompt},
    llm::LlmMessage,
    tool::SessionToolSelection,
    types::{Cursor, SessionId, TurnId},
};
use astrcode_extension_sdk::extension::ExtensionEvent;
use astrcode_session::{
    Session, SessionCreateParams, SessionCreationFailed, SessionError, SessionEventObserver,
    SessionEventSink, SessionRuntimeServices, SessionRuntimeState, emit_lifecycle_for_read_model,
};
use astrcode_session_projection::{AgentSessionLinkView, SessionReadModel, SessionSummary};
use astrcode_storage::{SessionStore, StorageError};
use futures_util::FutureExt;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{
    server_event_bus::ServerEventBus, session_resource_cleanup::SessionResourceCleanup,
    task_utils::OwnedTaskSet,
};

struct ForkCreationInput {
    source_id: SessionId,
    session_id: SessionId,
    working_dir: String,
    model_id: String,
    initial_system_prompt: PersistedSystemPrompt,
    source_cursor: Cursor,
    first_user_message: Option<String>,
    messages: Vec<LlmMessage>,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionManagerError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Extension(#[from] astrcode_extension_sdk::extension::ExtensionError),
    #[error(transparent)]
    Projection(#[from] astrcode_session_projection::ProjectionError),
    #[error(transparent)]
    Creation(#[from] SessionCreationFailed),
    #[error("invalid fork cursor: {0}")]
    InvalidCursor(String),
    #[error("session close task failed: {0}")]
    CloseTask(String),
    #[error("session creation task failed: {0}")]
    CreationTask(String),
}

/// Session durable 生命周期门面（create/open/delete/fork）与 per-session runtime 唯一性。
///
/// 不处理 active turn、输入队列或 child completion——那些由 [`crate::turn_scheduler`]
/// 与 [`crate::child_session`] 负责。
#[derive(Clone)]
pub struct SessionManager {
    event_store: Arc<dyn SessionStore>,
    transitions: Arc<SessionTransitions>,
    owned_tasks: Arc<OwnedTaskSet>,
    runtime_services: Arc<SessionRuntimeServices>,
    event_bus: Arc<ServerEventBus>,
    event_sink: Arc<SessionEventSink>,
    resource_cleanups: Vec<Arc<dyn SessionResourceCleanup>>,
}

impl SessionManager {
    // ─── 生命周期 ─────────────────────────────────────────────────────

    pub fn new(
        event_store: Arc<dyn SessionStore>,
        runtime_services: Arc<SessionRuntimeServices>,
        resource_cleanups: Vec<Arc<dyn SessionResourceCleanup>>,
    ) -> Self {
        let event_bus = Arc::new(ServerEventBus::new());
        let observer: Arc<dyn SessionEventObserver> = event_bus.clone();
        Self {
            event_store,
            transitions: Arc::new(SessionTransitions::default()),
            owned_tasks: OwnedTaskSet::new(),
            runtime_services,
            event_bus,
            event_sink: Arc::new(SessionEventSink::new(observer)),
            resource_cleanups,
        }
    }

    pub(crate) fn event_bus(&self) -> &Arc<ServerEventBus> {
        &self.event_bus
    }

    fn runtime_for(&self, session_id: &SessionId) -> Arc<SessionRuntimeState> {
        self.runtime_services
            .session_resources()
            .resources_for(session_id, || {
                Arc::new(SessionRuntimeState::new_with_event_sink(
                    session_id.clone(),
                    self.event_store.clone(),
                    Arc::clone(&self.event_sink),
                ))
            })
    }

    pub(crate) fn max_agent_depth(&self) -> usize {
        self.runtime_services.read_effective().agent.max_depth
    }

    pub(crate) fn spawn_creation_task<F>(
        &self,
        task: F,
    ) -> Result<tokio::task::JoinHandle<F::Output>, SessionManagerError>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.owned_tasks.spawn(task).map_err(|error| {
            SessionManagerError::CreationTask(format!("session manager is shutting down: {error}"))
        })
    }

    pub(crate) fn owned_tasks(&self) -> &Arc<OwnedTaskSet> {
        &self.owned_tasks
    }

    pub(crate) async fn configure_session_tools(
        &self,
        session: &Session,
        selection: SessionToolSelection,
    ) -> Result<SessionToolSelection, SessionError> {
        let effective = session.configure_tools(selection).await?;
        self.sync_durable_events(session.id()).await;
        Ok(effective)
    }

    pub(crate) async fn create(&self, working_dir: &str) -> Result<Session, SessionManagerError> {
        self.create_with_tool_selection(working_dir, None).await
    }

    pub(crate) async fn create_with_tool_selection(
        &self,
        working_dir: &str,
        tool_selection: Option<&SessionToolSelection>,
    ) -> Result<Session, SessionManagerError> {
        let manager = self.clone();
        let working_dir = working_dir.to_owned();
        let tool_selection = tool_selection.cloned();
        let task = self.spawn_creation_task(async move {
            let sid = astrcode_core::types::new_session_id();
            match AssertUnwindSafe(manager.create_root_transaction(
                sid.clone(),
                working_dir,
                tool_selection,
            ))
            .catch_unwind()
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    manager
                        .compensate_panicked_creation(&sid, "root session")
                        .await;
                    Err(SessionManagerError::CreationTask(
                        "root session creation transaction panicked".into(),
                    ))
                },
            }
        })?;
        task.await.map_err(|error| {
            SessionManagerError::CreationTask(format!(
                "root session creation transaction stopped: {error}"
            ))
        })?
    }

    async fn create_root_transaction(
        &self,
        sid: SessionId,
        working_dir: String,
        tool_selection: Option<SessionToolSelection>,
    ) -> Result<Session, SessionManagerError> {
        let runtime = self.runtime_for(&sid);
        let creation = runtime.begin_creation();
        let publication = self
            .event_sink
            .defer_publication(sid.clone())
            .map_err(SessionError::from)?;
        let session = match Session::create_with_params(SessionCreateParams {
            working_dir,
            model_id: self.runtime_services.read_effective().llm.model_id.clone(),
            parent_session_id: None,
            tool_selection,
            source_extension: None,
            extra_system_prompt: None,
            initial_system_prompt: None,
            runtime,
            runtime_services: Arc::clone(&self.runtime_services),
        })
        .await
        {
            Ok(session) => session,
            Err(error) => {
                if matches!(&error, SessionError::EventPublish(_)) {
                    if let Err(compensation_error) = self.discard_failed_creation(&sid).await {
                        tracing::warn!(
                            session_id = %sid,
                            error = %error,
                            compensation_error = %compensation_error,
                            "failed to fully compensate root session creation"
                        );
                    }
                } else {
                    self.runtime_services.session_resources().cleanup(&sid);
                }
                return Err(error.into());
            },
        };

        if let Err(error) = session
            .ensure_lifecycle_initialized(ExtensionEvent::SessionStart)
            .await
        {
            if let Err(compensation_error) = self.discard_failed_lifecycle_start(&session).await {
                tracing::warn!(
                    session_id = %sid,
                    error = %error,
                    compensation_error = %compensation_error,
                    "failed to fully compensate root session creation"
                );
            }
            return Err(error.into());
        }

        if let Err(error) = self.sync_durable_events_required(&sid).await {
            if let Err(compensation_error) = self.discard_failed_lifecycle_start(&session).await {
                tracing::warn!(
                    session_id = %sid,
                    error = %error,
                    compensation_error = %compensation_error,
                    "failed to fully compensate root session creation"
                );
            }
            return Err(error);
        }

        creation.commit();
        publication.commit();
        Ok(session)
    }

    pub(crate) async fn open(&self, session_id: SessionId) -> Result<Session, SessionManagerError> {
        let runtime = self.runtime_for(&session_id);
        runtime.wait_for_creation().await?;
        let result = async {
            loop {
                match self.transitions.begin_open(&session_id) {
                    TransitionStart::Waiting(pending) => {
                        pending.wait().await;
                    },
                    TransitionStart::Started(pending) => {
                        let opening = SessionTransitionGuard::new(
                            Arc::clone(&self.transitions),
                            session_id.clone(),
                            pending,
                        );
                        let session =
                            Session::open(Arc::clone(&runtime), Arc::clone(&self.runtime_services))
                                .await?;
                        self.event_sink
                            .activate(&session_id)
                            .map_err(SessionError::from)?;
                        session
                            .ensure_lifecycle_initialized(ExtensionEvent::SessionResume)
                            .await?;
                        opening.complete();
                        return Ok(session);
                    },
                }
            }
        }
        .await;
        if result.is_err() {
            self.runtime_services
                .session_resources()
                .cleanup_if_unshared(&session_id, &runtime);
        }
        result
    }

    pub(crate) async fn delete(&self, session_id: &SessionId) -> Result<(), SessionManagerError> {
        self.close_session(session_id, CloseSessionAction::Delete)
            .await
    }

    async fn emit_session_shutdown(
        &self,
        session_id: &SessionId,
    ) -> Result<(), SessionManagerError> {
        let model = self.event_store.session_read_model(session_id).await?;
        emit_lifecycle_for_read_model(
            &self.runtime_services,
            session_id,
            &model,
            ExtensionEvent::SessionShutdown,
        )
        .await
        .map_err(SessionManagerError::from)
    }

    // ─── 只读查询 ─────────────────────────────────────────────────────

    pub(crate) async fn read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<SessionReadModel>, SessionManagerError> {
        self.event_store
            .session_read_model(session_id)
            .await
            .map_err(SessionManagerError::from)
    }

    pub(crate) async fn read_recycled_model(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<SessionReadModel>, SessionManagerError> {
        self.event_store
            .recycled_session_read_model(session_id)
            .await
            .map_err(SessionManagerError::from)
    }

    pub(crate) async fn has_durable_turn_completion(
        &self,
        session_id: &SessionId,
        turn_id: &TurnId,
    ) -> Result<bool, SessionManagerError> {
        let events = self.event_store.replay_events(session_id).await?;
        Ok(events.iter().any(|event| {
            event.turn_id.as_ref() == Some(turn_id)
                && matches!(event.payload, DurableEventPayload::TurnCompleted { .. })
        }))
    }

    pub(crate) async fn has_messages(
        &self,
        session_id: &SessionId,
    ) -> Result<bool, SessionManagerError> {
        self.event_store
            .session_has_messages(session_id)
            .await
            .map_err(SessionManagerError::from)
    }

    pub(crate) async fn agent_sessions(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<AgentSessionLinkView>, SessionManagerError> {
        self.event_store
            .session_agent_sessions(session_id)
            .await
            .map_err(SessionManagerError::from)
    }

    pub(crate) async fn list_summaries(&self) -> Result<Vec<SessionSummary>, SessionManagerError> {
        self.event_store
            .list_session_summaries()
            .await
            .map_err(SessionManagerError::from)
    }

    pub(crate) async fn replay_from_limited(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
        max_events: usize,
    ) -> Result<Vec<Event>, SessionManagerError> {
        self.event_store
            .replay_from_limited(session_id, cursor, max_events)
            .await
            .map(|events| events.into_iter().map(Event::from).collect())
            .map_err(SessionManagerError::from)
    }

    pub(crate) async fn latest_cursor(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Cursor>, SessionManagerError> {
        self.event_store
            .latest_cursor(session_id)
            .await
            .map_err(SessionManagerError::from)
    }

    pub(crate) async fn session_store_dir(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<std::path::PathBuf>, SessionManagerError> {
        self.event_store
            .session_store_dir(session_id)
            .await
            .map_err(SessionManagerError::from)
    }

    /// 强制 fsync 指定会话的 durable event log。
    pub(crate) async fn sync_durable_events(&self, session_id: &SessionId) {
        if let Err(e) = self.sync_durable_events_required(session_id).await {
            tracing::error!(session_id = %session_id, error = %e, "failed to sync durable events");
        }
    }

    pub(crate) async fn sync_durable_events_required(
        &self,
        session_id: &SessionId,
    ) -> Result<(), SessionManagerError> {
        self.event_sink
            .sync(self.event_store.clone(), session_id)
            .await
            .map_err(SessionError::from)?;
        Ok(())
    }

    pub(crate) async fn shutdown_event_sink(&self) {
        self.event_sink.shutdown().await;
    }

    pub(crate) async fn recycle_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), SessionManagerError> {
        self.close_session(session_id, CloseSessionAction::Recycle)
            .await
    }

    async fn close_session(
        &self,
        session_id: &SessionId,
        action: CloseSessionAction,
    ) -> Result<(), SessionManagerError> {
        let closing = self.begin_session_transition(session_id).await;
        self.emit_session_shutdown(session_id).await?;
        let event_store = Arc::clone(&self.event_store);
        let event_bus = Arc::clone(&self.event_bus);
        let event_sink = Arc::clone(&self.event_sink);
        let resource_cleanups = self.resource_cleanups.clone();
        let session_resources = self.runtime_services.session_resources().clone();
        let session_id = session_id.clone();

        tokio::spawn(async move {
            let _closing = closing;
            let result = async {
                event_sink
                    .release(event_store.as_ref(), &session_id)
                    .await
                    .map_err(SessionError::from)?;
                match action {
                    CloseSessionAction::Delete => event_store.delete_session(&session_id).await?,
                    CloseSessionAction::Recycle => event_store.recycle_session(&session_id).await?,
                }
                event_bus.detach(&session_id);
                session_resources.cleanup(&session_id);
                for cleanup in resource_cleanups {
                    cleanup.cleanup(&session_id);
                }
                Ok::<_, SessionManagerError>(())
            }
            .await;
            if let Err(error) = &result {
                tracing::error!(%session_id, %error, "session close task failed");
            }
            result
        })
        .await
        .map_err(|error| SessionManagerError::CloseTask(error.to_string()))?
    }

    pub(crate) async fn begin_session_transition(
        &self,
        session_id: &SessionId,
    ) -> SessionTransitionGuard {
        loop {
            match self.transitions.begin_close(session_id) {
                TransitionStart::Waiting(pending) => pending.wait().await,
                TransitionStart::Started(pending) => {
                    return SessionTransitionGuard::new(
                        Arc::clone(&self.transitions),
                        session_id.clone(),
                        pending,
                    );
                },
            }
        }
    }

    /// 从 .recycled/ 恢复一个已回收的 session。
    pub(crate) async fn restore_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), SessionManagerError> {
        let transition = self.begin_session_transition(session_id).await;
        self.restore_session_in_transition(&transition).await
    }

    pub(crate) async fn restore_session_in_transition(
        &self,
        transition: &SessionTransitionGuard,
    ) -> Result<(), SessionManagerError> {
        self.event_store
            .restore_session(transition.session_id())
            .await
            .map_err(SessionManagerError::from)
    }

    /// Fork 一个已有会话，创建新 session 并复制 fork 点之前的消息前缀。
    ///
    /// fork 保证新 session 发送给 LLM 的 system prompt + 消息前缀与源 session 完全一致，
    /// 从而让 provider 侧的 KV 缓存（prompt cache）自动命中。
    ///
    /// - `source_id`: 源会话 ID
    /// - `at_cursor`: 可选 fork 点 cursor（event seq 的十进制字符串），为 None 则从末尾 fork
    ///
    /// 返回新 session 及其初始事件。
    pub(crate) async fn fork(
        &self,
        source_id: &SessionId,
        at_cursor: Option<&Cursor>,
    ) -> Result<Session, SessionManagerError> {
        let source_model = self.event_store.session_read_model(source_id).await?;

        let fork_cursor = at_cursor.cloned().unwrap_or_else(|| source_model.cursor());

        let (transcript_messages, first_user_message) = if at_cursor.is_some() {
            let events = self.event_store.replay_events(source_id).await?;
            let truncated_seq: u64 = fork_cursor
                .parse()
                .map_err(|_| SessionManagerError::InvalidCursor(fork_cursor.clone()))?;
            let truncated_events: Vec<_> = events
                .into_iter()
                .filter(|event| event.seq <= truncated_seq)
                .collect();
            let truncated_model =
                astrcode_session_projection::replay(source_id.clone(), &truncated_events)?;
            let first_user_message = truncated_model.first_user_message().map(str::to_owned);
            (truncated_model.transcript.messages, first_user_message)
        } else {
            (
                source_model.transcript.messages.clone(),
                source_model.first_user_message().map(str::to_owned),
            )
        };

        let input = ForkCreationInput {
            source_id: source_id.clone(),
            session_id: astrcode_core::types::new_session_id(),
            working_dir: source_model.identity.working_dir.clone(),
            model_id: source_model.identity.model_id.clone(),
            initial_system_prompt: PersistedSystemPrompt {
                text: source_model.system_prompt.text.clone(),
                fingerprint: source_model.system_prompt.fingerprint.clone(),
                extra_system_prompt: source_model.system_prompt.extra.clone(),
                source: astrcode_core::event::SystemPromptSource::Inherited,
            },
            source_cursor: fork_cursor,
            first_user_message,
            messages: transcript_messages
                .into_iter()
                .map(|message| message.message)
                .collect(),
        };
        let new_sid = input.session_id.clone();
        let manager = self.clone();
        let task = self.spawn_creation_task(async move {
            match AssertUnwindSafe(manager.create_fork_transaction(input))
                .catch_unwind()
                .await
            {
                Ok(result) => result,
                Err(_) => {
                    manager
                        .compensate_panicked_creation(&new_sid, "fork session")
                        .await;
                    Err(SessionManagerError::CreationTask(
                        "fork session creation transaction panicked".into(),
                    ))
                },
            }
        })?;
        task.await.map_err(|error| {
            SessionManagerError::CreationTask(format!(
                "fork session creation transaction stopped: {error}"
            ))
        })?
    }

    async fn create_fork_transaction(
        &self,
        input: ForkCreationInput,
    ) -> Result<Session, SessionManagerError> {
        let ForkCreationInput {
            source_id,
            session_id: new_sid,
            working_dir,
            model_id,
            initial_system_prompt,
            source_cursor,
            first_user_message,
            messages,
        } = input;
        let runtime = self.runtime_for(&new_sid);
        let creation = runtime.begin_creation();
        let publication = self
            .event_sink
            .defer_publication(new_sid.clone())
            .map_err(SessionError::from)?;
        let session = match Session::create_with_params(SessionCreateParams {
            working_dir,
            model_id,
            parent_session_id: None,
            tool_selection: None,
            source_extension: None,
            extra_system_prompt: None,
            initial_system_prompt: Some(initial_system_prompt),
            runtime,
            runtime_services: Arc::clone(&self.runtime_services),
        })
        .await
        {
            Ok(session) => session,
            Err(error) => {
                if matches!(&error, SessionError::EventPublish(_)) {
                    if let Err(compensation_error) = self.discard_failed_creation(&new_sid).await {
                        tracing::warn!(
                            source_session_id = %source_id,
                            fork_session_id = %new_sid,
                            error = %error,
                            compensation_error = %compensation_error,
                            "failed to fully compensate fork session creation"
                        );
                    }
                } else {
                    self.runtime_services.session_resources().cleanup(&new_sid);
                }
                return Err(error.into());
            },
        };

        if let Err(error) = session
            .emit_durable(
                None,
                DurableEventPayload::SessionForked {
                    source_session_id: source_id.clone(),
                    source_cursor,
                    first_user_message,
                    messages,
                },
            )
            .await
        {
            self.compensate_failed_fork_creation(
                &source_id,
                &session,
                &error,
                FailedForkCreationStage::Persisted,
            )
            .await;
            return Err(error.into());
        }

        if let Err(error) = session
            .ensure_lifecycle_initialized(ExtensionEvent::SessionStart)
            .await
        {
            self.compensate_failed_fork_creation(
                &source_id,
                &session,
                &error,
                FailedForkCreationStage::LifecycleStartFailed,
            )
            .await;
            return Err(error.into());
        }

        if let Err(error) = self.sync_durable_events_required(&new_sid).await {
            if let Err(compensation_error) = self.discard_failed_lifecycle_start(&session).await {
                tracing::warn!(
                    source_session_id = %source_id,
                    fork_session_id = %new_sid,
                    error = %error,
                    compensation_error = %compensation_error,
                    "failed to fully compensate fork session creation"
                );
            }
            return Err(error);
        }

        creation.commit();
        publication.commit();
        Ok(session)
    }

    async fn compensate_failed_fork_creation(
        &self,
        source_session_id: &SessionId,
        fork: &Session,
        cause: &SessionError,
        stage: FailedForkCreationStage,
    ) {
        let compensation_result = match stage {
            FailedForkCreationStage::Persisted => self
                .discard_failed_creation(fork.id())
                .await
                .map_err(|error| format!("discard fork session: {error}")),
            FailedForkCreationStage::LifecycleStartFailed => {
                self.discard_failed_lifecycle_start(fork).await
            },
        };
        if let Err(compensation_error) = compensation_result {
            tracing::warn!(
                source_session_id = %source_session_id,
                fork_session_id = %fork.id(),
                error = %cause,
                compensation_error = %compensation_error,
                "failed to fully compensate fork session creation"
            );
        }
    }

    async fn compensate_panicked_creation(&self, session_id: &SessionId, kind: &str) {
        let compensation = AssertUnwindSafe(async {
            let runtime = self.runtime_for(session_id);
            let compensation_result =
                match Session::open(runtime, Arc::clone(&self.runtime_services)).await {
                    Ok(session) => self.discard_failed_lifecycle_start(&session).await,
                    Err(_) => self.discard_failed_creation(session_id).await,
                };
            if let Err(error) = compensation_result {
                tracing::warn!(
                    %session_id,
                    creation_kind = kind,
                    compensation_error = %error,
                    "failed to fully compensate panicked session creation"
                );
            }
        })
        .catch_unwind()
        .await;
        if compensation.is_err() {
            tracing::warn!(
                %session_id,
                creation_kind = kind,
                "session creation panic compensation panicked"
            );
        }
    }

    async fn discard_failed_lifecycle_start(&self, session: &Session) -> Result<(), String> {
        let shutdown_result =
            AssertUnwindSafe(session.emit_lifecycle(ExtensionEvent::SessionShutdown))
                .catch_unwind()
                .await;
        let discard_result = self.discard_failed_creation(session.id()).await;
        let mut errors = Vec::new();
        match shutdown_result {
            Ok(Ok(())) => {},
            Ok(Err(error)) => errors.push(format!("run session shutdown hooks: {error}")),
            Err(_) => errors.push("session shutdown hooks panicked".into()),
        }
        if let Err(error) = discard_result {
            errors.push(format!("discard session: {error}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    async fn discard_failed_creation(&self, session_id: &SessionId) -> Result<(), String> {
        let closing = self.begin_session_transition(session_id).await;
        let release_result = self
            .event_sink
            .release(self.event_store.as_ref(), session_id)
            .await;
        let delete_result = self.event_store.delete_session(session_id).await;
        if delete_result.is_ok() {
            self.event_bus.detach(session_id);
            self.runtime_services
                .session_resources()
                .cleanup(session_id);
            for cleanup in &self.resource_cleanups {
                cleanup.cleanup(session_id);
            }
        }
        closing.complete();

        let mut errors = Vec::new();
        if let Err(error) = release_result {
            errors.push(format!("release event lane: {error}"));
        }
        if let Err(error) = delete_result {
            errors.push(format!("delete persisted session: {error}"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

/// session runtime 状态转换完成前供并发操作等待。
#[derive(Default)]
struct PendingSessionTransition(CancellationToken);

impl PendingSessionTransition {
    async fn wait(&self) {
        self.0.cancelled().await;
    }

    fn finish(&self) {
        self.0.cancel();
    }
}

enum TransitionStart {
    Waiting(Arc<PendingSessionTransition>),
    Started(Arc<PendingSessionTransition>),
}

#[derive(Clone, Copy)]
enum CloseSessionAction {
    Delete,
    Recycle,
}

#[derive(Clone, Copy)]
enum FailedForkCreationStage {
    Persisted,
    LifecycleStartFailed,
}

#[derive(Default)]
struct SessionTransitions {
    pending: Mutex<HashMap<SessionId, Arc<PendingSessionTransition>>>,
}

impl SessionTransitions {
    fn begin_open(&self, session_id: &SessionId) -> TransitionStart {
        self.begin(session_id)
    }

    fn begin_close(&self, session_id: &SessionId) -> TransitionStart {
        self.begin(session_id)
    }

    fn begin(&self, session_id: &SessionId) -> TransitionStart {
        let mut pending = self.pending.lock();
        match pending.get(session_id) {
            Some(existing) => TransitionStart::Waiting(Arc::clone(existing)),
            None => {
                let started = Arc::default();
                pending.insert(session_id.clone(), Arc::clone(&started));
                TransitionStart::Started(started)
            },
        }
    }

    fn complete(&self, session_id: &SessionId, expected: &Arc<PendingSessionTransition>) {
        let mut pending = self.pending.lock();
        if pending
            .get(session_id)
            .is_some_and(|current| Arc::ptr_eq(current, expected))
        {
            pending.remove(session_id);
            drop(pending);
            expected.finish();
        }
    }
}

pub(crate) struct SessionTransitionGuard {
    transitions: Arc<SessionTransitions>,
    session_id: SessionId,
    pending: Arc<PendingSessionTransition>,
    completed: bool,
}

impl SessionTransitionGuard {
    fn new(
        transitions: Arc<SessionTransitions>,
        session_id: SessionId,
        pending: Arc<PendingSessionTransition>,
    ) -> Self {
        Self {
            transitions,
            session_id,
            pending,
            completed: false,
        }
    }

    fn complete(mut self) {
        self.transitions.complete(&self.session_id, &self.pending);
        self.completed = true;
    }

    fn session_id(&self) -> &SessionId {
        &self.session_id
    }
}

impl Drop for SessionTransitionGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.transitions.complete(&self.session_id, &self.pending);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    };

    use astrcode_context::{NoopPostCompactEnricher, context_assembler::LlmContextAssembler};
    use astrcode_core::{
        config::{EffectiveConfig, LlmSettings},
        event::{DurableEvent, StoredEvent},
        llm::{LlmError, LlmEvent, LlmMessage, LlmProvider, ModelLimits},
        tool::{ToolDefinition, ToolResultArtifactSlice},
        types::ToolCallId,
    };
    use astrcode_extension_sdk::{
        extension::{ExtensionError, ExtensionEvent, LifecycleContext},
        runtime_ports::{NoopRuntimePorts, TurnHooks},
    };
    use astrcode_session::{SessionExtensionPorts, SessionRuntimeServices};
    use astrcode_session_projection::{
        AgentSessionLinkView, AgentSessionStatus, SessionReadModel, SessionSummary,
    };
    use astrcode_storage::{
        CompactSnapshotInput, EventReader, SessionEventJournal, SessionPathResolver, SessionReader,
        ToolResultArtifactInput, ToolResultArtifactRef, ToolResultArtifactStore,
        in_memory::InMemoryEventStore,
    };
    use tokio::sync::{Notify, Semaphore, mpsc};

    use super::*;

    const INJECTED_APPEND_ERROR: &str = "injected creation-link append failure";

    struct UnusedLlm;

    #[async_trait::async_trait]
    impl LlmProvider for UnusedLlm {
        async fn generate(
            &self,
            _messages: Vec<LlmMessage>,
            _tools: Vec<ToolDefinition>,
        ) -> Result<mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
            unreachable!("creation compensation tests do not run a turn")
        }

        fn model_limits(&self) -> ModelLimits {
            ModelLimits {
                max_input_tokens: 1024,
                max_output_tokens: 1024,
            }
        }
    }

    fn test_runtime_services() -> Arc<SessionRuntimeServices> {
        test_runtime_services_with_hooks(Arc::new(NoopRuntimePorts))
    }

    fn test_runtime_services_with_hooks(hooks: Arc<dyn TurnHooks>) -> Arc<SessionRuntimeServices> {
        let llm: Arc<dyn LlmProvider> = Arc::new(UnusedLlm);
        let mut llm_settings = LlmSettings::unconfigured();
        llm_settings.model_id = "mock-model".into();
        let noop = Arc::new(NoopRuntimePorts);
        Arc::new(SessionRuntimeServices::new(
            Arc::clone(&llm),
            llm,
            EffectiveConfig {
                llm: llm_settings.clone(),
                small_llm: llm_settings,
                context: Default::default(),
                agent: Default::default(),
                permissions: Default::default(),
                extensions: Default::default(),
            },
            SessionExtensionPorts::from_immutable_ports(noop.clone(), hooks, noop.clone()),
            Arc::new(LlmContextAssembler::new(Default::default())),
            Arc::new(NoopPostCompactEnricher),
            noop,
        ))
    }

    struct FailingStartHooks {
        fail_start_at: usize,
        starts: AtomicUsize,
        events: Mutex<Vec<(ExtensionEvent, String)>>,
    }

    impl FailingStartHooks {
        fn new(fail_start_at: usize) -> Self {
            Self {
                fail_start_at,
                starts: AtomicUsize::new(0),
                events: Mutex::new(Vec::new()),
            }
        }

        fn observed(&self, event: ExtensionEvent, session_id: &SessionId) -> bool {
            self.events
                .lock()
                .iter()
                .any(|(observed_event, observed_session_id)| {
                    observed_event == &event && observed_session_id == session_id.as_str()
                })
        }
    }

    #[async_trait::async_trait]
    impl TurnHooks for FailingStartHooks {
        async fn emit_lifecycle(
            &self,
            event: ExtensionEvent,
            ctx: LifecycleContext,
        ) -> Result<(), ExtensionError> {
            self.events.lock().push((event.clone(), ctx.session_id));
            if event == ExtensionEvent::SessionStart {
                let start = self.starts.fetch_add(1, Ordering::SeqCst) + 1;
                if start == self.fail_start_at {
                    return Err(ExtensionError::Internal(
                        "injected lifecycle start failure".into(),
                    ));
                }
            }
            Ok(())
        }
    }

    struct BlockingStartHooks {
        block_start_at: usize,
        starts: AtomicUsize,
        blocked: AtomicBool,
        entered: Notify,
        release: Semaphore,
        outcome: AtomicU8,
        events: Mutex<Vec<(ExtensionEvent, String)>>,
    }

    impl BlockingStartHooks {
        fn new(block_start_at: usize) -> Self {
            Self {
                block_start_at,
                starts: AtomicUsize::new(0),
                blocked: AtomicBool::new(false),
                entered: Notify::new(),
                release: Semaphore::new(0),
                outcome: AtomicU8::new(0),
                events: Mutex::new(Vec::new()),
            }
        }

        async fn wait_until_blocked(&self) {
            loop {
                let notified = self.entered.notified();
                if self.blocked.load(Ordering::SeqCst) {
                    return;
                }
                notified.await;
            }
        }

        fn release_with_failure(&self) {
            self.outcome.store(0, Ordering::SeqCst);
            self.release.add_permits(1);
        }

        fn release_success(&self) {
            self.outcome.store(1, Ordering::SeqCst);
            self.release.add_permits(1);
        }

        fn release_with_panic(&self) {
            self.outcome.store(2, Ordering::SeqCst);
            self.release.add_permits(1);
        }

        fn observed(&self, event: ExtensionEvent, session_id: &SessionId) -> bool {
            self.events
                .lock()
                .iter()
                .any(|(observed_event, observed_session_id)| {
                    observed_event == &event && observed_session_id == session_id.as_str()
                })
        }
    }

    #[async_trait::async_trait]
    impl TurnHooks for BlockingStartHooks {
        async fn emit_lifecycle(
            &self,
            event: ExtensionEvent,
            ctx: LifecycleContext,
        ) -> Result<(), ExtensionError> {
            self.events.lock().push((event.clone(), ctx.session_id));
            if event != ExtensionEvent::SessionStart {
                return Ok(());
            }

            let start = self.starts.fetch_add(1, Ordering::SeqCst) + 1;
            if start != self.block_start_at {
                return Ok(());
            }
            self.blocked.store(true, Ordering::SeqCst);
            self.entered.notify_waiters();
            self.release.acquire().await.unwrap().forget();
            match self.outcome.load(Ordering::SeqCst) {
                1 => Ok(()),
                2 => panic!("injected blocking lifecycle start panic"),
                _ => Err(ExtensionError::Internal(
                    "injected blocking lifecycle start failure".into(),
                )),
            }
        }
    }

    #[derive(Default)]
    struct FailingAppendStore {
        inner: InMemoryEventStore,
        append_count: AtomicUsize,
        fail_append_at: AtomicUsize,
        sync_count: AtomicUsize,
        fail_sync_at: AtomicUsize,
        fail_next_delete: AtomicBool,
        block_next_read: AtomicBool,
        read_blocked: AtomicBool,
        read_entered: Notify,
        read_release: Notify,
        created_sessions: Mutex<Vec<SessionId>>,
        created_notify: Notify,
    }

    impl FailingAppendStore {
        fn fail_next_append(&self) {
            self.fail_append_at.store(
                self.append_count.load(Ordering::SeqCst) + 1,
                Ordering::SeqCst,
            );
        }

        fn fail_next_sync(&self) {
            self.fail_sync_after(1);
        }

        fn fail_sync_after(&self, offset: usize) {
            self.fail_sync_at.store(
                self.sync_count.load(Ordering::SeqCst) + offset,
                Ordering::SeqCst,
            );
        }

        fn fail_next_delete(&self) {
            self.fail_next_delete.store(true, Ordering::SeqCst);
        }

        fn block_next_read(&self) {
            self.block_next_read.store(true, Ordering::SeqCst);
        }

        async fn wait_until_read_blocked(&self) {
            loop {
                let notified = self.read_entered.notified();
                if self.read_blocked.load(Ordering::SeqCst) {
                    return;
                }
                notified.await;
            }
        }

        fn release_read(&self) {
            self.read_release.notify_one();
        }

        fn created_sessions(&self) -> Vec<SessionId> {
            self.created_sessions.lock().clone()
        }

        async fn wait_for_created_sessions(&self, count: usize) {
            loop {
                let notified = self.created_notify.notified();
                if self.created_sessions.lock().len() >= count {
                    return;
                }
                notified.await;
            }
        }
    }

    #[async_trait::async_trait]
    impl EventReader for FailingAppendStore {
        async fn replay_events(
            &self,
            session_id: &SessionId,
        ) -> Result<Vec<StoredEvent>, StorageError> {
            self.inner.replay_events(session_id).await
        }

        async fn latest_cursor(
            &self,
            session_id: &SessionId,
        ) -> Result<Option<Cursor>, StorageError> {
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
    impl SessionReader for FailingAppendStore {
        async fn session_read_model(
            &self,
            session_id: &SessionId,
        ) -> Result<Arc<SessionReadModel>, StorageError> {
            if self.block_next_read.swap(false, Ordering::SeqCst) {
                self.read_blocked.store(true, Ordering::SeqCst);
                self.read_entered.notify_waiters();
                self.read_release.notified().await;
            }
            self.inner.session_read_model(session_id).await
        }

        async fn list_session_summaries(&self) -> Result<Vec<SessionSummary>, StorageError> {
            self.inner.list_session_summaries().await
        }

        async fn session_agent_sessions(
            &self,
            session_id: &SessionId,
        ) -> Result<Vec<AgentSessionLinkView>, StorageError> {
            self.inner.session_agent_sessions(session_id).await
        }
    }

    #[async_trait::async_trait]
    impl SessionPathResolver for FailingAppendStore {
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
                .planned_session_store_dir(
                    session_id,
                    working_dir,
                    parent_session_id,
                    source_extension,
                )
                .await
        }
    }

    #[async_trait::async_trait]
    impl ToolResultArtifactStore for FailingAppendStore {
        async fn read_tool_result_artifact_by_path(
            &self,
            session_id: &SessionId,
            path: &str,
            char_offset: usize,
            max_chars: usize,
        ) -> Result<ToolResultArtifactSlice, StorageError> {
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
    impl SessionEventJournal for FailingAppendStore {
        async fn create_session(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
            let session_id = event.session_id.clone();
            let stored = self.inner.create_session(event).await?;
            self.created_sessions.lock().push(session_id);
            self.created_notify.notify_waiters();
            Ok(stored)
        }

        async fn append_event(&self, event: DurableEvent) -> Result<StoredEvent, StorageError> {
            let append_number = self.append_count.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_append_at.load(Ordering::SeqCst) == append_number {
                return Err(StorageError::InvalidEvent(INJECTED_APPEND_ERROR.into()));
            }
            self.inner.append_event(event).await
        }

        async fn sync_durable_events(&self, session_id: &SessionId) -> Result<(), StorageError> {
            let sync_number = self.sync_count.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_sync_at.load(Ordering::SeqCst) == sync_number {
                return Err(StorageError::InvalidEvent(
                    "injected event-lane release failure".into(),
                ));
            }
            self.inner.sync_durable_events(session_id).await
        }
    }

    #[async_trait::async_trait]
    impl SessionStore for FailingAppendStore {
        async fn checkpoint(
            &self,
            session_id: &SessionId,
            cursor: &Cursor,
        ) -> Result<(), StorageError> {
            self.inner.checkpoint(session_id, cursor).await
        }

        async fn delete_session(&self, session_id: &SessionId) -> Result<(), StorageError> {
            if self.fail_next_delete.swap(false, Ordering::SeqCst) {
                return Err(StorageError::InvalidEvent(
                    "injected creation compensation delete failure".into(),
                ));
            }
            self.inner.delete_session(session_id).await
        }

        async fn write_compact_snapshot(
            &self,
            session_id: &SessionId,
            snapshot: CompactSnapshotInput,
        ) -> Result<Option<String>, StorageError> {
            self.inner
                .write_compact_snapshot(session_id, snapshot)
                .await
        }
    }

    #[derive(Default)]
    struct RecordingCleanup(Mutex<Vec<SessionId>>);

    impl SessionResourceCleanup for RecordingCleanup {
        fn cleanup(&self, session_id: &SessionId) {
            self.0.lock().push(session_id.clone());
        }
    }

    fn assert_runtime_was_released(
        services: &SessionRuntimeServices,
        store: Arc<dyn SessionStore>,
        session_id: &SessionId,
    ) {
        let replacement = Arc::new(SessionRuntimeState::new(session_id.clone(), store));
        let stored = services
            .session_resources()
            .resources_for(session_id, || Arc::clone(&replacement));
        assert!(
            Arc::ptr_eq(&stored, &replacement),
            "failed creation must release its runtime resource"
        );
        services.session_resources().cleanup(session_id);
    }

    fn assert_runtime_was_preserved(
        services: &SessionRuntimeServices,
        store: Arc<dyn SessionStore>,
        session_id: &SessionId,
    ) {
        let replacement = Arc::new(SessionRuntimeState::new(session_id.clone(), store));
        let stored = services
            .session_resources()
            .resources_for(session_id, || Arc::clone(&replacement));
        assert!(
            !Arc::ptr_eq(&stored, &replacement),
            "failed compensation must preserve the recoverable runtime resource"
        );
    }

    async fn create_root_session(
        store: Arc<dyn SessionStore>,
        runtime_services: Arc<SessionRuntimeServices>,
    ) -> Session {
        let session_id = astrcode_core::types::new_session_id();
        Session::create_with_params(SessionCreateParams {
            working_dir: ".".into(),
            model_id: "mock-model".into(),
            parent_session_id: None,
            tool_selection: None,
            source_extension: None,
            extra_system_prompt: None,
            initial_system_prompt: None,
            runtime: Arc::new(SessionRuntimeState::new(session_id, store)),
            runtime_services,
        })
        .await
        .unwrap()
    }

    async fn assert_task_waits_for_creation<T>(task: &mut tokio::task::JoinHandle<T>) {
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut *task)
                .await
                .is_err(),
            "open must wait while session creation is in progress"
        );
    }

    #[tokio::test]
    async fn transition_gate_blocks_open_until_close_finishes() {
        let transitions = Arc::new(SessionTransitions::default());
        let session_id = SessionId::new("session-closing");

        let TransitionStart::Started(pending) = transitions.begin_close(&session_id) else {
            panic!("close should acquire the transition");
        };

        let TransitionStart::Waiting(waiting) = transitions.begin_open(&session_id) else {
            panic!("open should wait while the session is closing");
        };
        assert!(Arc::ptr_eq(&pending, &waiting));

        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let task_release = Arc::clone(&release);
        let closing = SessionTransitionGuard::new(
            Arc::clone(&transitions),
            session_id.clone(),
            Arc::clone(&pending),
        );
        let detached = tokio::spawn(async move {
            task_release.acquire().await.unwrap().forget();
            drop(closing);
        });
        drop(detached);
        assert!(matches!(
            transitions.begin_open(&session_id),
            TransitionStart::Waiting(_)
        ));

        release.add_permits(1);
        waiting.wait().await;

        let TransitionStart::Started(pending) = transitions.begin_close(&session_id) else {
            panic!("completed close should release the transition");
        };
        transitions.complete(&session_id, &pending);
    }

    #[tokio::test]
    async fn failed_creation_steps_discard_session_resources() {
        let missing_store: Arc<dyn SessionStore> = Arc::new(InMemoryEventStore::new());
        let missing_services = test_runtime_services();
        let missing_manager = SessionManager::new(
            Arc::clone(&missing_store),
            Arc::clone(&missing_services),
            Vec::new(),
        );
        let missing_id = SessionId::new("missing-session");
        let shared_runtime = missing_manager.runtime_for(&missing_id);
        assert!(missing_manager.open(missing_id.clone()).await.is_err());
        assert!(Arc::ptr_eq(
            &shared_runtime,
            &missing_manager.runtime_for(&missing_id)
        ));
        drop(shared_runtime);
        assert!(missing_manager.open(missing_id.clone()).await.is_err());
        assert_runtime_was_released(
            missing_services.as_ref(),
            Arc::clone(&missing_store),
            &missing_id,
        );

        let root_store = Arc::new(FailingAppendStore::default());
        let root_store_port: Arc<dyn SessionStore> = root_store.clone();
        let root_hooks = Arc::new(FailingStartHooks::new(1));
        let root_hooks_port: Arc<dyn TurnHooks> = root_hooks.clone();
        let root_services = test_runtime_services_with_hooks(root_hooks_port);
        let root_cleanup = Arc::new(RecordingCleanup::default());
        let root_cleanup_port: Arc<dyn SessionResourceCleanup> = root_cleanup.clone();
        let root_manager = SessionManager::new(
            Arc::clone(&root_store_port),
            Arc::clone(&root_services),
            vec![root_cleanup_port],
        );
        let mut root_events = root_manager.event_bus().subscribe_all_notifications();

        let root_error = match root_manager.create(".").await {
            Ok(_) => panic!("root lifecycle start must fail"),
            Err(error) => error,
        };
        assert!(
            root_error
                .to_string()
                .contains("injected lifecycle start failure")
        );
        let root_id = root_store.created_sessions()[0].clone();
        assert!(root_store.list_sessions().await.unwrap().is_empty());
        assert!(root_hooks.observed(ExtensionEvent::SessionStart, &root_id));
        assert!(root_hooks.observed(ExtensionEvent::SessionShutdown, &root_id));
        assert!(
            root_events.try_recv().is_err(),
            "failed creation must not publish buffered session events"
        );
        assert_runtime_was_released(
            root_services.as_ref(),
            Arc::clone(&root_store_port),
            &root_id,
        );
        assert_eq!(*root_cleanup.0.lock(), vec![root_id]);

        let root_sync_store = Arc::new(FailingAppendStore::default());
        let root_sync_store_port: Arc<dyn SessionStore> = root_sync_store.clone();
        let root_sync_services = test_runtime_services();
        let root_sync_cleanup = Arc::new(RecordingCleanup::default());
        let root_sync_cleanup_port: Arc<dyn SessionResourceCleanup> = root_sync_cleanup.clone();
        let root_sync_manager = SessionManager::new(
            Arc::clone(&root_sync_store_port),
            Arc::clone(&root_sync_services),
            vec![root_sync_cleanup_port],
        );
        root_sync_store.fail_next_sync();
        let root_sync_error = match root_sync_manager.create(".").await {
            Ok(_) => panic!("root creation must not commit before its event log is synced"),
            Err(error) => error,
        };
        assert!(
            root_sync_error
                .to_string()
                .contains("injected event-lane release failure")
        );
        let root_sync_id = root_sync_store.created_sessions()[0].clone();
        assert!(root_sync_store.list_sessions().await.unwrap().is_empty());
        assert_runtime_was_released(
            root_sync_services.as_ref(),
            Arc::clone(&root_sync_store_port),
            &root_sync_id,
        );
        assert_eq!(*root_sync_cleanup.0.lock(), vec![root_sync_id]);

        let child_store = Arc::new(FailingAppendStore::default());
        let child_store_port: Arc<dyn SessionStore> = child_store.clone();
        let child_services = test_runtime_services();
        let parent =
            create_root_session(Arc::clone(&child_store_port), Arc::clone(&child_services)).await;
        let parent_id = parent.id().clone();

        child_store.fail_next_append();
        let child_error = match parent
            .spawn_child(
                ".",
                "mock-model",
                "worker".into(),
                "test compensation".into(),
                None,
                None,
                None,
                ToolCallId::new("call-compensation"),
            )
            .await
        {
            Ok(_) => panic!("parent link append must fail"),
            Err(error) => error,
        };
        assert!(child_error.to_string().contains(INJECTED_APPEND_ERROR));
        let child_id = child_store.created_sessions()[1].clone();
        assert_eq!(
            child_store.list_sessions().await.unwrap(),
            vec![parent_id.clone()]
        );
        assert!(parent.read_model().await.unwrap().agent_sessions.is_empty());
        assert_runtime_was_released(
            child_services.as_ref(),
            Arc::clone(&child_store_port),
            &child_id,
        );

        child_store.fail_sync_after(2);
        let child_sync_error = match parent
            .spawn_child(
                ".",
                "mock-model",
                "worker".into(),
                "test parent link sync compensation".into(),
                None,
                None,
                None,
                ToolCallId::new("call-sync-compensation"),
            )
            .await
        {
            Ok(_) => panic!("parent link sync must fail"),
            Err(error) => error,
        };
        assert!(
            child_sync_error
                .to_string()
                .contains("injected event-lane release failure")
        );
        let sync_child_id = child_store.created_sessions()[2].clone();
        assert_eq!(
            child_store.list_sessions().await.unwrap(),
            vec![parent_id.clone()]
        );
        let parent_model = parent.read_model().await.unwrap();
        let failed_link = parent_model
            .agent_sessions
            .iter()
            .find(|link| link.child_session_id == sync_child_id)
            .expect("parent link must be compensated with a terminal state");
        assert_eq!(failed_link.status, AgentSessionStatus::Failed);
        assert_runtime_was_released(
            child_services.as_ref(),
            Arc::clone(&child_store_port),
            &sync_child_id,
        );

        let child_lifecycle_store = Arc::new(FailingAppendStore::default());
        let child_lifecycle_store_port: Arc<dyn SessionStore> = child_lifecycle_store.clone();
        let child_lifecycle_hooks = Arc::new(FailingStartHooks::new(1));
        let child_lifecycle_hooks_port: Arc<dyn TurnHooks> = child_lifecycle_hooks.clone();
        let child_lifecycle_services = test_runtime_services_with_hooks(child_lifecycle_hooks_port);
        let lifecycle_parent = create_root_session(
            Arc::clone(&child_lifecycle_store_port),
            Arc::clone(&child_lifecycle_services),
        )
        .await;
        let lifecycle_parent_id = lifecycle_parent.id().clone();

        let lifecycle_error = match lifecycle_parent
            .spawn_child(
                ".",
                "mock-model",
                "worker".into(),
                "test lifecycle compensation".into(),
                None,
                None,
                None,
                ToolCallId::new("call-lifecycle-compensation"),
            )
            .await
        {
            Ok(_) => panic!("child lifecycle start must fail"),
            Err(error) => error,
        };
        assert!(
            lifecycle_error
                .to_string()
                .contains("injected lifecycle start failure")
        );
        let lifecycle_child_id = child_lifecycle_store.created_sessions()[1].clone();
        assert_eq!(
            child_lifecycle_store.list_sessions().await.unwrap(),
            vec![lifecycle_parent_id]
        );
        assert!(
            lifecycle_parent
                .read_model()
                .await
                .unwrap()
                .agent_sessions
                .is_empty(),
            "a child is linked only after lifecycle initialization succeeds"
        );
        assert!(child_lifecycle_hooks.observed(ExtensionEvent::SessionStart, &lifecycle_child_id));
        assert!(
            child_lifecycle_hooks.observed(ExtensionEvent::SessionShutdown, &lifecycle_child_id)
        );
        assert_runtime_was_released(
            child_lifecycle_services.as_ref(),
            Arc::clone(&child_lifecycle_store_port),
            &lifecycle_child_id,
        );

        let fork_store = Arc::new(FailingAppendStore::default());
        let fork_store_port: Arc<dyn SessionStore> = fork_store.clone();
        let fork_services = test_runtime_services();
        let external_cleanup = Arc::new(RecordingCleanup::default());
        let cleanup_port: Arc<dyn SessionResourceCleanup> = external_cleanup.clone();
        let manager = SessionManager::new(
            Arc::clone(&fork_store_port),
            Arc::clone(&fork_services),
            vec![cleanup_port],
        );
        let source = manager.create(".").await.unwrap();

        fork_store.fail_next_append();
        let fork_error = match manager.fork(source.id(), None).await {
            Ok(_) => panic!("fork link append must fail"),
            Err(error) => error,
        };
        assert!(fork_error.to_string().contains(INJECTED_APPEND_ERROR));
        let fork_id = fork_store.created_sessions()[1].clone();
        assert_eq!(
            fork_store.list_sessions().await.unwrap(),
            vec![source.id().clone()]
        );
        assert_runtime_was_released(
            fork_services.as_ref(),
            Arc::clone(&fork_store_port),
            &fork_id,
        );
        assert_eq!(*external_cleanup.0.lock(), vec![fork_id]);

        let fork_lifecycle_store = Arc::new(FailingAppendStore::default());
        let fork_lifecycle_store_port: Arc<dyn SessionStore> = fork_lifecycle_store.clone();
        let fork_lifecycle_hooks = Arc::new(FailingStartHooks::new(2));
        let fork_lifecycle_hooks_port: Arc<dyn TurnHooks> = fork_lifecycle_hooks.clone();
        let fork_lifecycle_services = test_runtime_services_with_hooks(fork_lifecycle_hooks_port);
        let fork_lifecycle_cleanup = Arc::new(RecordingCleanup::default());
        let fork_lifecycle_cleanup_port: Arc<dyn SessionResourceCleanup> =
            fork_lifecycle_cleanup.clone();
        let lifecycle_manager = SessionManager::new(
            Arc::clone(&fork_lifecycle_store_port),
            Arc::clone(&fork_lifecycle_services),
            vec![fork_lifecycle_cleanup_port],
        );
        let lifecycle_source = lifecycle_manager.create(".").await.unwrap();

        let lifecycle_fork_error = match lifecycle_manager.fork(lifecycle_source.id(), None).await {
            Ok(_) => panic!("fork lifecycle start must fail"),
            Err(error) => error,
        };
        assert!(
            lifecycle_fork_error
                .to_string()
                .contains("injected lifecycle start failure")
        );
        let lifecycle_fork_id = fork_lifecycle_store.created_sessions()[1].clone();
        assert_eq!(
            fork_lifecycle_store.list_sessions().await.unwrap(),
            vec![lifecycle_source.id().clone()]
        );
        assert!(fork_lifecycle_hooks.observed(ExtensionEvent::SessionStart, lifecycle_source.id()));
        assert!(fork_lifecycle_hooks.observed(ExtensionEvent::SessionStart, &lifecycle_fork_id));
        assert!(fork_lifecycle_hooks.observed(ExtensionEvent::SessionShutdown, &lifecycle_fork_id));
        assert_runtime_was_released(
            fork_lifecycle_services.as_ref(),
            Arc::clone(&fork_lifecycle_store_port),
            &lifecycle_fork_id,
        );
        assert_eq!(*fork_lifecycle_cleanup.0.lock(), vec![lifecycle_fork_id]);

        let close_store = Arc::new(FailingAppendStore::default());
        let close_store_port: Arc<dyn SessionStore> = close_store.clone();
        let close_services = test_runtime_services();
        let close_cleanup = Arc::new(RecordingCleanup::default());
        let close_cleanup_port: Arc<dyn SessionResourceCleanup> = close_cleanup.clone();
        let close_manager = SessionManager::new(
            Arc::clone(&close_store_port),
            Arc::clone(&close_services),
            vec![close_cleanup_port],
        );
        let close_session = close_manager.create(".").await.unwrap();
        let close_session_id = close_session.id().clone();

        close_store.fail_next_sync();
        let close_error = close_manager.delete(&close_session_id).await.unwrap_err();
        assert!(
            close_error
                .to_string()
                .contains("injected event-lane release failure")
        );
        assert_eq!(
            close_store.list_sessions().await.unwrap(),
            vec![close_session_id.clone()]
        );
        assert!(close_cleanup.0.lock().is_empty());
        assert_runtime_was_preserved(
            close_services.as_ref(),
            Arc::clone(&close_store_port),
            &close_session_id,
        );
    }

    #[tokio::test]
    async fn shutdown_rejects_fork_that_has_not_registered_its_creation_owner() {
        let store = Arc::new(FailingAppendStore::default());
        let store_port: Arc<dyn SessionStore> = store.clone();
        let manager = Arc::new(SessionManager::new(
            Arc::clone(&store_port),
            test_runtime_services(),
            vec![],
        ));
        let source_id = manager.create(".").await.unwrap().id().clone();
        store.block_next_read();

        let fork_manager = Arc::clone(&manager);
        let fork_source_id = source_id.clone();
        let fork = tokio::spawn(async move { fork_manager.fork(&fork_source_id, None).await });
        store.wait_until_read_blocked().await;
        manager.owned_tasks.close_and_wait().await;
        store.release_read();

        let error = match fork.await.unwrap() {
            Ok(_) => panic!("fork must not start after the creation gate closes"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("session manager is shutting down")
        );
        assert_eq!(store.list_sessions().await.unwrap(), vec![source_id]);
    }

    #[tokio::test]
    async fn aborted_creation_callers_leave_no_partial_root_fork_or_child() {
        let root_store = Arc::new(FailingAppendStore::default());
        let root_store_port: Arc<dyn SessionStore> = root_store.clone();
        let root_hooks = Arc::new(BlockingStartHooks::new(1));
        let root_services = test_runtime_services_with_hooks(root_hooks.clone());
        let root_manager = Arc::new(SessionManager::new(
            Arc::clone(&root_store_port),
            root_services,
            vec![],
        ));
        let create_manager = Arc::clone(&root_manager);
        let root_caller = tokio::spawn(async move { create_manager.create(".").await });
        root_hooks.wait_until_blocked().await;
        root_store.wait_for_created_sessions(1).await;
        let root_id = root_store.created_sessions()[0].clone();
        root_caller.abort();
        assert!(matches!(
            root_caller.await,
            Err(error) if error.is_cancelled()
        ));
        root_hooks.release_success();
        root_manager.owned_tasks.close_and_wait().await;
        assert!(root_manager.open(root_id.clone()).await.is_ok());
        assert_eq!(root_store.list_sessions().await.unwrap(), vec![root_id]);

        let fork_store = Arc::new(FailingAppendStore::default());
        let fork_store_port: Arc<dyn SessionStore> = fork_store.clone();
        let fork_hooks = Arc::new(BlockingStartHooks::new(2));
        let fork_services = test_runtime_services_with_hooks(fork_hooks.clone());
        let fork_manager = Arc::new(SessionManager::new(
            Arc::clone(&fork_store_port),
            fork_services,
            vec![],
        ));
        let source = fork_manager.create(".").await.unwrap();
        let source_id = source.id().clone();
        let caller_manager = Arc::clone(&fork_manager);
        let caller_source_id = source_id.clone();
        let fork_caller =
            tokio::spawn(async move { caller_manager.fork(&caller_source_id, None).await });
        fork_hooks.wait_until_blocked().await;
        fork_store.wait_for_created_sessions(2).await;
        let fork_id = fork_store.created_sessions()[1].clone();
        fork_caller.abort();
        assert!(matches!(
            fork_caller.await,
            Err(error) if error.is_cancelled()
        ));
        fork_hooks.release_with_failure();
        fork_manager.owned_tasks.close_and_wait().await;
        assert_eq!(fork_store.list_sessions().await.unwrap(), vec![source_id]);
        assert!(fork_hooks.observed(ExtensionEvent::SessionShutdown, &fork_id));

        let child_store = Arc::new(FailingAppendStore::default());
        let child_store_port: Arc<dyn SessionStore> = child_store.clone();
        let child_hooks = Arc::new(BlockingStartHooks::new(1));
        let child_services = test_runtime_services_with_hooks(child_hooks.clone());
        let child_manager = Arc::new(SessionManager::new(
            Arc::clone(&child_store_port),
            Arc::clone(&child_services),
            vec![],
        ));
        let parent = create_root_session(Arc::clone(&child_store_port), child_services).await;
        let parent_id = parent.id().clone();
        let child_parent = parent.clone();
        let child_owner = child_manager
            .spawn_creation_task(async move {
                child_parent
                    .spawn_child(
                        ".",
                        "mock-model",
                        "worker".into(),
                        "panic during lifecycle initialization".into(),
                        None,
                        None,
                        None,
                        ToolCallId::new("call-aborted-child"),
                    )
                    .await
            })
            .unwrap();
        let child_caller = tokio::spawn(child_owner);
        child_hooks.wait_until_blocked().await;
        child_store.wait_for_created_sessions(2).await;
        let child_id = child_store.created_sessions()[1].clone();
        child_caller.abort();
        assert!(matches!(
            child_caller.await,
            Err(error) if error.is_cancelled()
        ));
        child_hooks.release_with_panic();
        child_manager.owned_tasks.close_and_wait().await;
        assert_eq!(child_store.list_sessions().await.unwrap(), vec![parent_id]);
        assert!(parent.read_model().await.unwrap().agent_sessions.is_empty());
        assert!(child_hooks.observed(ExtensionEvent::SessionShutdown, &child_id));
    }

    #[tokio::test]
    async fn concurrent_open_waits_for_root_fork_and_child_initialization() {
        let root_store = Arc::new(FailingAppendStore::default());
        let root_store_port: Arc<dyn SessionStore> = root_store.clone();
        let root_hooks = Arc::new(BlockingStartHooks::new(1));
        let root_hooks_port: Arc<dyn TurnHooks> = root_hooks.clone();
        let root_services = test_runtime_services_with_hooks(root_hooks_port);
        let root_manager = Arc::new(SessionManager::new(
            Arc::clone(&root_store_port),
            Arc::clone(&root_services),
            vec![],
        ));
        let create_manager = Arc::clone(&root_manager);
        let create_task = tokio::spawn(async move { create_manager.create(".").await });
        root_hooks.wait_until_blocked().await;
        root_store.wait_for_created_sessions(1).await;
        let root_id = root_store.created_sessions()[0].clone();
        let open_manager = Arc::clone(&root_manager);
        let open_root_id = root_id.clone();
        let mut open_task = tokio::spawn(async move { open_manager.open(open_root_id).await });
        assert_task_waits_for_creation(&mut open_task).await;

        root_store.fail_next_delete();
        root_hooks.release_with_failure();
        let create_error = match create_task.await.unwrap() {
            Ok(_) => panic!("root lifecycle start must fail"),
            Err(error) => error,
        };
        assert!(
            create_error
                .to_string()
                .contains("injected blocking lifecycle start failure")
        );
        let open_error = match open_task.await.unwrap() {
            Ok(_) => panic!("open must not resume a failed root creation"),
            Err(error) => error,
        };
        assert!(
            open_error
                .to_string()
                .contains("session creation failed before lifecycle initialization committed")
        );
        assert!(!root_hooks.observed(ExtensionEvent::SessionResume, &root_id));
        assert_eq!(
            root_store.list_sessions().await.unwrap(),
            vec![root_id.clone()]
        );
        assert_runtime_was_preserved(
            root_services.as_ref(),
            Arc::clone(&root_store_port),
            &root_id,
        );
        let repeated_open_error = match root_manager.open(root_id.clone()).await {
            Ok(_) => panic!("failed creation tombstone must reject later opens"),
            Err(error) => error,
        };
        assert!(
            repeated_open_error
                .to_string()
                .contains("session creation failed before lifecycle initialization committed")
        );
        assert!(!root_hooks.observed(ExtensionEvent::SessionResume, &root_id));

        let fork_store = Arc::new(FailingAppendStore::default());
        let fork_store_port: Arc<dyn SessionStore> = fork_store.clone();
        let fork_hooks = Arc::new(BlockingStartHooks::new(2));
        let fork_hooks_port: Arc<dyn TurnHooks> = fork_hooks.clone();
        let fork_services = test_runtime_services_with_hooks(fork_hooks_port);
        let fork_manager = Arc::new(SessionManager::new(
            Arc::clone(&fork_store_port),
            Arc::clone(&fork_services),
            vec![],
        ));
        let source = fork_manager.create(".").await.unwrap();
        let source_id = source.id().clone();
        let create_fork_manager = Arc::clone(&fork_manager);
        let create_fork_source_id = source_id.clone();
        let fork_task =
            tokio::spawn(
                async move { create_fork_manager.fork(&create_fork_source_id, None).await },
            );
        fork_hooks.wait_until_blocked().await;
        fork_store.wait_for_created_sessions(2).await;
        let fork_id = fork_store.created_sessions()[1].clone();
        let open_fork_manager = Arc::clone(&fork_manager);
        let open_fork_id = fork_id.clone();
        let mut open_fork_task =
            tokio::spawn(async move { open_fork_manager.open(open_fork_id).await });
        assert_task_waits_for_creation(&mut open_fork_task).await;

        fork_hooks.release_with_failure();
        let fork_error = match fork_task.await.unwrap() {
            Ok(_) => panic!("fork lifecycle start must fail"),
            Err(error) => error,
        };
        assert!(
            fork_error
                .to_string()
                .contains("injected blocking lifecycle start failure")
        );
        let open_fork_error = match open_fork_task.await.unwrap() {
            Ok(_) => panic!("open must not resume a failed fork creation"),
            Err(error) => error,
        };
        assert!(
            open_fork_error
                .to_string()
                .contains("session creation failed before lifecycle initialization committed")
        );
        assert!(!fork_hooks.observed(ExtensionEvent::SessionResume, &fork_id));

        let child_store = Arc::new(FailingAppendStore::default());
        let child_store_port: Arc<dyn SessionStore> = child_store.clone();
        let child_hooks = Arc::new(BlockingStartHooks::new(1));
        let child_hooks_port: Arc<dyn TurnHooks> = child_hooks.clone();
        let child_services = test_runtime_services_with_hooks(child_hooks_port);
        let child_manager = Arc::new(SessionManager::new(
            Arc::clone(&child_store_port),
            Arc::clone(&child_services),
            vec![],
        ));
        let parent =
            create_root_session(Arc::clone(&child_store_port), Arc::clone(&child_services)).await;
        let child_parent = parent.clone();
        let child_task = tokio::spawn(async move {
            child_parent
                .spawn_child(
                    ".",
                    "mock-model",
                    "worker".into(),
                    "block lifecycle initialization".into(),
                    None,
                    None,
                    None,
                    ToolCallId::new("call-blocked-child"),
                )
                .await
        });
        child_hooks.wait_until_blocked().await;
        child_store.wait_for_created_sessions(2).await;
        let child_id = child_store.created_sessions()[1].clone();
        let open_child_manager = Arc::clone(&child_manager);
        let open_child_id = child_id.clone();
        let mut open_child_task =
            tokio::spawn(async move { open_child_manager.open(open_child_id).await });
        assert_task_waits_for_creation(&mut open_child_task).await;

        child_hooks.release_with_failure();
        let child_error = match child_task.await.unwrap() {
            Ok(_) => panic!("child lifecycle start must fail"),
            Err(error) => error,
        };
        assert!(
            child_error
                .to_string()
                .contains("injected blocking lifecycle start failure")
        );
        let open_child_error = match open_child_task.await.unwrap() {
            Ok(_) => panic!("open must not resume a failed child creation"),
            Err(error) => error,
        };
        assert!(
            open_child_error
                .to_string()
                .contains("session creation failed before lifecycle initialization committed")
        );
        assert!(!child_hooks.observed(ExtensionEvent::SessionResume, &child_id));
        assert!(parent.read_model().await.unwrap().agent_sessions.is_empty());
    }
}
