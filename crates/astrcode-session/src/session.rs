//! Session 句柄 — 带存储能力的会话操作入口。

use std::sync::Arc;

use astrcode_core::{
    event::{
        DurableEvent, DurableEventPayload, LiveEvent, LiveEventPayload, PersistedSystemPrompt,
    },
    tool::{
        SessionToolSelection, ToolResultArtifactError, ToolResultArtifactReader,
        ToolResultArtifactSlice,
    },
    types::*,
};
use astrcode_extension_sdk::extension::LifecycleEvent;
use astrcode_session_projection::SessionReadModel;
use astrcode_storage::{
    CompactSnapshotInput, StorageError, ToolResultArtifactInput, ToolResultArtifactRef,
};

use crate::{
    SessionEventPublishError, session_error::SessionError, session_runtime::SessionRuntimeState,
    session_runtime_services::SessionRuntimeServices, session_state::SessionStateSource,
    turn_context::SharedTurnContext,
};

/// 创建 session 所需的参数集合。
#[derive(Clone)]
pub struct SessionCreateParams {
    pub working_dir: String,
    pub model_id: String,
    pub parent_session_id: Option<SessionId>,
    pub tool_selection: Option<SessionToolSelection>,
    pub source_extension: Option<String>,
    pub extra_system_prompt: Option<String>,
    /// 仅 fork 等需要精确继承 prompt 的创建路径设置；普通创建由 session 自行组装。
    pub initial_system_prompt: Option<PersistedSystemPrompt>,
    pub runtime: Arc<SessionRuntimeState>,
    pub runtime_services: Arc<SessionRuntimeServices>,
}

/// 会话句柄 — 带存储能力的会话操作入口。
///
/// 字段语义：
/// - `runtime`：按 sid 共享的进程内瞬态资源与有序事件写入入口。
/// - `runtime_services`：跨 session 共享的基础设施（LLM、扩展、上下文组装器、配置）。
///
/// `Clone` 是廉价的 Arc clone，可以自由复制。
#[derive(Clone)]
pub struct Session {
    pub(crate) state_source: SessionStateSource,
    pub(crate) runtime: Arc<SessionRuntimeState>,
    pub(crate) runtime_services: Arc<SessionRuntimeServices>,
}

impl Session {
    pub fn id(&self) -> &SessionId {
        self.runtime.session_id()
    }

    pub fn runtime(&self) -> &SessionRuntimeState {
        &self.runtime
    }

    pub(crate) fn runtime_services(&self) -> &SessionRuntimeServices {
        &self.runtime_services
    }

    pub async fn ensure_lifecycle_initialized(
        &self,
        event: LifecycleEvent,
    ) -> Result<(), SessionError> {
        self.runtime
            .ensure_lifecycle_initialized(|| self.emit_lifecycle(event))
            .await
    }

    pub async fn session_store_dir(&self) -> Option<std::path::PathBuf> {
        self.runtime
            .store()
            .session_store_dir(self.id())
            .await
            .ok()
            .flatten()
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
    ) -> Result<ToolResultArtifactSlice, ToolResultArtifactError> {
        self.runtime
            .store()
            .read_tool_result_artifact_by_path(self.id(), path, char_offset, max_chars)
            .await
            .map_err(|error| match error {
                StorageError::InvalidId(message) => ToolResultArtifactError::InvalidPath(message),
                StorageError::NotFound(_) => ToolResultArtifactError::NotFound(path.to_owned()),
                StorageError::Unsupported(message) => ToolResultArtifactError::Unsupported(message),
                error => ToolResultArtifactError::Read(error.to_string()),
            })
    }
}

// ── Storage operations ──

impl Session {
    pub async fn read_model(&self) -> Result<Arc<SessionReadModel>, SessionError> {
        Ok(self.state_source.read_model(self.id()).await?)
    }

    pub async fn latest_cursor(&self) -> Result<Option<Cursor>, SessionError> {
        Ok(self.state_source.latest_cursor(self.id()).await?)
    }

    pub async fn checkpoint(&self, cursor: &Cursor) -> Result<(), SessionError> {
        Ok(self.runtime.store().checkpoint(self.id(), cursor).await?)
    }

    pub async fn write_compact_snapshot(
        &self,
        snapshot: CompactSnapshotInput,
    ) -> Result<Option<String>, SessionError> {
        Ok(self
            .runtime
            .store()
            .write_compact_snapshot(self.id(), snapshot)
            .await?)
    }

    pub async fn write_tool_artifact(
        &self,
        artifact: ToolResultArtifactInput,
    ) -> Result<ToolResultArtifactRef, SessionError> {
        Ok(self
            .runtime
            .store()
            .write_tool_result_artifact(self.id(), artifact)
            .await?)
    }
}

// ── Event emission ──

impl Session {
    pub fn emit_live(&self, turn_id: Option<&TurnId>, payload: LiveEventPayload) {
        let event = LiveEvent::new(self.id().clone(), turn_id.cloned(), payload);
        if let Err(error) = self
            .runtime
            .event_sink()
            .publish_live(self.runtime.store().clone(), event)
        {
            // best-effort 事件丢弃是常态；仅按 2 的幂次（1、2、4…）记录，避免刷屏。
            if matches!(
                &error,
                SessionEventPublishError::Full { dropped } if !dropped.is_power_of_two()
            ) {
                return;
            }
            tracing::warn!(
                session_id = %self.id(),
                %error,
                "failed to publish best-effort live event"
            );
        }
    }

    pub(crate) async fn emit_live_required(
        &self,
        turn_id: Option<&TurnId>,
        payload: LiveEventPayload,
    ) -> Result<astrcode_core::types::EventId, SessionError> {
        // 与 `session_runtime.rs` 的 `SessionScopedEventPublisher::send_confirmed`
        // Live 分支平行：成功只表示事件已进入有序 lane，observer 派发是异步的。
        let event = LiveEvent::new(self.id().clone(), turn_id.cloned(), payload);
        let event_id = event.id.clone();
        self.runtime
            .event_sink()
            .publish_live_required(self.runtime.store().clone(), event)
            .await?;
        Ok(event_id)
    }

    pub async fn emit_durable(
        &self,
        turn_id: Option<&TurnId>,
        payload: DurableEventPayload,
    ) -> Result<astrcode_core::event::StoredEvent, SessionError> {
        let event = DurableEvent::new(self.id().clone(), turn_id.cloned(), payload);
        Ok(self
            .runtime
            .event_sink()
            .append(self.runtime.store().clone(), event)
            .await?)
    }

    pub(crate) async fn sync_durable_events(&self) -> Result<(), SessionError> {
        self.runtime
            .event_sink()
            .sync(self.runtime.store().clone(), self.id())
            .await?;
        Ok(())
    }

    pub async fn emit_lifecycle(&self, event: LifecycleEvent) -> Result<(), SessionError> {
        let model = self.read_model().await?;
        emit_lifecycle_for_read_model(
            &self.runtime_services,
            self.id(),
            &model,
            self.session_store_dir().await,
            event,
        )
        .await
    }

    /// 配置后续 turn 使用的模型。活跃 turn 保留已固定的不可变快照。
    pub async fn configure_model(&self, model_id: String) -> Result<bool, SessionError> {
        if self.read_model().await?.identity.model_id == model_id {
            return Ok(false);
        }

        self.emit_durable(None, DurableEventPayload::ModelIdChanged { model_id })
            .await?;
        Ok(true)
    }

    /// 配置后续 turn 使用的工具边界。
    ///
    /// 子 session 不能扩大父 session 当前边界；活跃 turn 保留已固定的不可变快照。
    pub async fn configure_tools(
        &self,
        requested: SessionToolSelection,
    ) -> Result<SessionToolSelection, SessionError> {
        let model = self.read_model().await?;
        let parent_selection = match model.identity.parent.as_ref() {
            Some(parent) => {
                let parent_session_id = &parent.session_id;
                let parent_model = self.state_source.read_model(parent_session_id).await?;
                self.effective_tool_selection(parent_session_id, &parent_model)
                    .await?
            },
            None => None,
        };
        let tool_selection = SessionToolSelection::restrict(parent_selection.as_ref(), &requested);
        self.emit_durable(
            None,
            DurableEventPayload::SessionToolsConfigured {
                selection: tool_selection.clone(),
            },
        )
        .await?;
        Ok(tool_selection)
    }
}

/// 发射 session 生命周期事件，不要求构造完整 [`Session`]。
pub async fn emit_lifecycle_for_read_model(
    runtime_services: &SessionRuntimeServices,
    session_id: &SessionId,
    model: &SessionReadModel,
    session_store_dir: Option<std::path::PathBuf>,
    event: LifecycleEvent,
) -> Result<(), SessionError> {
    let ctx =
        SharedTurnContext::from_read_model(session_id, model, session_store_dir).lifecycle_ctx();
    runtime_services
        .turn_runtime_view()
        .await?
        .turn_hooks()
        .emit_lifecycle(event, ctx)
        .await?;
    Ok(())
}
