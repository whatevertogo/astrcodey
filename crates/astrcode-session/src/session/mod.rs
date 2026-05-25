//! Session 句柄 — 带存储能力的会话操作入口。

mod children;
mod compact;
mod events;
mod prompt;
mod turn_entry;

use std::sync::Arc;

use astrcode_core::{
    storage::{EventStore, StorageError, ToolResultArtifactReader, ToolResultArtifactSlice},
    types::*,
};
use astrcode_support::shell::resolve_shell;

use crate::{
    session_runtime::SessionRuntimeState, session_runtime_services::SessionRuntimeServices,
};

/// 创建 session 所需的参数集合。
#[derive(Clone)]
pub struct SessionCreateParams {
    pub store: Arc<dyn EventStore>,
    pub sid: SessionId,
    pub working_dir: String,
    pub model_id: String,
    pub parent: Option<SessionId>,
    pub tool_policy: Option<astrcode_core::extension::ChildToolPolicy>,
    pub source_extension: Option<String>,
    pub runtime: Arc<SessionRuntimeState>,
    pub caps: Arc<SessionRuntimeServices>,
}

/// 会话句柄 — 带存储能力的会话操作入口。
///
/// 字段语义：
/// - `runtime`：进程内瞬态资源（工具表、file_obs、bg_tasks、event_tx）。 broadcast 在 runtime
///   上而不是 Session 上：同 sid 多次 `Session::open` / `clone` 仍共享同一个
///   broadcast，订阅者一处订阅就能看到所有实例发出的事件。
/// - `caps`：跨 session 共享的基础设施（LLM、扩展、上下文组装器、配置）。
///
/// `Clone` 是廉价的 Arc clone，可以自由复制。
#[derive(Clone)]
pub struct Session {
    pub(crate) id: SessionId,
    pub(crate) store: Arc<dyn EventStore>,
    pub(crate) runtime: Arc<SessionRuntimeState>,
    pub(crate) caps: Arc<SessionRuntimeServices>,
}

impl Session {
    /// 用调用方指定的 sid 创建会话。
    ///
    /// **注意**：`runtime` 必须由调用方保证「同 sid 唯一」，否则同 sid 的不同 Session
    /// 实例会有不同的 broadcast、不同的工具表、不同的 bg_tasks，订阅者只能看到自己那份
    /// 实例上发出的事件。生产路径走 `SessionManager`，由其内部的 `runtime_states` HashMap
    /// 保证唯一；CLI / 测试若直接调本入口须自行维护一份 sid→runtime 映射，或接受隔离语义。
    pub async fn create_with_params(params: SessionCreateParams) -> Result<Self, SessionError> {
        params
            .store
            .create_session(
                &params.sid,
                &params.working_dir,
                &params.model_id,
                params.parent.as_ref(),
                params.tool_policy.as_ref(),
                params.source_extension.as_deref(),
            )
            .await?;
        if let Some(policy) = &params.tool_policy {
            params.runtime.set_tool_policy(Some(policy.clone()));
        }
        Ok(Self {
            id: params.sid,
            store: params.store,
            runtime: params.runtime,
            caps: params.caps,
        })
    }

    /// 用调用方指定的 sid 创建会话（参数展开版，兼容旧调用点）。
    #[allow(clippy::too_many_arguments)]
    pub async fn create_with_id(
        store: Arc<dyn EventStore>,
        sid: SessionId,
        working_dir: &str,
        model_id: &str,
        parent: Option<&SessionId>,
        tool_policy: Option<&astrcode_core::extension::ChildToolPolicy>,
        source_extension: Option<&str>,
        runtime: Arc<SessionRuntimeState>,
        caps: Arc<SessionRuntimeServices>,
    ) -> Result<Self, SessionError> {
        Self::create_with_params(SessionCreateParams {
            store,
            sid,
            working_dir: working_dir.to_string(),
            model_id: model_id.to_string(),
            parent: parent.cloned(),
            tool_policy: tool_policy.cloned(),
            source_extension: source_extension.map(str::to_string),
            runtime,
            caps,
        })
        .await
    }

    /// 从磁盘恢复已有会话并附带运行时/能力/事件广播。
    pub async fn open(
        store: Arc<dyn EventStore>,
        id: SessionId,
        runtime: Arc<SessionRuntimeState>,
        caps: Arc<SessionRuntimeServices>,
    ) -> Result<Self, SessionError> {
        store.open_session(&id).await?;
        if runtime.tool_policy().is_none() {
            let model = store.session_read_model(&id).await?;
            if let Some(policy) = model.tool_policy {
                runtime.set_tool_policy(Some(policy));
            }
        }
        Ok(Self {
            id,
            store,
            runtime,
            caps,
        })
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn runtime(&self) -> &Arc<SessionRuntimeState> {
        &self.runtime
    }

    pub fn caps(&self) -> &Arc<SessionRuntimeServices> {
        &self.caps
    }

    pub async fn session_store_dir(&self) -> Option<std::path::PathBuf> {
        self.store.session_store_dir(&self.id).await.ok().flatten()
    }

    pub fn subscribe(&self) -> tokio::sync::mpsc::Receiver<astrcode_core::event::Event> {
        self.runtime.subscribe()
    }

    pub async fn read_model(
        &self,
    ) -> Result<astrcode_core::storage::SessionReadModel, SessionError> {
        Ok(self.store.session_read_model(&self.id).await?)
    }

    pub async fn current_system_prompt(&self) -> Result<Option<String>, SessionError> {
        Ok(self.store.session_system_prompt(&self.id).await?)
    }

    pub async fn latest_cursor(&self) -> Result<Option<Cursor>, SessionError> {
        Ok(self.store.latest_cursor(&self.id).await?)
    }

    pub async fn checkpoint(&self, cursor: &Cursor) -> Result<(), SessionError> {
        Ok(self.store.checkpoint(&self.id, cursor).await?)
    }

    pub async fn write_compact_snapshot(
        &self,
        snapshot: astrcode_core::storage::CompactSnapshotInput,
    ) -> Result<Option<String>, SessionError> {
        Ok(self
            .store
            .write_compact_snapshot(&self.id, snapshot)
            .await?)
    }

    pub async fn write_tool_artifact(
        &self,
        artifact: astrcode_core::storage::ToolResultArtifactInput,
    ) -> Result<astrcode_core::storage::ToolResultArtifactRef, SessionError> {
        Ok(self
            .store
            .write_tool_result_artifact(&self.id, artifact)
            .await?)
    }

    pub async fn refresh_tools(
        &self,
        working_dir: &str,
    ) -> Arc<astrcode_tools::registry::ToolRegistry> {
        let caps = &self.caps;
        let runtime = &self.runtime;
        let timeout = caps.read_effective().agent.shell_timeout_secs;
        let tool_policy = runtime.tool_policy();
        let registry = crate::session_setup::build_tool_registry_snapshot(
            caps.extension_runner(),
            working_dir,
            timeout,
            tool_policy.as_ref(),
        )
        .await;
        let registry = Arc::new(registry);
        runtime.set_tool_registry(Arc::clone(&registry));
        registry
    }

    pub async fn initialize_runtime(&self, working_dir: &str) -> Result<(), SessionError> {
        self.refresh_tools(working_dir).await;
        self.refresh_prompt(working_dir, None, None).await?;
        Ok(())
    }

    pub async fn ensure_runtime_ready(&self) -> Result<(), SessionError> {
        let state = self.read_model().await?;
        if self.runtime.tool_registry().list_definitions().is_empty() {
            self.refresh_tools(&state.working_dir).await;
        }
        if state.system_prompt.is_none() {
            self.refresh_prompt(&state.working_dir, None, None).await?;
        }
        Ok(())
    }

    pub(crate) fn resolve_shell_name() -> String {
        resolve_shell().name
    }
}

#[async_trait::async_trait]
impl ToolResultArtifactReader for Session {
    async fn read_tool_result_artifact_by_path(
        &self,
        _session_id: &SessionId,
        path: &str,
        char_offset: usize,
        max_chars: usize,
    ) -> Result<ToolResultArtifactSlice, StorageError> {
        self.store
            .read_tool_result_artifact_by_path(&self.id, path, char_offset, max_chars)
            .await
    }
}

/// 会话操作中可能出现的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("Extension error: {0}")]
    Extension(#[from] astrcode_core::extension::ExtensionError),
    #[error("{0}")]
    Other(String),
}

// Re-export lifecycle helper for external callers (session_manager).
pub use events::emit_lifecycle_for_read_model;
