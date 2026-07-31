//! Session 句柄 — 带存储能力的会话操作入口。

use std::{collections::HashSet, panic::AssertUnwindSafe, sync::Arc};

use astrcode_core::{
    event::{
        DurableEvent, DurableEventPayload, LiveEvent, LiveEventPayload, ParentSessionRef,
        PersistedSystemPrompt, Phase, SessionStarted, StoredEvent, SystemPromptSource,
    },
    llm::TURN_ABORTED_SOURCE,
    tool::{
        SessionToolSelection, ToolResultArtifactError, ToolResultArtifactReader,
        ToolResultArtifactSlice,
    },
    types::*,
    user_input::UserInput,
};
use astrcode_extension_sdk::{
    extension::{
        ExtensionError, ExtensionEvent, UserMessageEnvelopeContext, UserMessageEnvelopeResult,
    },
    runtime_ports::{RuntimeSnapshotState, ToolCatalogScope},
};
use astrcode_session_projection::{AgentSessionStatus, SessionReadModel};
use astrcode_storage::{
    CompactSnapshotInput, StorageError, ToolResultArtifactInput, ToolResultArtifactRef,
};
use futures_util::FutureExt;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::{
    SessionEventPublishError, ToolRegistry,
    payload::{
        JSON_RPC_INTERNAL_ERROR, TURN_FINISH_ABORTED, TURN_FINISH_ERROR,
        agent_run_completed_payload, system_prompt_configured_payload,
        transcript_rewritten_payload, turn_completed_payload,
    },
    runtime_stability::RuntimeStabilityBudget,
    session_runtime::SessionRuntimeState,
    session_runtime_services::SessionRuntimeServices,
    session_state::SessionStateSource,
    session_tools::{BaseToolRegistryKey, ToolCacheLookup},
    turn_context::{SharedTurnContext, TurnError},
    turn_handle::{SharedTurnFinalization, TurnHandle},
    turn_runner::{RunTurnResult, TurnFinalization, TurnLoop, run_turn},
};

// ── Session struct & lifecycle ──

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
    state_source: SessionStateSource,
    pub(crate) runtime: Arc<SessionRuntimeState>,
    pub(crate) runtime_services: Arc<SessionRuntimeServices>,
}

impl Session {
    /// 使用 runtime 已绑定的 session id 创建会话。
    ///
    /// **注意**：`runtime` 必须由调用方保证「同 sid 唯一」，否则同 sid 的不同 Session
    /// 实例会有不同的工具缓存与审批状态。生产路径通过 [`crate::SessionResourceStore`]
    /// 保证唯一；直接调用本入口的测试或嵌入方需维持相同约束。
    pub async fn create_with_params(mut params: SessionCreateParams) -> Result<Self, SessionError> {
        let state_source = SessionStateSource::new(params.runtime.store().clone());
        params.tool_selection = resolve_initial_tool_selection(
            &state_source,
            params.parent_session_id.as_ref(),
            params.tool_selection.as_ref(),
        )
        .await?;
        Self::create_persisted(params, state_source).await
    }

    async fn create_persisted(
        params: SessionCreateParams,
        state_source: SessionStateSource,
    ) -> Result<Self, SessionError> {
        let session = Self {
            state_source,
            runtime: Arc::clone(&params.runtime),
            runtime_services: Arc::clone(&params.runtime_services),
        };
        let model_id = params.model_id;
        let initial_system_prompt = match params.initial_system_prompt {
            Some(prompt) => prompt,
            None => {
                session
                    .prepare_initial_system_prompt(
                        &params.working_dir,
                        &model_id,
                        params.parent_session_id.as_ref(),
                        params.tool_selection.as_ref(),
                        params.source_extension.as_deref(),
                        params.extra_system_prompt.as_deref(),
                    )
                    .await?
            },
        };
        let started = DurableEvent::session(
            session.id().clone(),
            DurableEventPayload::SessionStarted(SessionStarted {
                working_dir: params.working_dir,
                model_id,
                parent: params
                    .parent_session_id
                    .map(|session_id| ParentSessionRef { session_id }),
                tool_selection: params.tool_selection.unwrap_or_default(),
                source_extension: params.source_extension,
                initial_system_prompt,
            }),
        );
        session
            .runtime
            .event_sink()
            .create(session.runtime.store().clone(), started)
            .await?;
        Ok(session)
    }

    /// 从磁盘恢复已有会话并附带运行时服务和事件广播。
    pub async fn open(
        runtime: Arc<SessionRuntimeState>,
        runtime_services: Arc<SessionRuntimeServices>,
    ) -> Result<Self, SessionError> {
        runtime.store().open_session(runtime.session_id()).await?;
        Ok(Self {
            state_source: SessionStateSource::new(runtime.store().clone()),
            runtime,
            runtime_services,
        })
    }

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
        event: ExtensionEvent,
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

/// 会话操作中可能出现的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("Event publish error: {0}")]
    EventPublish(#[from] SessionEventPublishError),
    #[error("Extension error: {0}")]
    Extension(#[from] ExtensionError),
    #[error("extension runtime changed during session preparation after {attempts} attempts")]
    RuntimeUnstable { attempts: usize },
    #[error("session parent chain contains a cycle at {session_id}")]
    ParentCycle { session_id: SessionId },
    #[error("session creation task failed: {0}")]
    CreationTask(String),
}

async fn resolve_initial_tool_selection(
    state_source: &SessionStateSource,
    parent_session_id: Option<&SessionId>,
    requested: Option<&SessionToolSelection>,
) -> Result<Option<SessionToolSelection>, SessionError> {
    let Some(parent_session_id) = parent_session_id else {
        return Ok(SessionToolSelection::intersect(None, requested));
    };
    let parent = state_source.read_model(parent_session_id).await?;
    let parent_selection =
        resolve_effective_tool_selection(state_source, parent_session_id, &parent).await?;
    Ok(SessionToolSelection::intersect(
        parent_selection.as_ref(),
        requested,
    ))
}

async fn resolve_effective_tool_selection(
    state_source: &SessionStateSource,
    session_id: &SessionId,
    model: &SessionReadModel,
) -> Result<Option<SessionToolSelection>, SessionError> {
    let mut visited = HashSet::from([session_id.clone()]);
    let mut selection = Some(model.identity.tool_selection.clone());
    let mut parent_session_id = model
        .identity
        .parent
        .as_ref()
        .map(|parent| parent.session_id.clone());

    while let Some(parent_id) = parent_session_id {
        if !visited.insert(parent_id.clone()) {
            return Err(SessionError::ParentCycle {
                session_id: parent_id,
            });
        }
        let parent = state_source.read_model(&parent_id).await?;
        selection = SessionToolSelection::intersect(
            Some(&parent.identity.tool_selection),
            selection.as_ref(),
        );
        parent_session_id = parent
            .identity
            .parent
            .as_ref()
            .map(|parent| parent.session_id.clone());
    }

    Ok(selection)
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
    ) -> Result<(), SessionError> {
        let event = LiveEvent::new(self.id().clone(), turn_id.cloned(), payload);
        self.runtime
            .event_sink()
            .publish_live_required(self.runtime.store().clone(), event)
            .await?;
        Ok(())
    }

    pub async fn emit_durable(
        &self,
        turn_id: Option<&TurnId>,
        payload: DurableEventPayload,
    ) -> Result<StoredEvent, SessionError> {
        let event = DurableEvent::new(self.id().clone(), turn_id.cloned(), payload);
        Ok(self
            .runtime
            .event_sink()
            .append(self.runtime.store().clone(), event)
            .await?)
    }

    async fn sync_durable_events(&self) -> Result<(), SessionError> {
        self.runtime
            .event_sink()
            .sync(self.runtime.store().clone(), self.id())
            .await?;
        Ok(())
    }

    pub async fn emit_lifecycle(&self, event: ExtensionEvent) -> Result<(), SessionError> {
        let model = self.read_model().await?;
        emit_lifecycle_for_read_model(&self.runtime_services, self.id(), &model, event).await
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
    event: ExtensionEvent,
) -> Result<(), SessionError> {
    let ctx = SharedTurnContext::from_read_model(session_id, model).lifecycle_ctx();
    runtime_services
        .turn_hooks()
        .emit_lifecycle(event, ctx)
        .await?;
    Ok(())
}

// ── Tool & runtime init ──

impl Session {
    /// Resolves the immutable tool registry used by one operation or turn.
    ///
    /// The registry is returned to the caller and pinned for the operation.
    /// Session state only caches immutable snapshots by runtime generation,
    /// so prompt construction, provider schemas, and execution share one
    /// exact registry without explicit invalidation.
    pub async fn tool_registry_snapshot(
        &self,
        working_dir: &str,
    ) -> Result<Arc<ToolRegistry>, SessionError> {
        let model = self.read_model().await?;
        let tool_selection = self.effective_tool_selection(self.id(), &model).await?;
        let mut stability = RuntimeStabilityBudget::new();
        Ok(self
            .resolve_tool_registry_snapshot(working_dir, tool_selection.as_ref(), &mut stability)
            .await?
            .registry)
    }
}

// ── System prompt ──

pub(crate) fn normalize_extra_system_prompt(extra_system_prompt: Option<&str>) -> Option<String> {
    extra_system_prompt.and_then(|prompt| {
        let trimmed = prompt.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

struct PreparedSystemPrompt {
    text: String,
    fingerprint: String,
    resolved_extra: Option<String>,
}

struct PreparedRuntimeSnapshot {
    registry: Arc<ToolRegistry>,
    prompt: PreparedSystemPrompt,
    tool_selection: Option<SessionToolSelection>,
}

struct ResolvedToolRegistrySnapshot {
    registry: Arc<ToolRegistry>,
    base_key: BaseToolRegistryKey,
    runtime_generation: u64,
}

async fn retry_runtime_snapshot(
    stability: &mut RuntimeStabilityBudget,
) -> Result<(), SessionError> {
    stability
        .retry_after_change()
        .await
        .map_err(|attempts| SessionError::RuntimeUnstable { attempts })
}

impl Session {
    async fn resolve_tool_registry_snapshot(
        &self,
        working_dir: &str,
        tool_selection: Option<&SessionToolSelection>,
        stability: &mut RuntimeStabilityBudget,
    ) -> Result<ResolvedToolRegistrySnapshot, SessionError> {
        let scope = ToolCatalogScope {
            working_dir: working_dir.to_owned(),
            session_store_dir: self.session_store_dir().await,
        };
        self.resolve_tool_registry_snapshot_for_scope(&scope, tool_selection, stability)
            .await
    }

    async fn resolve_tool_registry_snapshot_for_scope(
        &self,
        scope: &ToolCatalogScope,
        tool_selection: Option<&SessionToolSelection>,
        stability: &mut RuntimeStabilityBudget,
    ) -> Result<ResolvedToolRegistrySnapshot, SessionError> {
        loop {
            let RuntimeSnapshotState::Stable(runtime_generation) =
                self.runtime_services.runtime_snapshot_state()
            else {
                retry_runtime_snapshot(stability).await?;
                continue;
            };
            let base_key = self.base_tool_registry_key(scope);
            let cache = self.runtime.tool_registry_cache();
            let build = match cache.lookup_or_reserve(&base_key) {
                ToolCacheLookup::Hit(base_registry) => {
                    let registry = cache.filtered_registry(base_registry, tool_selection);
                    return Ok(ResolvedToolRegistrySnapshot {
                        registry,
                        base_key,
                        runtime_generation,
                    });
                },
                ToolCacheLookup::Wait(mut notification) => {
                    let _ = notification.changed().await;
                    continue;
                },
                ToolCacheLookup::Build(build) => build,
            };

            let built = crate::session_setup::build_base_tool_registry(
                self.runtime_services.tool_catalog(),
                scope,
            )
            .await?;
            let base_registry = Arc::new(built.registry);
            if self.runtime_services.runtime_snapshot_state()
                == RuntimeSnapshotState::Stable(runtime_generation)
                && self.runtime_services.tool_catalog().revision() == base_key.catalog_revision
                && built.revision == base_key.catalog_revision
            {
                build.complete(Arc::clone(&base_registry), built.completeness);
                let registry = cache.filtered_registry(base_registry, tool_selection);
                return Ok(ResolvedToolRegistrySnapshot {
                    registry,
                    base_key,
                    runtime_generation,
                });
            }
            drop(build);
            retry_runtime_snapshot(stability).await?;
        }
    }

    /// 在 runtime 稳定窗口内解析工具表快照并构建 system prompt；runtime 变更时重试。
    ///
    /// 返回的 prompt 与 tool_snapshot 来自同一稳定窗口，调用方按需组装各自的返回类型。
    async fn build_stable_system_prompt(
        &self,
        scope: &ToolCatalogScope,
        model_id: &str,
        resolved_extra: Option<&str>,
        is_subagent: bool,
        tool_selection: Option<&SessionToolSelection>,
        stability: &mut RuntimeStabilityBudget,
    ) -> Result<(PreparedSystemPrompt, ResolvedToolRegistrySnapshot), SessionError> {
        let working_dir = scope.working_dir.as_str();
        loop {
            let tool_snapshot = self
                .resolve_tool_registry_snapshot_for_scope(scope, tool_selection, stability)
                .await?;
            let (text, fingerprint) = self
                .build_system_prompt(
                    working_dir,
                    model_id,
                    resolved_extra,
                    is_subagent,
                    tool_snapshot.registry.as_ref(),
                )
                .await?;
            if self.runtime_services.runtime_snapshot_state()
                == RuntimeSnapshotState::Stable(tool_snapshot.runtime_generation)
                && self.runtime_services.tool_catalog().revision()
                    == tool_snapshot.base_key.catalog_revision
            {
                return Ok((
                    PreparedSystemPrompt {
                        text,
                        fingerprint,
                        resolved_extra: resolved_extra.map(str::to_owned),
                    },
                    tool_snapshot,
                ));
            }
            retry_runtime_snapshot(stability).await?;
        }
    }

    async fn prepare_initial_system_prompt(
        &self,
        working_dir: &str,
        model_id: &str,
        parent_session_id: Option<&SessionId>,
        tool_selection: Option<&SessionToolSelection>,
        source_extension: Option<&str>,
        extra_system_prompt: Option<&str>,
    ) -> Result<PersistedSystemPrompt, SessionError> {
        let planned_store_dir = self
            .runtime
            .store()
            .planned_session_store_dir(self.id(), working_dir, parent_session_id, source_extension)
            .await?;
        let scope = ToolCatalogScope {
            working_dir: working_dir.to_owned(),
            session_store_dir: planned_store_dir,
        };
        let resolved_extra = normalize_extra_system_prompt(extra_system_prompt);
        let mut stability = RuntimeStabilityBudget::new();
        let (prompt, _tool_snapshot) = self
            .build_stable_system_prompt(
                &scope,
                model_id,
                resolved_extra.as_deref(),
                parent_session_id.is_some(),
                tool_selection,
                &mut stability,
            )
            .await?;
        Ok(PersistedSystemPrompt {
            text: prompt.text,
            fingerprint: prompt.fingerprint,
            extra_system_prompt: prompt.resolved_extra,
            source: SystemPromptSource::Native,
        })
    }

    fn base_tool_registry_key(&self, scope: &ToolCatalogScope) -> BaseToolRegistryKey {
        BaseToolRegistryKey {
            catalog_revision: self.runtime_services.tool_catalog().revision(),
            working_dir: scope.working_dir.clone(),
            session_store_dir: scope.session_store_dir.clone(),
        }
    }

    async fn prepare_runtime_snapshot(
        &self,
        working_dir: &str,
        state: &SessionReadModel,
        model_id: &str,
    ) -> Result<PreparedRuntimeSnapshot, SessionError> {
        let resolved_extra = state.system_prompt.extra.clone();
        let is_subagent = state.identity.parent.is_some();
        let tool_selection = self.effective_tool_selection(self.id(), state).await?;
        let scope = ToolCatalogScope {
            working_dir: working_dir.to_owned(),
            session_store_dir: self.session_store_dir().await,
        };
        let mut stability = RuntimeStabilityBudget::new();
        let (prompt, tool_snapshot) = self
            .build_stable_system_prompt(
                &scope,
                model_id,
                resolved_extra.as_deref(),
                is_subagent,
                tool_selection.as_ref(),
                &mut stability,
            )
            .await?;
        Ok(PreparedRuntimeSnapshot {
            registry: tool_snapshot.registry,
            prompt,
            tool_selection,
        })
    }

    async fn effective_tool_selection(
        &self,
        session_id: &SessionId,
        model: &SessionReadModel,
    ) -> Result<Option<SessionToolSelection>, SessionError> {
        resolve_effective_tool_selection(&self.state_source, session_id, model).await
    }

    async fn persist_system_prompt(
        &self,
        prepared: PreparedSystemPrompt,
        stored_fingerprint: Option<&str>,
    ) -> Result<bool, SessionError> {
        if stored_fingerprint == Some(prepared.fingerprint.as_str()) {
            return Ok(false);
        }

        self.emit_durable(
            None,
            system_prompt_configured_payload(
                prepared.text,
                prepared.fingerprint,
                prepared.resolved_extra,
                SystemPromptSource::Native,
            ),
        )
        .await?;
        Ok(true)
    }

    async fn build_system_prompt(
        &self,
        working_dir: &str,
        model_id: &str,
        resolved_extra: Option<&str>,
        is_subagent: bool,
        tool_registry: &ToolRegistry,
    ) -> Result<(String, String), SessionError> {
        let tools_with_meta = tool_registry.list_definitions_with_prompt_metadata();
        let tools: Vec<_> = tools_with_meta.iter().map(|(def, _)| def.clone()).collect();
        let tool_prompt_metadata = tools_with_meta
            .into_iter()
            .filter_map(|(def, meta)| meta.map(|m| (def.name, m)))
            .collect();
        Ok(crate::session_setup::build_system_prompt_snapshot(
            crate::session_setup::SystemPromptSnapshotInput {
                prompt_contributor: self.runtime_services.prompt_contributor(),
                session_id: self.id().as_str(),
                working_dir,
                model_id,
                tools: &tools,
                extra_system_prompt: resolved_extra,
                tool_prompt_metadata,
                include_agents_rules: !is_subagent,
            },
        )
        .await?)
    }
}

// ── Transcript rewrite ──

impl Session {
    /// 记录 compact 并重写同一 session 的 provider transcript 前缀。
    pub async fn rewrite_transcript_for_compaction(
        &self,
        trigger_name: String,
        compaction: astrcode_context::CompactResult,
        source_seq: u64,
        strategy: astrcode_core::compaction::CompactStrategy,
    ) -> Result<StoredEvent, SessionError> {
        let event = self
            .emit_durable(
                None,
                transcript_rewritten_payload(trigger_name, &compaction, source_seq, strategy),
            )
            .await?;
        if let Err(error) = self.checkpoint(&event.seq.to_string()).await {
            tracing::warn!(
                session_id = %self.id(),
                seq = event.seq,
                error = %error,
                "transcript rewrite committed but checkpoint was skipped"
            );
        }
        Ok(event)
    }
}

// ── Child session ──
// spawn_child 与 AgentSessionSpawned 事件。
// 完成等待、终态写入、回收与通知由 `astrcode-server::child_session` 编排。

/// 创建子会话所需的参数。
pub struct SpawnChildParams {
    pub working_dir: String,
    pub model_id: String,
    pub agent_name: String,
    pub task: String,
    pub extra_system_prompt: Option<String>,
    pub tool_selection: Option<SessionToolSelection>,
    pub source_extension: Option<String>,
    pub tool_call_id: ToolCallId,
}

impl Session {
    pub async fn spawn_child(&self, mut params: SpawnChildParams) -> Result<Self, SessionError> {
        params.tool_selection = resolve_initial_tool_selection(
            &self.state_source,
            Some(self.id()),
            params.tool_selection.as_ref(),
        )
        .await?;
        let child_session_id = new_session_id();
        let publication = self
            .runtime
            .event_sink()
            .defer_publication(child_session_id.clone())?;
        match AssertUnwindSafe(self.create_child_transaction(params, child_session_id.clone()))
            .catch_unwind()
            .await
        {
            Ok(Ok(child)) => {
                publication.commit();
                Ok(child)
            },
            Ok(Err(error)) => Err(error),
            Err(_) => {
                let cause = SessionError::CreationTask(
                    "child session creation transaction panicked".into(),
                );
                let compensation = AssertUnwindSafe(
                    self.compensate_panicked_child_creation(&child_session_id, &cause),
                )
                .catch_unwind()
                .await;
                if compensation.is_err() {
                    tracing::warn!(
                        parent_session_id = %self.id(),
                        child_session_id = %child_session_id,
                        "child creation panic compensation panicked"
                    );
                }
                Err(cause)
            },
        }
    }

    /// 子会话 runtime 在 shared resource store 中的唯一实例。
    fn child_runtime(&self, child_sid: &SessionId) -> Arc<SessionRuntimeState> {
        let child_store = self.runtime.store().clone();
        self.runtime_services
            .session_resources()
            .resources_for(child_sid, || {
                Arc::new(SessionRuntimeState::new_with_event_sink(
                    child_sid.clone(),
                    child_store,
                    self.runtime.event_sink_arc(),
                ))
            })
    }

    async fn create_child_transaction(
        &self,
        params: SpawnChildParams,
        child_sid: SessionId,
    ) -> Result<Self, SessionError> {
        let SpawnChildParams {
            working_dir,
            model_id,
            agent_name,
            task,
            extra_system_prompt,
            tool_selection,
            source_extension,
            tool_call_id,
        } = params;
        let child_runtime = self.child_runtime(&child_sid);
        let creation = child_runtime.begin_creation();
        let child = match Session::create_persisted(
            SessionCreateParams {
                working_dir,
                model_id,
                parent_session_id: Some(self.id().clone()),
                tool_selection: tool_selection.clone(),
                source_extension,
                extra_system_prompt,
                initial_system_prompt: None,
                runtime: Arc::clone(&child_runtime),
                runtime_services: Arc::clone(&self.runtime_services),
            },
            self.state_source.clone(),
        )
        .await
        {
            Ok(child) => child,
            Err(error) => {
                if matches!(&error, SessionError::EventPublish(_)) {
                    if let Err(compensation_error) =
                        self.discard_failed_child_runtime(&child_runtime).await
                    {
                        tracing::warn!(
                            parent_session_id = %self.id(),
                            child_session_id = %child_sid,
                            error = %error,
                            compensation_error = %compensation_error,
                            "failed to fully compensate child session creation"
                        );
                    }
                } else {
                    self.runtime_services
                        .session_resources()
                        .cleanup(&child_sid);
                }
                return Err(error);
            },
        };

        // 父链接是 child 进入父投影的持久可见边界；初始化失败时不能留下 Running 链接。
        if let Err(error) = child
            .ensure_lifecycle_initialized(ExtensionEvent::SessionStart)
            .await
        {
            self.compensate_failed_child_creation(&child, &child_sid, &error, false)
                .await;
            return Err(error);
        }

        if let Err(error) = child.sync_durable_events().await {
            self.compensate_failed_child_creation(&child, &child_sid, &error, false)
                .await;
            return Err(error);
        }

        if let Err(error) = self
            .emit_durable(
                None,
                DurableEventPayload::AgentSessionSpawned {
                    child_session_id: child_sid.clone(),
                    agent_name,
                    task,
                    tool_selection,
                    tool_call_id,
                },
            )
            .await
        {
            self.compensate_failed_child_creation(&child, &child_sid, &error, false)
                .await;
            return Err(error);
        }
        if let Err(error) = self.sync_durable_events().await {
            self.compensate_failed_child_creation(&child, &child_sid, &error, true)
                .await;
            return Err(error);
        }
        creation.commit();
        Ok(child)
    }

    async fn compensate_panicked_child_creation(
        &self,
        child_session_id: &SessionId,
        cause: &SessionError,
    ) {
        let child_runtime = self.child_runtime(child_session_id);
        let child = match Session::open(
            Arc::clone(&child_runtime),
            Arc::clone(&self.runtime_services),
        )
        .await
        {
            Ok(child) => child,
            Err(_) => {
                if let Err(error) = self.discard_failed_child_runtime(&child_runtime).await {
                    tracing::warn!(
                        parent_session_id = %self.id(),
                        child_session_id = %child_session_id,
                        error = %cause,
                        compensation_error = %error,
                        "failed to fully compensate panicked child session creation"
                    );
                }
                return;
            },
        };
        let parent_linked = match self.read_model().await {
            Ok(parent) => parent.agent_sessions.iter().any(|link| {
                link.child_session_id == *child_session_id
                    && link.status == AgentSessionStatus::Running
            }),
            Err(error) => {
                tracing::warn!(
                    parent_session_id = %self.id(),
                    child_session_id = %child_session_id,
                    error = %error,
                    "could not inspect parent link during child creation compensation"
                );
                true
            },
        };
        self.compensate_failed_child_creation(&child, child_session_id, cause, parent_linked)
            .await;
    }

    async fn compensate_failed_child_creation(
        &self,
        child: &Self,
        child_session_id: &SessionId,
        cause: &SessionError,
        parent_linked: bool,
    ) {
        let mut compensation_errors = Vec::new();
        let mut parent_link_settled = !parent_linked;
        if parent_linked {
            let terminal = match self
                .emit_durable(
                    None,
                    crate::payload::agent_session_failed_payload(
                        child_session_id.clone(),
                        format!("child creation did not commit: {cause}"),
                    ),
                )
                .await
            {
                Ok(_) => self.sync_durable_events().await,
                Err(error) => Err(error),
            };
            match terminal {
                Ok(()) => parent_link_settled = true,
                Err(error) => {
                    compensation_errors.push(format!("settle parent child link: {error}"));
                },
            }
        }
        match AssertUnwindSafe(child.emit_lifecycle(ExtensionEvent::SessionShutdown))
            .catch_unwind()
            .await
        {
            Ok(Ok(())) => {},
            Ok(Err(error)) => {
                compensation_errors.push(format!("run child shutdown hooks: {error}"));
            },
            Err(_) => compensation_errors.push("child shutdown hooks panicked".into()),
        }
        if parent_link_settled {
            if let Err(error) = child.discard_failed_creation().await {
                compensation_errors.push(error);
            }
        }

        if !compensation_errors.is_empty() {
            tracing::warn!(
                parent_session_id = %self.id(),
                child_session_id = %child_session_id,
                error = %cause,
                compensation_error = %compensation_errors.join("; "),
                "failed to fully compensate child session creation"
            );
        }
    }

    async fn discard_failed_creation(&self) -> Result<(), String> {
        self.discard_failed_child_runtime(&self.runtime).await
    }

    async fn discard_failed_child_runtime(
        &self,
        child_runtime: &SessionRuntimeState,
    ) -> Result<(), String> {
        let release_result = child_runtime
            .event_sink()
            .release(child_runtime.store().as_ref(), child_runtime.session_id())
            .await;
        let delete_result = child_runtime
            .store()
            .delete_session(child_runtime.session_id())
            .await;
        if delete_result.is_ok() {
            self.runtime_services
                .session_resources()
                .cleanup(child_runtime.session_id());
        }

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

// ── Turn submission ──

impl Session {
    async fn emit_turn_start_events(
        &self,
        text: &str,
        attachments: &[astrcode_core::message_attachment::MessageAttachment],
        turn_id: &TurnId,
        accepted_seq: Option<u64>,
    ) -> Result<(), TurnError> {
        self.emit_durable(Some(turn_id), DurableEventPayload::TurnStarted)
            .await?;
        self.emit_durable(
            Some(turn_id),
            DurableEventPayload::UserMessage {
                message_id: new_message_id(),
                text: text.to_string(),
                attachments: attachments.to_vec(),
                accepted_seq,
            },
        )
        .await?;
        self.emit_live(Some(turn_id), LiveEventPayload::AgentRunStarted);
        Ok(())
    }

    async fn apply_user_message_envelope(
        &self,
        text: String,
        attachments: &[astrcode_core::message_attachment::MessageAttachment],
        turn_id: &TurnId,
    ) -> Result<String, TurnError> {
        let state = self.read_model().await?;
        let original_text = text.clone();
        let ctx = UserMessageEnvelopeContext {
            session_id: self.id().to_string(),
            turn_id: turn_id.to_string(),
            working_dir: state.identity.working_dir.clone(),
            model: astrcode_core::config::ModelSelection::simple(&state.identity.model_id),
            text,
            attachments: attachments.to_vec(),
            session_store_dir: self.session_store_dir().await,
        };
        match self
            .runtime_services()
            .turn_hooks_arc()
            .emit_user_message_envelope(ctx)
            .await?
        {
            UserMessageEnvelopeResult::Allow => Ok(original_text),
            UserMessageEnvelopeResult::ReplaceText { text } => Ok(text),
            UserMessageEnvelopeResult::AppendText { text } => {
                let mut combined = original_text;
                if !combined.is_empty() && !text.is_empty() {
                    combined.push_str("\n\n");
                }
                combined.push_str(&text);
                Ok(combined)
            },
            UserMessageEnvelopeResult::Block { reason } => Err(TurnError::InputBlocked { reason }),
        }
    }

    async fn prepare_turn_runner(&self) -> Result<TurnLoop, TurnError> {
        let session_store_dir = self
            .runtime
            .store()
            .session_store_dir(self.id())
            .await
            .map_err(SessionError::from)?;
        let approval_history = self.runtime.approval_history();
        let approval_history_path = session_store_dir
            .as_deref()
            .map(crate::permission::approval_history_path);
        approval_history
            .ensure_loaded(approval_history_path.as_deref())
            .await
            .map_err(|error| TurnError::ApprovalHistory(error.to_string()))?;

        let mut pre_state = self.read_model().await?;
        let model_id = if pre_state.identity.parent.is_some() {
            pre_state.identity.model_id.clone()
        } else {
            self.runtime_services.read_effective().llm.model_id.clone()
        };
        if pre_state.identity.model_id != model_id {
            self.emit_durable(
                None,
                DurableEventPayload::ModelIdChanged {
                    model_id: model_id.clone(),
                },
            )
            .await?;
            pre_state = self.read_model().await?;
        }
        let llm = self.runtime_services.llm_for_model_id(&model_id);
        let working_dir = pre_state.identity.working_dir.clone();
        let (registry, tool_selection, prompt_changed) =
            if pre_state.system_prompt.source == SystemPromptSource::Inherited {
                let tool_selection = self.effective_tool_selection(self.id(), &pre_state).await?;
                let mut stability = RuntimeStabilityBudget::new();
                let tool_snapshot = self
                    .resolve_tool_registry_snapshot(
                        &working_dir,
                        tool_selection.as_ref(),
                        &mut stability,
                    )
                    .await?;
                (tool_snapshot.registry, tool_selection, false)
            } else {
                let stored_fingerprint = pre_state.system_prompt.fingerprint.clone();
                let prepared = self
                    .prepare_runtime_snapshot(&working_dir, &pre_state, &model_id)
                    .await?;
                let prompt_changed = self
                    .persist_system_prompt(prepared.prompt, Some(&stored_fingerprint))
                    .await?;
                (prepared.registry, prepared.tool_selection, prompt_changed)
            };

        let session_state = if prompt_changed {
            // Prompt 刷新可能写入 durable event，需重读 projection。
            self.read_model().await?
        } else {
            pre_state
        };
        let cancellation_token = CancellationToken::new();
        TurnLoop::new_with_llm(
            self.clone(),
            &session_state,
            tool_selection.unwrap_or_default(),
            session_store_dir,
            llm,
            registry,
            cancellation_token,
        )
    }

    async fn run_and_finalize_turn(
        session: Session,
        mut agent: TurnLoop,
        text: String,
        turn_id: TurnId,
        cancellation_token: CancellationToken,
        completion_tx: oneshot::Sender<RunTurnResult>,
        finalization_state: SharedTurnFinalization,
    ) {
        let mut result = run_turn(&mut agent, &text, &turn_id).await;
        match finalize_turn(&session, &turn_id, &result.finalization).await {
            Ok(()) => result.finalization.terminal_persisted = true,
            Err(error) => {
                tracing::error!(
                    session_id = %session.id(),
                    turn_id = %turn_id,
                    %error,
                    finish_reason = %result.finalization.finish_reason,
                    "failed to persist turn finalization; registry must retain ownership for retry"
                );
            },
        }
        cancellation_token.cancel();
        *finalization_state.lock() = Some(result.finalization.clone());
        let _ = completion_tx.send(result);
    }

    pub async fn submit(
        &self,
        input: UserInput,
        turn_id: TurnId,
        accepted_seq: Option<u64>,
    ) -> Result<TurnHandle, TurnError> {
        let UserInput { text, attachments } = input;
        let text = self
            .apply_user_message_envelope(text, &attachments, &turn_id)
            .await?;
        self.emit_turn_start_events(&text, &attachments, &turn_id, accepted_seq)
            .await?;
        let agent = match self.prepare_turn_runner().await {
            Ok(agent) => agent,
            Err(error) => {
                self.settle_failed_turn_setup(&turn_id, &error).await;
                return Err(error);
            },
        };
        let cancellation_token = agent.cancellation_token();
        let (completion_tx, completion_rx) = oneshot::channel();
        let turn_id_for_task = turn_id.clone();
        let session_for_completion = self.clone();
        let cancellation_for_task = cancellation_token.clone();
        let finalization_state = Arc::new(parking_lot::Mutex::new(None));
        let finalization_for_task = Arc::clone(&finalization_state);
        let join = tokio::spawn(async move {
            Self::run_and_finalize_turn(
                session_for_completion,
                agent,
                text,
                turn_id_for_task,
                cancellation_for_task,
                completion_tx,
                finalization_for_task,
            )
            .await;
        });

        Ok(TurnHandle::new(
            turn_id,
            join,
            cancellation_token,
            completion_rx,
            finalization_state,
        ))
    }

    async fn settle_failed_turn_setup(&self, turn_id: &TurnId, error: &TurnError) {
        if let Err(persist_error) = self
            .emit_durable(
                Some(turn_id),
                DurableEventPayload::ErrorOccurred {
                    code: JSON_RPC_INTERNAL_ERROR,
                    message: error.to_string(),
                    recoverable: false,
                },
            )
            .await
        {
            tracing::error!(
                session_id = %self.id(),
                %turn_id,
                error = %persist_error,
                "failed to persist turn setup error"
            );
        }
        if let Err(persist_error) = self
            .emit_durable(Some(turn_id), turn_completed_payload(TURN_FINISH_ERROR))
            .await
        {
            tracing::error!(
                session_id = %self.id(),
                %turn_id,
                error = %persist_error,
                "failed to complete turn after setup error"
            );
        }
        self.emit_live(
            Some(turn_id),
            agent_run_completed_payload(TURN_FINISH_ERROR),
        );
    }
}

pub async fn finalize_aborted_turn(
    session: &Session,
    turn_id: &TurnId,
) -> Result<(), SessionError> {
    let state = session.read_model().await?;
    if state.execution.phase == Phase::Idle {
        return Ok(());
    }
    emit_interrupted_tool_results(
        session,
        &state,
        Some(turn_id),
        InterruptedToolOutcome::Cancelled,
    )
    .await?;
    let has_aborted_context = state
        .transcript
        .messages
        .last()
        .and_then(|message| message.source.as_deref())
        == Some(TURN_ABORTED_SOURCE);
    if !has_aborted_context {
        emit_turn_aborted_context(session, Some(turn_id)).await?;
    }
    emit_turn_completed(session, turn_id, TURN_FINISH_ABORTED).await
}

pub async fn finalize_turn(
    session: &Session,
    turn_id: &TurnId,
    finalization: &TurnFinalization,
) -> Result<(), SessionError> {
    if finalization.aborted {
        return finalize_aborted_turn(session, turn_id).await;
    }

    let state = session.read_model().await?;
    if state.execution.phase == Phase::Idle {
        return Ok(());
    }
    if let Some(message) = &finalization.pending_error {
        if state.execution.phase != Phase::Error {
            session
                .emit_durable(
                    Some(turn_id),
                    DurableEventPayload::ErrorOccurred {
                        code: JSON_RPC_INTERNAL_ERROR,
                        message: message.clone(),
                        recoverable: false,
                    },
                )
                .await?;
        }
    }
    emit_turn_completed(session, turn_id, &finalization.finish_reason).await
}

/// 持久化 turn 完成事件并发送对应 live 事件。
async fn emit_turn_completed(
    session: &Session,
    turn_id: &TurnId,
    finish_reason: &str,
) -> Result<(), SessionError> {
    session
        .emit_durable(Some(turn_id), turn_completed_payload(finish_reason))
        .await?;
    session.emit_live(Some(turn_id), agent_run_completed_payload(finish_reason));
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptedToolOutcome {
    Failed,
    Cancelled,
}

pub async fn emit_interrupted_tool_results(
    session: &Session,
    state: &SessionReadModel,
    turn_id: Option<&TurnId>,
    outcome: InterruptedToolOutcome,
) -> Result<usize, SessionError> {
    let mut emitted = 0;
    for pending in state.tool_calls_needing_interruption() {
        let payload = match outcome {
            InterruptedToolOutcome::Failed => DurableEventPayload::ToolCallFailed {
                call_id: pending.call_id.into(),
                tool_name: pending.tool_name,
                error: "tool execution interrupted before completion".into(),
                metadata: Default::default(),
                duration_ms: None,
                arguments: String::new(),
                arguments_json: None,
            },
            InterruptedToolOutcome::Cancelled => DurableEventPayload::ToolCallCancelled {
                call_id: pending.call_id.into(),
                tool_name: pending.tool_name,
                reason: "turn aborted".into(),
                duration_ms: None,
                arguments: String::new(),
                arguments_json: None,
            },
        };
        session.emit_durable(turn_id, payload).await?;
        emitted += 1;
    }
    Ok(emitted)
}

pub async fn emit_turn_aborted_context(
    session: &Session,
    turn_id: Option<&TurnId>,
) -> Result<(), SessionError> {
    session
        .emit_durable(turn_id, DurableEventPayload::TurnAbortedContext)
        .await?;
    Ok(())
}
