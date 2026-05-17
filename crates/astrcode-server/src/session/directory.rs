use std::sync::Arc;

use astrcode_core::{
    config::ModelSelection,
    event::Event,
    extension::ExtensionEvent,
    storage::{EventStore, SessionReadModel, SessionSummary, StorageError},
    types::{Cursor, SessionId},
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_session::{
    Session, SessionError, SessionRuntimeRegistry, background::BackgroundTaskManager,
};
use parking_lot::Mutex;

use crate::config_manager::ConfigManager;

pub(crate) struct CreatedSession {
    pub(crate) session: Session,
    pub(crate) start_event: Event,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionDirectoryError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Extension(#[from] astrcode_core::extension::ExtensionError),
    #[error("session created but no events found")]
    MissingStartEvent,
}

/// Server 侧的 session 生命周期门面。
///
/// durable session 仍由 [`Session`] / [`EventStore`] 负责；这里集中管理
/// 与 session 同生灭的进程内资源，避免 handler 逐项记忆清理细节。
pub struct SessionDirectory {
    event_store: Arc<dyn EventStore>,
    config: Arc<ConfigManager>,
    extension_runner: Arc<ExtensionRunner>,
    runtime_registry: Arc<SessionRuntimeRegistry>,
    background_tasks: Arc<Mutex<BackgroundTaskManager>>,
}

impl SessionDirectory {
    // ─── 生命周期 ─────────────────────────────────────────────────────

    pub fn new(
        event_store: Arc<dyn EventStore>,
        config: Arc<ConfigManager>,
        extension_runner: Arc<ExtensionRunner>,
        runtime_registry: Arc<SessionRuntimeRegistry>,
        background_tasks: Arc<Mutex<BackgroundTaskManager>>,
    ) -> Self {
        Self {
            event_store,
            config,
            extension_runner,
            runtime_registry,
            background_tasks,
        }
    }

    pub(crate) async fn create(
        &self,
        working_dir: &str,
    ) -> Result<CreatedSession, SessionDirectoryError> {
        let model_id = self.config.read_effective().llm.model_id.clone();
        let session =
            Session::create(Arc::clone(&self.event_store), working_dir, &model_id, None).await?;
        let sid = session.id().clone();
        self.runtime_registry.get_or_create(&sid);

        let start_event = self
            .event_store
            .replay_events(&sid)
            .await?
            .into_iter()
            .next()
            .ok_or(SessionDirectoryError::MissingStartEvent)?;

        let lifecycle_ctx = astrcode_core::extension::LifecycleContext {
            session_id: sid.to_string(),
            working_dir: working_dir.to_string(),
            model: ModelSelection::simple(model_id),
        };
        self.extension_runner
            .emit_lifecycle(ExtensionEvent::SessionStart, lifecycle_ctx)
            .await?;

        Ok(CreatedSession {
            session,
            start_event,
        })
    }

    pub(crate) async fn open(
        &self,
        session_id: SessionId,
    ) -> Result<Session, SessionDirectoryError> {
        let session = Session::open(Arc::clone(&self.event_store), session_id.clone()).await?;
        self.runtime_registry.get_or_create(&session_id);
        Ok(session)
    }

    pub(crate) async fn create_child(
        &self,
        working_dir: &str,
        model_id: &str,
        parent_session_id: &SessionId,
    ) -> Result<Session, SessionDirectoryError> {
        let session = Session::create(
            Arc::clone(&self.event_store),
            working_dir,
            model_id,
            Some(parent_session_id),
        )
        .await?;
        self.runtime_registry.get_or_create(session.id());
        Ok(session)
    }

    pub(crate) async fn delete(&self, session_id: &SessionId) -> Result<(), SessionDirectoryError> {
        let lifecycle_ctx = astrcode_core::extension::LifecycleContext {
            session_id: session_id.to_string(),
            working_dir: String::new(),
            model: ModelSelection::simple(self.config.read_effective().llm.model_id.clone()),
        };
        self.extension_runner
            .emit_lifecycle(ExtensionEvent::SessionShutdown, lifecycle_ctx)
            .await?;
        self.event_store.delete_session(session_id).await?;
        self.cleanup_background_tasks(session_id);
        self.runtime_registry.remove(session_id);
        Ok(())
    }

    // ─── 只读查询 ─────────────────────────────────────────────────────

    pub(crate) async fn read_model(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionReadModel, SessionDirectoryError> {
        self.event_store
            .session_read_model(session_id)
            .await
            .map_err(SessionDirectoryError::from)
    }

    pub(crate) async fn list_summaries(
        &self,
    ) -> Result<Vec<SessionSummary>, SessionDirectoryError> {
        self.event_store
            .list_session_summaries()
            .await
            .map_err(SessionDirectoryError::from)
    }

    pub(crate) async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<Event>, SessionDirectoryError> {
        self.event_store
            .replay_from(session_id, cursor)
            .await
            .map_err(SessionDirectoryError::from)
    }

    pub(crate) async fn latest_cursor(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Cursor>, SessionDirectoryError> {
        self.event_store
            .latest_cursor(session_id)
            .await
            .map_err(SessionDirectoryError::from)
    }

    // ─── session 级运行时清理 ─────────────────────────────────────────
    pub(crate) fn cleanup_background_tasks(&self, session_id: &SessionId) {
        self.background_tasks.lock().cleanup_session(session_id);
    }
}
