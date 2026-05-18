use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    config::ModelSelection,
    event::Event,
    extension::ExtensionEvent,
    storage::{EventStore, SessionReadModel, SessionSummary, StorageError},
    types::{Cursor, SessionId},
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_session::{Capabilities, Session, SessionError, SessionRuntimeState};
use parking_lot::Mutex;

use crate::config_manager::ConfigManager;

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
///
/// 后台任务（`BackgroundTaskManager`）现在由 `SessionRuntimeState` 持有，
/// 跟着 session 走；SessionManager 不再持有全局副本。删除 session 时通过
/// `runtime_states` 找到对应 runtime 并清理它的 bg_tasks。
pub struct SessionManager {
    event_store: Arc<dyn EventStore>,
    config: Arc<ConfigManager>,
    extension_runner: Arc<ExtensionRunner>,
    runtime_states: Mutex<HashMap<SessionId, Arc<SessionRuntimeState>>>,
    capabilities: Arc<Capabilities>,
}

impl SessionManager {
    // ─── 生命周期 ─────────────────────────────────────────────────────

    pub fn new(
        event_store: Arc<dyn EventStore>,
        config: Arc<ConfigManager>,
        extension_runner: Arc<ExtensionRunner>,
        capabilities: Arc<Capabilities>,
    ) -> Self {
        Self {
            event_store,
            config,
            extension_runner,
            runtime_states: Mutex::new(HashMap::new()),
            capabilities,
        }
    }

    fn get_or_create_runtime(&self, session_id: &SessionId) -> Arc<SessionRuntimeState> {
        self.runtime_states
            .lock()
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(SessionRuntimeState::default()))
            .clone()
    }

    pub(crate) fn config(&self) -> &Arc<ConfigManager> {
        &self.config
    }

    pub(crate) async fn create(
        &self,
        working_dir: &str,
    ) -> Result<CreatedSession, SessionManagerError> {
        let model_id = self.config.read_effective().llm.model_id.clone();
        // 先在 registry 里登记 runtime，再创建 Session 让两者共享同一份。
        let sid = astrcode_core::types::new_session_id();
        let runtime = self.get_or_create_runtime(&sid);
        // SessionManager 调用 Session::create_with_id 而非 create_full：因为 sid 已生成。
        let session = Session::create_with_id(
            Arc::clone(&self.event_store),
            sid.clone(),
            working_dir,
            &model_id,
            None,
            None,
            runtime,
            Arc::clone(&self.capabilities),
        )
        .await?;

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
        let runtime = self.get_or_create_runtime(&session_id);
        let session = Session::open(
            Arc::clone(&self.event_store),
            session_id,
            runtime,
            Arc::clone(&self.capabilities),
        )
        .await?;
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
        // 清理本 session 的 runtime（含 bg_tasks）后从 registry 移除。
        if let Some(runtime) = self.runtime_states.lock().remove(session_id) {
            runtime
                .background_tasks()
                .lock()
                .cleanup_session(session_id);
        }
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
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc, time::Duration};

    use astrcode_context::prompt_engine::load_system_prompt_files;
    use astrcode_core::{
        extension::{Extension, Registrar, ToolHandler},
        tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult},
    };
    use astrcode_session::session_setup::{
        SystemPromptSnapshotInput, build_system_prompt_snapshot, build_tool_registry_snapshot,
    };

    use super::*;

    struct StaticToolExtension {
        id: &'static str,
        tool_name: &'static str,
        description: &'static str,
    }

    #[async_trait::async_trait]
    impl Extension for StaticToolExtension {
        fn id(&self) -> &str {
            self.id
        }

        fn register(&self, reg: &mut Registrar) {
            reg.tool(
                ToolDefinition {
                    name: self.tool_name.into(),
                    description: self.description.into(),
                    parameters: serde_json::json!({"type": "object"}),
                    origin: ToolOrigin::Extension,
                    execution_mode: ExecutionMode::Sequential,
                },
                Arc::new(StaticToolHandler),
            );
        }
    }

    struct StaticToolHandler;

    #[async_trait::async_trait]
    impl ToolHandler for StaticToolHandler {
        async fn execute(
            &self,
            tool_name: &str,
            _arguments: serde_json::Value,
            _working_dir: &str,
            _ctx: &astrcode_core::tool::ToolExecutionContext,
        ) -> Result<ToolResult, astrcode_core::extension::ExtensionError> {
            Err(astrcode_core::extension::ExtensionError::NotFound(
                tool_name.into(),
            ))
        }
    }

    #[tokio::test]
    async fn child_extra_system_prompt_participates_in_snapshot_build() {
        let runner = ExtensionRunner::new(Duration::from_secs(1));
        let prompt_files = load_system_prompt_files(".").await;
        let (system_prompt, fingerprint) =
            build_system_prompt_snapshot(SystemPromptSnapshotInput {
                extension_runner: &runner,
                session_id: "session-1",
                working_dir: ".",
                model_id: "mock",
                tools: &[],
                extra_system_prompt: Some("child body"),
                tool_prompt_metadata: HashMap::new(),
                prompt_files,
            })
            .await
            .unwrap();

        assert!(system_prompt.contains("child body"));
        assert!(!fingerprint.is_empty());
    }

    #[tokio::test]
    async fn tool_snapshot_precedence_is_explicit() {
        let runner = ExtensionRunner::new(Duration::from_secs(1));
        runner
            .register(Arc::new(StaticToolExtension {
                id: "first",
                tool_name: "shell",
                description: "first extension shell",
            }))
            .await;
        runner
            .register(Arc::new(StaticToolExtension {
                id: "second",
                tool_name: "shell",
                description: "second extension shell",
            }))
            .await;

        let registry = build_tool_registry_snapshot(&runner, ".", 1, None).await;
        let shell = registry.find_definition("shell").unwrap();

        assert_eq!(shell.origin, ToolOrigin::Extension);
        assert_eq!(shell.description, "first extension shell");
    }
}
