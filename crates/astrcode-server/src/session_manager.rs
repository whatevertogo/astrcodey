use std::{
    collections::HashMap,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

use astrcode_core::{
    event::{DurableEvent, DurableEventPayload, Event},
    tool::SessionToolSelection,
    types::{Cursor, SessionId},
};
use astrcode_extension_sdk::extension::ExtensionEvent;
use astrcode_session::{
    Session, SessionCreateParams, SessionError, SessionRuntimeServices, SessionRuntimeState,
    emit_lifecycle_for_read_model,
};
use astrcode_session_projection::{AgentSessionLinkView, SessionReadModel, SessionSummary};
use astrcode_storage::{SessionStore, StorageError};
use parking_lot::Mutex;

use crate::{
    config_manager::ConfigManager, server_event_bus::ServerEventBus,
    session_resource_cleanup::SessionResourceCleanup,
};

pub(crate) struct CreatedSession {
    pub(crate) session: Session,
    pub(crate) start_event: Event,
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
    #[error("session created but no events found")]
    MissingStartEvent,
    #[error("invalid fork cursor: {0}")]
    InvalidCursor(String),
}

/// Session durable 生命周期门面（create/open/delete/fork）与 per-session runtime 唯一性。
///
/// 不处理 active turn、输入队列或 child completion——那些由 [`crate::turn_scheduler`]
/// 与 [`crate::child_session`] 负责。
pub struct SessionManager {
    event_store: Arc<dyn SessionStore>,
    config: Arc<ConfigManager>,
    runtime_registry: SessionRuntimeRegistry,
    runtime_services: Arc<SessionRuntimeServices>,
    event_bus: OnceLock<Arc<ServerEventBus>>,
    resource_cleanups: Vec<Arc<dyn SessionResourceCleanup>>,
}

impl SessionManager {
    // ─── 生命周期 ─────────────────────────────────────────────────────

    pub fn new(
        event_store: Arc<dyn SessionStore>,
        config: Arc<ConfigManager>,
        runtime_services: Arc<SessionRuntimeServices>,
        resource_cleanups: Vec<Arc<dyn SessionResourceCleanup>>,
    ) -> Self {
        Self {
            event_store,
            config,
            runtime_registry: SessionRuntimeRegistry::default(),
            runtime_services,
            event_bus: OnceLock::new(),
            resource_cleanups,
        }
    }

    /// 绑定事件总线（含 internal reactor）。create/fork/open 时 attach，delete/recycle 时 detach。
    pub fn bind_event_bus(&self, event_bus: Arc<ServerEventBus>) {
        let _ = self.event_bus.set(event_bus);
    }

    fn attach_session_subscribers(&self, session: &Session) {
        if let Some(bus) = self.event_bus.get() {
            bus.attach(session);
        }
    }

    fn detach_session_subscribers(&self, session_id: &SessionId) {
        if let Some(bus) = self.event_bus.get() {
            bus.detach(session_id);
        }
    }

    fn get_or_create_runtime(&self, session_id: &SessionId) -> Arc<SessionRuntimeState> {
        self.runtime_registry
            .get_or_create(session_id, || self.new_runtime_state())
    }

    fn runtime_for_open(&self, session_id: &SessionId) -> RuntimeForOpen {
        self.runtime_registry
            .runtime_for_open(session_id, || self.new_runtime_state())
    }

    fn new_runtime_state(&self) -> Arc<SessionRuntimeState> {
        let model_id = self.config.read_effective().llm.model_id.clone();
        Arc::new(SessionRuntimeState::new(
            self.runtime_services.llm(),
            self.runtime_services.small_llm(),
            model_id,
        ))
    }

    pub(crate) fn config(&self) -> &Arc<ConfigManager> {
        &self.config
    }

    /// 把子会话的 runtime 注册到 manager。
    ///
    /// 子会话由 `Session::spawn_child` 创建，其 runtime 不经过 `get_or_create_runtime`，
    /// 必须手动注册才能让后续 `open(child_sid)` 拿到同一个 runtime（共享广播通道）。
    /// event_bus 的 attach 由 TurnScheduler 在 submit 时统一处理。
    pub(crate) fn register_child_session(&self, session: &Session) {
        let sid = session.id().clone();
        let runtime = session.runtime_arc();
        self.runtime_registry.insert(sid, runtime);
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

    pub(crate) async fn create(
        &self,
        working_dir: &str,
    ) -> Result<CreatedSession, SessionManagerError> {
        self.create_with_tool_selection(working_dir, None).await
    }

    pub(crate) async fn create_with_tool_selection(
        &self,
        working_dir: &str,
        tool_selection: Option<&SessionToolSelection>,
    ) -> Result<CreatedSession, SessionManagerError> {
        let model_id = self.config.read_effective().llm.model_id.clone();
        // 先在 registry 里登记 runtime，再创建 Session 让两者共享同一份。
        let sid = astrcode_core::types::new_session_id();
        let runtime = self.get_or_create_runtime(&sid);
        let session = match Session::create_with_params(SessionCreateParams {
            store: Arc::clone(&self.event_store),
            session_id: sid.clone(),
            working_dir: working_dir.to_owned(),
            model_id,
            parent_session_id: None,
            tool_selection: tool_selection.cloned(),
            source_extension: None,
            initial_system_prompt: None,
            runtime,
            runtime_services: Arc::clone(&self.runtime_services),
        })
        .await
        {
            Ok(session) => session,
            Err(error) => {
                self.runtime_registry.cleanup_runtime(&sid);
                return Err(error.into());
            },
        };

        self.attach_session_subscribers(&session);

        let start_event = self
            .event_store
            .replay_events(&sid)
            .await?
            .into_iter()
            .next()
            .ok_or(SessionManagerError::MissingStartEvent)?
            .into();

        session.emit_lifecycle(ExtensionEvent::SessionStart).await?;

        Ok(CreatedSession {
            session,
            start_event,
        })
    }

    pub(crate) async fn open(&self, session_id: SessionId) -> Result<Session, SessionManagerError> {
        loop {
            match self.runtime_for_open(&session_id) {
                RuntimeForOpen::Ready(runtime) => {
                    let session = Session::open(
                        Arc::clone(&self.event_store),
                        session_id.clone(),
                        runtime,
                        Arc::clone(&self.runtime_services),
                    )
                    .await?;
                    self.attach_session_subscribers(&session);
                    return Ok(session);
                },
                RuntimeForOpen::Resuming(pending) => {
                    pending.wait().await;
                },
                RuntimeForOpen::Started(runtime) => {
                    let resume = SessionResumeGuard::new(
                        &self.runtime_registry,
                        session_id.clone(),
                        Arc::clone(&runtime),
                    );
                    let session = Session::open(
                        Arc::clone(&self.event_store),
                        session_id.clone(),
                        runtime,
                        Arc::clone(&self.runtime_services),
                    )
                    .await?;
                    session
                        .emit_lifecycle(ExtensionEvent::SessionResume)
                        .await?;
                    resume.complete();
                    self.attach_session_subscribers(&session);
                    return Ok(session);
                },
            }
        }
    }

    pub(crate) async fn delete(&self, session_id: &SessionId) -> Result<(), SessionManagerError> {
        self.emit_session_shutdown(session_id).await?;
        self.event_store.delete_session(session_id).await?;
        self.cleanup_session_resources(session_id);
        Ok(())
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

    /// 释放 session 占用的进程内资源。
    ///
    /// delete 和 recycle 共享同一套清理流程，确保两条路径不会出现遗漏。
    fn cleanup_session_resources(&self, session_id: &SessionId) {
        self.runtime_registry.cleanup_runtime(session_id);
        self.detach_session_subscribers(session_id);
        // 外部资源清理（trait 注入）。
        for cleanup in &self.resource_cleanups {
            cleanup.cleanup(session_id);
        }
    }

    // ─── 只读查询 ─────────────────────────────────────────────────────

    pub(crate) async fn read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionReadModel, SessionManagerError> {
        self.event_store
            .session_read_model(session_id)
            .await
            .map_err(SessionManagerError::from)
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
        if let Err(e) = self.event_store.sync_durable_events(session_id).await {
            tracing::error!(session_id = %session_id, error = %e, "failed to sync durable events");
        }
    }

    /// 将共享运行时服务中的 provider / model_id 同步到所有已打开的 session runtime。
    ///
    /// 配置热更新只改 `SessionRuntimeServices`；调用方在 `apply_raw_config_and_rebuild`
    /// 之后必须调用此方法，否则非 active session 的 turn 仍会用旧的 per-session binding。
    pub(crate) fn sync_all_model_bindings_from_config(&self) {
        let effective = self.config.read_effective();
        self.runtime_registry.sync_model_bindings(
            self.runtime_services.llm(),
            self.runtime_services.small_llm(),
            effective.llm.model_id.clone(),
        );
    }

    pub(crate) async fn recycle_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), SessionManagerError> {
        self.emit_session_shutdown(session_id).await?;
        self.event_store
            .recycle_session(session_id)
            .await
            .map_err(SessionManagerError::from)?;
        self.cleanup_session_resources(session_id);
        Ok(())
    }

    /// 从 .recycled/ 恢复一个已回收的 session。
    pub(crate) async fn restore_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), SessionManagerError> {
        self.event_store
            .restore_session(session_id)
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

        let (context_messages, retained_messages) = if at_cursor.is_some() {
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
            (
                truncated_model.transcript.context_messages,
                truncated_model.transcript.messages,
            )
        } else {
            (
                source_model.transcript.context_messages.clone(),
                source_model.transcript.messages.clone(),
            )
        };

        let model_id = self.config.read_effective().llm.model_id.clone();
        let new_sid = astrcode_core::types::new_session_id();
        let runtime = self.get_or_create_runtime(&new_sid);
        let initial_system_prompt = Some(astrcode_core::event::PersistedSystemPrompt {
            text: source_model.system_prompt.text.clone(),
            fingerprint: source_model.system_prompt.fingerprint.clone(),
            extra_system_prompt: source_model.system_prompt.extra.clone(),
            source: astrcode_core::event::SystemPromptSource::Inherited,
        });
        let session = match Session::create_with_params(SessionCreateParams {
            store: Arc::clone(&self.event_store),
            session_id: new_sid.clone(),
            working_dir: source_model.identity.working_dir.clone(),
            model_id,
            parent_session_id: None,
            tool_selection: None,
            source_extension: None,
            initial_system_prompt,
            runtime,
            runtime_services: Arc::clone(&self.runtime_services),
        })
        .await
        {
            Ok(session) => session,
            Err(error) => {
                self.runtime_registry.cleanup_runtime(&new_sid);
                return Err(error.into());
            },
        };

        self.attach_session_subscribers(&session);

        session
            .append_event(DurableEvent::new(
                new_sid.clone(),
                None,
                DurableEventPayload::SessionForked {
                    source_session_id: source_id.clone(),
                    source_cursor: fork_cursor,
                    context_messages: context_messages.into_iter().map(|m| m.message).collect(),
                    retained_messages: retained_messages.into_iter().map(|m| m.message).collect(),
                },
            ))
            .await?;

        Ok(session)
    }
}

/// 首次 cold open 的 SessionResume 完成前，后续 open 需在此 gate 上等待。
#[derive(Default)]
struct PendingSessionResume {
    done: AtomicBool,
    notify: tokio::sync::Notify,
}

impl PendingSessionResume {
    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.done.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    fn finish(&self) {
        self.done.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

enum SessionRuntimeEntry {
    Ready(Arc<SessionRuntimeState>),
    Resuming {
        runtime: Arc<SessionRuntimeState>,
        pending: Arc<PendingSessionResume>,
    },
}

impl SessionRuntimeEntry {
    fn runtime(&self) -> &Arc<SessionRuntimeState> {
        match self {
            Self::Ready(runtime) | Self::Resuming { runtime, .. } => runtime,
        }
    }
}

enum RuntimeForOpen {
    Ready(Arc<SessionRuntimeState>),
    Resuming(Arc<PendingSessionResume>),
    Started(Arc<SessionRuntimeState>),
}

/// 保证同一 `SessionId` 在当前进程里只有一份 local runtime state。
#[derive(Default)]
struct SessionRuntimeRegistry {
    states: Mutex<HashMap<SessionId, SessionRuntimeEntry>>,
}

impl SessionRuntimeRegistry {
    fn get_or_create(
        &self,
        session_id: &SessionId,
        create: impl FnOnce() -> Arc<SessionRuntimeState>,
    ) -> Arc<SessionRuntimeState> {
        let mut states = self.states.lock();
        if let Some(entry) = states.get(session_id) {
            return Arc::clone(entry.runtime());
        }
        let runtime = create();
        states.insert(
            session_id.clone(),
            SessionRuntimeEntry::Ready(Arc::clone(&runtime)),
        );
        runtime
    }

    fn runtime_for_open(
        &self,
        session_id: &SessionId,
        create: impl FnOnce() -> Arc<SessionRuntimeState>,
    ) -> RuntimeForOpen {
        let mut states = self.states.lock();
        match states.get(session_id) {
            Some(SessionRuntimeEntry::Ready(runtime)) => RuntimeForOpen::Ready(Arc::clone(runtime)),
            Some(SessionRuntimeEntry::Resuming { pending, .. }) => {
                RuntimeForOpen::Resuming(Arc::clone(pending))
            },
            None => {
                let runtime = create();
                states.insert(
                    session_id.clone(),
                    SessionRuntimeEntry::Resuming {
                        runtime: Arc::clone(&runtime),
                        pending: Arc::default(),
                    },
                );
                RuntimeForOpen::Started(runtime)
            },
        }
    }

    fn insert(&self, session_id: SessionId, runtime: Arc<SessionRuntimeState>) {
        self.states
            .lock()
            .insert(session_id, SessionRuntimeEntry::Ready(runtime));
    }

    fn complete_session_resume(&self, session_id: &SessionId, expected: &Arc<SessionRuntimeState>) {
        let mut states = self.states.lock();
        let transition = match states.get(session_id) {
            Some(SessionRuntimeEntry::Resuming { runtime, pending })
                if Arc::ptr_eq(runtime, expected) =>
            {
                Some((Arc::clone(runtime), Arc::clone(pending)))
            },
            _ => None,
        };
        if let Some((runtime, pending)) = transition {
            states.insert(session_id.clone(), SessionRuntimeEntry::Ready(runtime));
            drop(states);
            pending.finish();
        }
    }

    fn fail_session_resume(&self, session_id: &SessionId, expected: &Arc<SessionRuntimeState>) {
        let mut states = self.states.lock();
        let pending = match states.get(session_id) {
            Some(SessionRuntimeEntry::Resuming { runtime, pending })
                if Arc::ptr_eq(runtime, expected) =>
            {
                Some(Arc::clone(pending))
            },
            _ => None,
        };
        if let Some(pending) = pending {
            states.remove(session_id);
            drop(states);
            pending.finish();
        }
    }

    fn sync_model_bindings(
        &self,
        llm: Arc<dyn astrcode_core::llm::LlmProvider>,
        small_llm: Arc<dyn astrcode_core::llm::LlmProvider>,
        model_id: String,
    ) {
        for entry in self.states.lock().values() {
            entry.runtime().replace_model_binding(
                Arc::clone(&llm),
                Arc::clone(&small_llm),
                model_id.clone(),
            );
        }
    }

    fn cleanup_runtime(&self, session_id: &SessionId) {
        let removed = self.states.lock().remove(session_id);
        if let Some(SessionRuntimeEntry::Resuming { pending, .. }) = removed {
            pending.finish();
        }
    }
}

struct SessionResumeGuard<'a> {
    registry: &'a SessionRuntimeRegistry,
    session_id: SessionId,
    runtime: Arc<SessionRuntimeState>,
    completed: bool,
}

impl<'a> SessionResumeGuard<'a> {
    fn new(
        registry: &'a SessionRuntimeRegistry,
        session_id: SessionId,
        runtime: Arc<SessionRuntimeState>,
    ) -> Self {
        Self {
            registry,
            session_id,
            runtime,
            completed: false,
        }
    }

    fn complete(mut self) {
        self.registry
            .complete_session_resume(&self.session_id, &self.runtime);
        self.completed = true;
    }
}

impl Drop for SessionResumeGuard<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.registry
                .fail_session_resume(&self.session_id, &self.runtime);
        }
    }
}
