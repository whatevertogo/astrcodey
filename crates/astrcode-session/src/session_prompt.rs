//! Session prompt and tool registry service.

use std::sync::Arc;

use astrcode_core::{event::SystemPromptSource, tool::SessionToolSelection, types::*};
use astrcode_extension_sdk::runtime_ports::{RuntimeSnapshotState, ToolCatalogScope};
use astrcode_session_projection::SessionReadModel;

use crate::{
    ToolRegistry,
    payload::system_prompt_configured_payload,
    runtime_stability::RuntimeStabilityBudget,
    session::Session,
    session_error::SessionError,
    session_tools::{BaseToolRegistryKey, ToolCacheLookup},
};

pub(crate) fn normalize_extra_system_prompt(extra_system_prompt: Option<&str>) -> Option<String> {
    extra_system_prompt.and_then(|prompt| {
        let trimmed = prompt.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

pub(crate) struct PreparedSystemPrompt {
    pub(crate) text: String,
    pub(crate) fingerprint: String,
    pub(crate) resolved_extra: Option<String>,
}

pub(crate) struct PreparedRuntimeSnapshot {
    pub(crate) registry: Arc<ToolRegistry>,
    pub(crate) prompt: PreparedSystemPrompt,
    pub(crate) tool_selection: Option<SessionToolSelection>,
}

pub(crate) struct ResolvedToolRegistrySnapshot {
    pub(crate) registry: Arc<ToolRegistry>,
    pub(crate) base_key: BaseToolRegistryKey,
    pub(crate) runtime_generation: u64,
}

pub(crate) async fn retry_runtime_snapshot(
    stability: &mut RuntimeStabilityBudget,
) -> Result<(), SessionError> {
    stability
        .retry_after_change()
        .await
        .map_err(|attempts| SessionError::RuntimeUnstable { attempts })
}

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

    pub(crate) async fn resolve_tool_registry_snapshot(
        &self,
        working_dir: &str,
        tool_selection: Option<&SessionToolSelection>,
        stability: &mut RuntimeStabilityBudget,
    ) -> Result<ResolvedToolRegistrySnapshot, SessionError> {
        let scope = ToolCatalogScope {
            working_dir: working_dir.to_owned(),
            session_store_dir: self.session_store_dir().await,
        };
        self.resolve_tool_registry_snapshot_for_scope(scope, tool_selection, stability)
            .await
    }

    async fn resolve_tool_registry_snapshot_for_scope(
        &self,
        scope: ToolCatalogScope,
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
            let base_key = self.base_tool_registry_key(&scope);
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
                &scope,
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
        scope: ToolCatalogScope,
        model_id: &str,
        resolved_extra: Option<&str>,
        is_subagent: bool,
        tool_selection: Option<&SessionToolSelection>,
        stability: &mut RuntimeStabilityBudget,
    ) -> Result<(PreparedSystemPrompt, ResolvedToolRegistrySnapshot), SessionError> {
        let working_dir = scope.working_dir.clone();
        loop {
            let tool_snapshot = self
                .resolve_tool_registry_snapshot_for_scope(scope.clone(), tool_selection, stability)
                .await?;
            let (text, fingerprint) = self
                .build_system_prompt(
                    &working_dir,
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

    pub(crate) async fn prepare_initial_system_prompt(
        &self,
        working_dir: &str,
        model_id: &str,
        parent_session_id: Option<&SessionId>,
        tool_selection: Option<&SessionToolSelection>,
        source_extension: Option<&str>,
        extra_system_prompt: Option<&str>,
    ) -> Result<astrcode_core::event::PersistedSystemPrompt, SessionError> {
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
                scope,
                model_id,
                resolved_extra.as_deref(),
                parent_session_id.is_some(),
                tool_selection,
                &mut stability,
            )
            .await?;
        Ok(astrcode_core::event::PersistedSystemPrompt {
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

    pub(crate) async fn prepare_runtime_snapshot(
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
                scope,
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

    pub(crate) async fn effective_tool_selection(
        &self,
        session_id: &SessionId,
        model: &SessionReadModel,
    ) -> Result<Option<SessionToolSelection>, SessionError> {
        crate::session_lifecycle::resolve_effective_tool_selection(
            &self.state_source,
            session_id,
            model,
        )
        .await
    }

    pub(crate) async fn persist_system_prompt(
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
        let tools: Vec<_> = tools_with_meta
            .iter()
            .map(|tool| tool.definition.clone())
            .collect();
        let tool_prompt_metadata = tools_with_meta
            .into_iter()
            .filter_map(|tool| tool.prompt_metadata.map(|m| (tool.definition.name, m)))
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
