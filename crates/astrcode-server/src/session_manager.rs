use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    config::ModelSelection,
    event::{Event, EventPayload},
    extension::ExtensionEvent,
    storage::{EventStore, SessionReadModel, SessionSummary, StorageError},
    tool::FileObservationStore,
    types::{Cursor, SessionId},
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_session::{
    Session, SessionError, SessionRuntimeRegistry, background::BackgroundTaskManager,
};
use astrcode_tools::registry::ToolRegistry;
use parking_lot::Mutex;

use crate::{
    bootstrap::{
        SystemPromptSnapshotInput, build_system_prompt_snapshot_with_files,
        build_tool_registry_snapshot, load_system_prompt_files,
    },
    config_manager::ConfigManager,
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
    Extension(#[from] astrcode_core::extension::ExtensionError),
    #[error("session created but no events found")]
    MissingStartEvent,
}

/// Server 侧的 session 生命周期门面。
///
/// durable session 仍由 [`Session`] / [`EventStore`] 负责；这里集中管理
/// 与 session 同生灭的进程内资源，避免 handler 逐项记忆清理细节。
pub struct SessionManager {
    event_store: Arc<dyn EventStore>,
    config: Arc<ConfigManager>,
    extension_runner: Arc<ExtensionRunner>,
    runtime_registry: Arc<SessionRuntimeRegistry>,
    background_tasks: Arc<Mutex<BackgroundTaskManager>>,
    tool_registries: Mutex<HashMap<SessionId, Arc<ToolRegistry>>>,
}

impl SessionManager {
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
            tool_registries: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) async fn create(
        &self,
        working_dir: &str,
    ) -> Result<CreatedSession, SessionManagerError> {
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
            .ok_or(SessionManagerError::MissingStartEvent)?;

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

    pub(crate) async fn open(&self, session_id: SessionId) -> Result<Session, SessionManagerError> {
        let session = Session::open(Arc::clone(&self.event_store), session_id.clone()).await?;
        self.runtime_registry.get_or_create(&session_id);
        Ok(session)
    }

    pub(crate) async fn create_child(
        &self,
        working_dir: &str,
        model_id: &str,
        parent_session_id: &SessionId,
    ) -> Result<Session, SessionManagerError> {
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

    pub(crate) async fn delete(&self, session_id: &SessionId) -> Result<(), SessionManagerError> {
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
        self.tool_registries.lock().remove(session_id);
        Ok(())
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

    pub(crate) async fn list_summaries(&self) -> Result<Vec<SessionSummary>, SessionManagerError> {
        self.event_store
            .list_session_summaries()
            .await
            .map_err(SessionManagerError::from)
    }

    pub(crate) async fn replay_from(
        &self,
        session_id: &SessionId,
        cursor: &Cursor,
    ) -> Result<Vec<Event>, SessionManagerError> {
        self.event_store
            .replay_from(session_id, cursor)
            .await
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

    // ─── session 级运行时资源 ─────────────────────────────────────────

    pub(crate) async fn ensure_tool_registry(
        &self,
        session_id: &SessionId,
        working_dir: &str,
    ) -> Arc<ToolRegistry> {
        if let Some(registry) = self.tool_registries.lock().get(session_id).cloned() {
            return registry;
        }

        self.refresh_tool_registry(session_id, working_dir).await
    }

    pub(crate) async fn refresh_tool_registry(
        &self,
        session_id: &SessionId,
        working_dir: &str,
    ) -> Arc<ToolRegistry> {
        let timeout = self.config.read_effective().llm.read_timeout_secs;
        let registry =
            build_tool_registry_snapshot(&self.extension_runner, working_dir, timeout).await;
        self.tool_registries
            .lock()
            .insert(session_id.clone(), Arc::clone(&registry));
        registry
    }

    pub(crate) fn file_observation_store(
        &self,
        session_id: &SessionId,
    ) -> Arc<dyn FileObservationStore> {
        self.runtime_registry
            .get_or_create(session_id)
            .file_observation_store()
    }

    pub(crate) fn cleanup_background_tasks(&self, session_id: &SessionId) {
        self.background_tasks.lock().cleanup_session(session_id);
    }

    // ─── prompt 初始化 ────────────────────────────────────────────────

    pub(crate) async fn initialize_system_prompt(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        extra_system_prompt: Option<&str>,
    ) -> Result<(Arc<ToolRegistry>, Event), SessionManagerError> {
        let registry_fut = self.refresh_tool_registry(session_id, working_dir);
        let prompt_files_fut = load_system_prompt_files(working_dir);
        let (tool_registry, prompt_files) = tokio::join!(registry_fut, prompt_files_fut);
        let event = self
            .configure_system_prompt_with_files(
                session_id,
                working_dir,
                &tool_registry,
                extra_system_prompt,
                prompt_files,
            )
            .await?;
        Ok((tool_registry, event))
    }

    pub(crate) async fn configure_system_prompt(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        tool_registry: &ToolRegistry,
        extra_system_prompt: Option<&str>,
    ) -> Result<Event, SessionManagerError> {
        let prompt_files = load_system_prompt_files(working_dir).await;
        self.configure_system_prompt_with_files(
            session_id,
            working_dir,
            tool_registry,
            extra_system_prompt,
            prompt_files,
        )
        .await
    }

    pub(crate) async fn build_system_prompt_snapshot(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        model_id: &str,
        tool_registry: &ToolRegistry,
        extra_system_prompt: Option<&str>,
    ) -> Result<(String, String), SessionManagerError> {
        let prompt_files = load_system_prompt_files(working_dir).await;
        self.build_system_prompt_snapshot_with_files(
            session_id,
            working_dir,
            model_id,
            tool_registry,
            extra_system_prompt,
            prompt_files,
        )
        .await
    }

    async fn configure_system_prompt_with_files(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        tool_registry: &ToolRegistry,
        extra_system_prompt: Option<&str>,
        prompt_files: astrcode_context::prompt_engine::PromptFiles,
    ) -> Result<Event, SessionManagerError> {
        let model_id = self.config.read_effective().llm.model_id.clone();
        let (system_prompt, fingerprint) = self
            .build_system_prompt_snapshot_with_files(
                session_id,
                working_dir,
                &model_id,
                tool_registry,
                extra_system_prompt,
                prompt_files,
            )
            .await?;
        self.event_store
            .append_event(Event::new(
                session_id.clone(),
                None,
                EventPayload::SystemPromptConfigured {
                    text: system_prompt,
                    fingerprint,
                },
            ))
            .await
            .map_err(SessionManagerError::from)
    }

    async fn build_system_prompt_snapshot_with_files(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        model_id: &str,
        tool_registry: &ToolRegistry,
        extra_system_prompt: Option<&str>,
        prompt_files: astrcode_context::prompt_engine::PromptFiles,
    ) -> Result<(String, String), SessionManagerError> {
        let tools_with_meta = tool_registry.list_definitions_with_prompt_metadata();
        let tools: Vec<_> = tools_with_meta.iter().map(|(def, _)| def.clone()).collect();
        let tool_prompt_metadata = tools_with_meta
            .into_iter()
            .filter_map(|(def, meta)| meta.map(|m| (def.name, m)))
            .collect();
        build_system_prompt_snapshot_with_files(SystemPromptSnapshotInput {
            extension_runner: &self.extension_runner,
            session_id: session_id.as_str(),
            working_dir,
            model_id,
            tools: &tools,
            extra_system_prompt,
            tool_prompt_metadata,
            prompt_files,
        })
        .await
        .map_err(SessionManagerError::from)
    }
}
