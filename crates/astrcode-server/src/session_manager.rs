use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    event::{DurableEventPayload, Event},
    tool::SessionToolSelection,
    types::{Cursor, SessionId},
};
use astrcode_extension_sdk::extension::ExtensionEvent;
use astrcode_session::{
    Session, SessionCreateParams, SessionError, SessionEventObserver, SessionEventSink,
    SessionRuntimeServices, SessionRuntimeState, emit_lifecycle_for_read_model,
};
use astrcode_session_projection::{AgentSessionLinkView, SessionReadModel, SessionSummary};
use astrcode_storage::{SessionStore, StorageError};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{server_event_bus::ServerEventBus, session_resource_cleanup::SessionResourceCleanup};

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
    transitions: Arc<SessionTransitions>,
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
        let sid = astrcode_core::types::new_session_id();
        let runtime = self.runtime_for(&sid);
        let session = match Session::create_with_params(SessionCreateParams {
            working_dir: working_dir.to_owned(),
            model_id: self.runtime_services.read_effective().llm.model_id.clone(),
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
                self.runtime_services.session_resources().cleanup(&sid);
                return Err(error.into());
            },
        };

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
                    let runtime = self.runtime_for(&session_id);
                    let session =
                        Session::open(runtime, Arc::clone(&self.runtime_services)).await?;
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
        if let Err(e) = self
            .event_sink
            .sync(self.event_store.clone(), session_id)
            .await
        {
            tracing::error!(session_id = %session_id, error = %e, "failed to sync durable events");
        }
    }

    pub(crate) async fn shutdown(&self) {
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
        let closing = self.begin_session_close(session_id).await;
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

    async fn begin_session_close(&self, session_id: &SessionId) -> SessionTransitionGuard {
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
        let runtime = self.runtime_for(&new_sid);
        let initial_system_prompt = Some(astrcode_core::event::PersistedSystemPrompt {
            text: source_model.system_prompt.text.clone(),
            fingerprint: source_model.system_prompt.fingerprint.clone(),
            extra_system_prompt: source_model.system_prompt.extra.clone(),
            source: astrcode_core::event::SystemPromptSource::Inherited,
        });
        let session = match Session::create_with_params(SessionCreateParams {
            working_dir: source_model.identity.working_dir.clone(),
            model_id: source_model.identity.model_id.clone(),
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
                self.runtime_services.session_resources().cleanup(&new_sid);
                return Err(error.into());
            },
        };

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

enum TransitionStart {
    Waiting(Arc<PendingSessionTransition>),
    Started(Arc<PendingSessionTransition>),
}

#[derive(Clone, Copy)]
enum CloseSessionAction {
    Delete,
    Recycle,
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

struct SessionTransitionGuard {
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
    use super::*;

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
}
