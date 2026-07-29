use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    event::{DurableEventPayload, Event},
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
use tokio_util::sync::CancellationToken;

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
    #[error("session close task failed: {0}")]
    CloseTask(String),
}

/// Session durable 生命周期门面（create/open/delete/fork）与 per-session runtime 唯一性。
///
/// 不处理 active turn、输入队列或 child completion——那些由 [`crate::turn_scheduler`]
/// 与 [`crate::child_session`] 负责。
pub struct SessionManager {
    event_store: Arc<dyn SessionStore>,
    config: Arc<ConfigManager>,
    runtime_registry: Arc<SessionRuntimeRegistry>,
    runtime_services: Arc<SessionRuntimeServices>,
    event_bus: Arc<ServerEventBus>,
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
            runtime_registry: Arc::new(SessionRuntimeRegistry::default()),
            runtime_services,
            event_bus: Arc::new(ServerEventBus::new()),
            resource_cleanups,
        }
    }

    pub(crate) fn event_bus(&self) -> &Arc<ServerEventBus> {
        &self.event_bus
    }

    fn attach_session_subscribers(&self, session: &Session) {
        self.event_bus.attach(session);
    }

    fn runtime_for_open(&self, session_id: &SessionId) -> RuntimeForOpen {
        self.runtime_registry
            .runtime_for_open(session_id, || self.new_runtime_state(session_id))
    }

    fn new_runtime_state(&self, session_id: &SessionId) -> Arc<SessionRuntimeState> {
        let model_id = self.config.read_effective().llm.model_id.clone();
        Arc::new(SessionRuntimeState::new(
            session_id.clone(),
            self.event_store.clone(),
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
    /// 子会话由 `Session::spawn_child` 创建，其 runtime 不经过 manager 的创建路径，
    /// 必须手动注册才能让后续 `open(child_sid)` 拿到同一个 runtime（共享广播通道）。
    /// event_bus 的 attach 由 TurnScheduler 在 submit 时统一处理。
    pub(crate) fn register_child_session(&self, session: &Session) {
        let runtime = session.runtime_arc();
        self.runtime_registry.insert(runtime);
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
        // 先在 registry 里登记 runtime，再创建 Session 让两者共享同一份。
        let sid = astrcode_core::types::new_session_id();
        let runtime = self.new_runtime_state(&sid);
        self.runtime_registry.insert(Arc::clone(&runtime));
        let session = match Session::create_with_params(SessionCreateParams {
            working_dir: working_dir.to_owned(),
            parent_session_id: None,
            tool_selection: tool_selection.cloned(),
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
                    let session =
                        Session::open(runtime, Arc::clone(&self.runtime_services)).await?;
                    self.attach_session_subscribers(&session);
                    return Ok(session);
                },
                RuntimeForOpen::Waiting(pending) => {
                    pending.wait().await;
                },
                RuntimeForOpen::Started(runtime) => {
                    let resume = SessionResumeGuard::new(
                        &self.runtime_registry,
                        session_id.clone(),
                        Arc::clone(&runtime),
                    );
                    let session =
                        Session::open(runtime, Arc::clone(&self.runtime_services)).await?;
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
        if let Some(runtime) = self.runtime_registry.get(session_id) {
            match runtime.sync_durable_events().await {
                Ok(()) => return,
                Err(error) => {
                    tracing::warn!(
                        session_id = %session_id,
                        %error,
                        "session publisher sync unavailable; falling back to journal sync"
                    );
                },
            }
        }
        if let Err(e) = self.event_store.sync_durable_events(session_id).await {
            tracing::error!(session_id = %session_id, error = %e, "failed to sync durable events");
        }
    }

    /// 将共享运行时服务中的 provider / model_id 同步到所有已打开的 session runtime。
    ///
    /// 配置热更新先改 `SessionRuntimeServices`；调用方在配置事务提交
    /// 之后必须调用此方法，否则非 active session 的 turn 仍会用旧的 per-session binding。
    pub(crate) fn sync_all_model_bindings_from_config(&self) {
        let effective = self.config.read_effective();
        self.runtime_registry.sync_model_bindings(
            self.runtime_services.llm(),
            self.runtime_services.small_llm(),
            effective.llm.model_id.clone(),
        );
    }

    pub(crate) async fn shutdown_runtimes(&self) {
        for runtime in self.runtime_registry.runtimes() {
            if let Err(error) = runtime.shutdown_event_publisher().await {
                tracing::warn!(
                    session_id = %runtime.session_id(),
                    %error,
                    "failed to stop session event publisher"
                );
            }
        }
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
        self.emit_session_shutdown(session_id).await?;
        let (closing, runtime) = self.begin_session_close(session_id).await;
        let event_store = Arc::clone(&self.event_store);
        let event_bus = Arc::clone(&self.event_bus);
        let resource_cleanups = self.resource_cleanups.clone();
        let session_id = session_id.clone();

        tokio::spawn(async move {
            let _closing = closing;
            if let Some(runtime) = runtime {
                runtime
                    .shutdown_event_publisher()
                    .await
                    .map_err(SessionError::from)?;
            } else {
                event_store.sync_durable_events(&session_id).await?;
            }
            match action {
                CloseSessionAction::Delete => event_store.delete_session(&session_id).await?,
                CloseSessionAction::Recycle => event_store.recycle_session(&session_id).await?,
            }
            event_bus.detach(&session_id).await;
            for cleanup in resource_cleanups {
                cleanup.cleanup(&session_id);
            }
            Ok::<_, SessionManagerError>(())
        })
        .await
        .map_err(|error| SessionManagerError::CloseTask(error.to_string()))?
    }

    async fn begin_session_close(
        &self,
        session_id: &SessionId,
    ) -> (SessionCloseGuard, Option<Arc<SessionRuntimeState>>) {
        loop {
            match self.runtime_registry.runtime_for_close(session_id) {
                RuntimeForClose::Waiting(pending) => pending.wait().await,
                RuntimeForClose::Started { runtime, pending } => {
                    return (
                        SessionCloseGuard::new(
                            Arc::clone(&self.runtime_registry),
                            session_id.clone(),
                            pending,
                        ),
                        runtime,
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

        let new_sid = astrcode_core::types::new_session_id();
        let runtime = self.new_runtime_state(&new_sid);
        self.runtime_registry.insert(Arc::clone(&runtime));
        let initial_system_prompt = Some(astrcode_core::event::PersistedSystemPrompt {
            text: source_model.system_prompt.text.clone(),
            fingerprint: source_model.system_prompt.fingerprint.clone(),
            extra_system_prompt: source_model.system_prompt.extra.clone(),
            source: astrcode_core::event::SystemPromptSource::Inherited,
        });
        let session = match Session::create_with_params(SessionCreateParams {
            working_dir: source_model.identity.working_dir.clone(),
            parent_session_id: None,
            tool_selection: None,
            source_extension: None,
            extra_system_prompt: None,
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
            .emit_durable(
                None,
                DurableEventPayload::SessionForked {
                    source_session_id: source_id.clone(),
                    source_cursor: fork_cursor,
                    first_user_message,
                    messages: transcript_messages
                        .into_iter()
                        .map(|message| message.message)
                        .collect(),
                },
            )
            .await?;

        Ok(session)
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

enum SessionRuntimeEntry {
    Ready(Arc<SessionRuntimeState>),
    Resuming {
        runtime: Arc<SessionRuntimeState>,
        pending: Arc<PendingSessionTransition>,
    },
    Closing {
        runtime: Option<Arc<SessionRuntimeState>>,
        pending: Arc<PendingSessionTransition>,
    },
}

impl SessionRuntimeEntry {
    fn runtime(&self) -> Option<&Arc<SessionRuntimeState>> {
        match self {
            Self::Ready(runtime) | Self::Resuming { runtime, .. } => Some(runtime),
            Self::Closing { runtime, .. } => runtime.as_ref(),
        }
    }
}

enum RuntimeForOpen {
    Ready(Arc<SessionRuntimeState>),
    Waiting(Arc<PendingSessionTransition>),
    Started(Arc<SessionRuntimeState>),
}

enum RuntimeForClose {
    Waiting(Arc<PendingSessionTransition>),
    Started {
        runtime: Option<Arc<SessionRuntimeState>>,
        pending: Arc<PendingSessionTransition>,
    },
}

#[derive(Clone, Copy)]
enum CloseSessionAction {
    Delete,
    Recycle,
}

/// 保证同一 `SessionId` 在当前进程里只有一份 local runtime state。
#[derive(Default)]
struct SessionRuntimeRegistry {
    states: Mutex<HashMap<SessionId, SessionRuntimeEntry>>,
}

impl SessionRuntimeRegistry {
    fn get(&self, session_id: &SessionId) -> Option<Arc<SessionRuntimeState>> {
        self.states
            .lock()
            .get(session_id)
            .and_then(SessionRuntimeEntry::runtime)
            .map(Arc::clone)
    }

    fn runtime_for_open(
        &self,
        session_id: &SessionId,
        create: impl FnOnce() -> Arc<SessionRuntimeState>,
    ) -> RuntimeForOpen {
        let mut states = self.states.lock();
        match states.get(session_id) {
            Some(SessionRuntimeEntry::Ready(runtime)) => RuntimeForOpen::Ready(Arc::clone(runtime)),
            Some(SessionRuntimeEntry::Resuming { pending, .. })
            | Some(SessionRuntimeEntry::Closing { pending, .. }) => {
                RuntimeForOpen::Waiting(Arc::clone(pending))
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

    fn runtime_for_close(&self, session_id: &SessionId) -> RuntimeForClose {
        let mut states = self.states.lock();
        match states.get(session_id) {
            Some(SessionRuntimeEntry::Resuming { pending, .. })
            | Some(SessionRuntimeEntry::Closing { pending, .. }) => {
                RuntimeForClose::Waiting(Arc::clone(pending))
            },
            Some(SessionRuntimeEntry::Ready(runtime)) => {
                let runtime = Arc::clone(runtime);
                let pending = Arc::default();
                states.insert(
                    session_id.clone(),
                    SessionRuntimeEntry::Closing {
                        runtime: Some(Arc::clone(&runtime)),
                        pending: Arc::clone(&pending),
                    },
                );
                RuntimeForClose::Started {
                    runtime: Some(runtime),
                    pending,
                }
            },
            None => {
                let pending = Arc::default();
                states.insert(
                    session_id.clone(),
                    SessionRuntimeEntry::Closing {
                        runtime: None,
                        pending: Arc::clone(&pending),
                    },
                );
                RuntimeForClose::Started {
                    runtime: None,
                    pending,
                }
            },
        }
    }

    fn insert(&self, runtime: Arc<SessionRuntimeState>) {
        self.states.lock().insert(
            runtime.session_id().clone(),
            SessionRuntimeEntry::Ready(runtime),
        );
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
            if let Some(runtime) = entry.runtime() {
                runtime.replace_model_binding(
                    Arc::clone(&llm),
                    Arc::clone(&small_llm),
                    model_id.clone(),
                );
            }
        }
    }

    fn runtimes(&self) -> Vec<Arc<SessionRuntimeState>> {
        self.states
            .lock()
            .values()
            .filter_map(SessionRuntimeEntry::runtime)
            .cloned()
            .collect()
    }

    fn cleanup_runtime(&self, session_id: &SessionId) {
        let removed = self.states.lock().remove(session_id);
        match removed {
            Some(SessionRuntimeEntry::Resuming { pending, .. })
            | Some(SessionRuntimeEntry::Closing { pending, .. }) => pending.finish(),
            Some(SessionRuntimeEntry::Ready(_)) | None => {},
        }
    }

    fn complete_session_close(
        &self,
        session_id: &SessionId,
        expected: &Arc<PendingSessionTransition>,
    ) {
        let mut states = self.states.lock();
        let pending = match states.get(session_id) {
            Some(SessionRuntimeEntry::Closing { pending, .. })
                if Arc::ptr_eq(pending, expected) =>
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

struct SessionCloseGuard {
    registry: Arc<SessionRuntimeRegistry>,
    session_id: SessionId,
    pending: Arc<PendingSessionTransition>,
}

impl SessionCloseGuard {
    fn new(
        registry: Arc<SessionRuntimeRegistry>,
        session_id: SessionId,
        pending: Arc<PendingSessionTransition>,
    ) -> Self {
        Self {
            registry,
            session_id,
            pending,
        }
    }
}

impl Drop for SessionCloseGuard {
    fn drop(&mut self) {
        self.registry
            .complete_session_close(&self.session_id, &self.pending);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runtime_registry_blocks_open_until_close_finishes() {
        let registry = Arc::new(SessionRuntimeRegistry::default());
        let session_id = SessionId::new("session-closing");

        let RuntimeForClose::Started {
            runtime: None,
            pending,
        } = registry.runtime_for_close(&session_id)
        else {
            panic!("cold session should start closing without a runtime");
        };

        let RuntimeForOpen::Waiting(waiting) =
            registry.runtime_for_open(&session_id, || panic!("open must not create a runtime"))
        else {
            panic!("open should wait while the session is closing");
        };
        assert!(Arc::ptr_eq(&pending, &waiting));

        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let task_release = Arc::clone(&release);
        let closing = SessionCloseGuard::new(
            Arc::clone(&registry),
            session_id.clone(),
            Arc::clone(&pending),
        );
        let detached = tokio::spawn(async move {
            task_release.acquire().await.unwrap().forget();
            drop(closing);
        });
        drop(detached);
        assert!(matches!(
            registry.runtime_for_open(&session_id, || panic!("close task still owns the guard")),
            RuntimeForOpen::Waiting(_)
        ));

        release.add_permits(1);
        waiting.wait().await;

        let RuntimeForClose::Started {
            runtime: None,
            pending,
        } = registry.runtime_for_close(&session_id)
        else {
            panic!("completed close should release the registry entry");
        };
        registry.complete_session_close(&session_id, &pending);
    }
}
