use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::Arc,
};

use astrcode_core::tool::{SessionOperations, Tool};

use crate::extension::{
    CompactContext, CompactEvent, CompactResult, ContinueAfterStopContext, ContinueAfterStopResult,
    ExtensionError, ExtensionEvent, LifecycleContext, PostToolUseContext, PostToolUseResult,
    PreToolUseContext, PreToolUseResult, PromptBuildContext, PromptContributions, ProviderContext,
    ProviderEvent, ProviderResult, UserMessageEnvelopeContext, UserMessageEnvelopeResult,
};

/// Publication state shared by all runtime ports used to prepare one turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeSnapshotState {
    Stable(u64),
    Updating,
}

/// Reports whether the extension runtime can provide a coherent snapshot.
///
/// Implementations must report `Updating` before any tool or prompt state
/// becomes mutable, and publish a new stable generation after the update.
pub trait RuntimeSnapshotProvider: Send + Sync {
    fn runtime_snapshot_state(&self) -> RuntimeSnapshotState {
        RuntimeSnapshotState::Stable(0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolCatalogCompleteness {
    Complete,
    Partial,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCatalogDiagnostic {
    pub source: String,
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCatalogScope {
    pub working_dir: String,
    pub session_store_dir: Option<PathBuf>,
}

#[derive(Clone)]
pub struct ToolCatalogSnapshot {
    pub revision: u64,
    pub tools: Vec<Arc<dyn Tool>>,
    pub completeness: ToolCatalogCompleteness,
    pub diagnostics: Vec<ToolCatalogDiagnostic>,
}

impl ToolCatalogSnapshot {
    pub fn complete(revision: u64, tools: Vec<Arc<dyn Tool>>) -> Self {
        Self {
            revision,
            tools,
            completeness: ToolCatalogCompleteness::Complete,
            diagnostics: Vec::new(),
        }
    }
}

/// Supplies the tool catalog visible to a session.
#[async_trait::async_trait]
pub trait ToolCatalogProvider: Send + Sync {
    fn revision(&self) -> u64 {
        0
    }

    async fn tool_catalog(
        &self,
        _scope: &ToolCatalogScope,
    ) -> Result<ToolCatalogSnapshot, ExtensionError> {
        Ok(ToolCatalogSnapshot::complete(self.revision(), Vec::new()))
    }
}

/// 将多个完整 catalog 组合成 session 使用的单一快照。
///
/// providers 按优先级从高到低排列；重复名称保留更靠前的 provider。
pub struct CompositeToolCatalogProvider {
    providers: Vec<(String, Arc<dyn ToolCatalogProvider>)>,
}

impl CompositeToolCatalogProvider {
    pub fn new(providers: Vec<(String, Arc<dyn ToolCatalogProvider>)>) -> Self {
        Self { providers }
    }

    fn combined_revision(revisions: impl IntoIterator<Item = u64>) -> u64 {
        let mut hasher = DefaultHasher::new();
        for revision in revisions {
            revision.hash(&mut hasher);
        }
        hasher.finish()
    }
}

#[async_trait::async_trait]
impl ToolCatalogProvider for CompositeToolCatalogProvider {
    fn revision(&self) -> u64 {
        Self::combined_revision(
            self.providers
                .iter()
                .map(|(_, provider)| provider.revision()),
        )
    }

    async fn tool_catalog(
        &self,
        scope: &ToolCatalogScope,
    ) -> Result<ToolCatalogSnapshot, ExtensionError> {
        let mut revisions = Vec::with_capacity(self.providers.len());
        let mut tools = Vec::new();
        let mut diagnostics = Vec::new();
        let mut completeness = ToolCatalogCompleteness::Complete;
        let mut names = HashMap::<String, String>::new();

        for (source, provider) in &self.providers {
            let snapshot = provider.tool_catalog(scope).await?;
            revisions.push(snapshot.revision);
            if snapshot.completeness == ToolCatalogCompleteness::Partial {
                completeness = ToolCatalogCompleteness::Partial;
            }
            diagnostics.extend(snapshot.diagnostics);
            for tool in snapshot.tools {
                let name = tool.definition().name;
                if let Some(winner) = names.get(&name) {
                    diagnostics.push(ToolCatalogDiagnostic {
                        source: source.clone(),
                        message: format!(
                            "tool {name} is shadowed by higher-priority catalog {winner}"
                        ),
                    });
                    continue;
                }
                names.insert(name, source.clone());
                tools.push(tool);
            }
        }

        Ok(ToolCatalogSnapshot {
            revision: Self::combined_revision(revisions),
            tools,
            completeness,
            diagnostics,
        })
    }
}

/// Supplies prompt fragments contributed by extensions.
#[async_trait::async_trait]
pub trait PromptContributor: Send + Sync {
    async fn collect_prompt_contributions(
        &self,
        _ctx: PromptBuildContext,
    ) -> Result<PromptContributions, ExtensionError> {
        Ok(PromptContributions::default())
    }
}

/// Dispatches turn and session lifecycle hooks.
#[async_trait::async_trait]
pub trait TurnHooks: Send + Sync {
    async fn emit_pre_tool_use(
        &self,
        _ctx: PreToolUseContext,
    ) -> Result<PreToolUseResult, ExtensionError> {
        Ok(PreToolUseResult::Allow)
    }

    async fn emit_post_tool_use(
        &self,
        _ctx: PostToolUseContext,
    ) -> Result<PostToolUseResult, ExtensionError> {
        Ok(PostToolUseResult::Allow)
    }

    async fn emit_provider(
        &self,
        _event: ProviderEvent,
        _ctx: ProviderContext,
    ) -> Result<ProviderResult, ExtensionError> {
        Ok(ProviderResult::Allow)
    }

    async fn emit_compact(
        &self,
        _event: CompactEvent,
        _ctx: CompactContext,
    ) -> Result<CompactResult, ExtensionError> {
        Ok(CompactResult::Allow)
    }

    async fn emit_continue_after_stop(
        &self,
        _ctx: ContinueAfterStopContext,
    ) -> Result<ContinueAfterStopResult, ExtensionError> {
        Ok(ContinueAfterStopResult::EndTurn)
    }

    async fn emit_user_message_envelope(
        &self,
        _ctx: UserMessageEnvelopeContext,
    ) -> Result<UserMessageEnvelopeResult, ExtensionError> {
        Ok(UserMessageEnvelopeResult::Allow)
    }

    async fn emit_lifecycle(
        &self,
        _event: ExtensionEvent,
        _ctx: LifecycleContext,
    ) -> Result<(), ExtensionError> {
        Ok(())
    }
}

/// Provides late-bound session operations to tools without coupling Session
/// to a concrete server implementation.
pub trait SessionOperationsProvider: Send + Sync {
    fn session_ops(&self) -> Option<Arc<dyn SessionOperations>> {
        None
    }
}

/// Empty extension runtime for embedded hosts that do not need extensions.
pub struct NoopRuntimePorts;

impl RuntimeSnapshotProvider for NoopRuntimePorts {}

#[async_trait::async_trait]
impl ToolCatalogProvider for NoopRuntimePorts {}

#[async_trait::async_trait]
impl PromptContributor for NoopRuntimePorts {}

#[async_trait::async_trait]
impl TurnHooks for NoopRuntimePorts {}

impl SessionOperationsProvider for NoopRuntimePorts {}

#[cfg(test)]
mod tests {
    use std::{
        path::Path,
        sync::atomic::{AtomicU64, Ordering},
    };

    use astrcode_core::tool::{
        ExecutionMode, ToolDefinition, ToolError, ToolExecutionContext, ToolExecutionResult,
        ToolOrigin,
    };

    use super::*;

    struct NamedTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.0.into(),
                description: String::new(),
                parameters: serde_json::json!({"type": "object"}),
                strict: false,
                origin: ToolOrigin::Extension,
                execution_mode: ExecutionMode::Sequential,
            }
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
            _ctx: &ToolExecutionContext,
        ) -> Result<ToolExecutionResult, ToolError> {
            unreachable!("catalog tests do not execute tools")
        }
    }

    struct StaticCatalog {
        revision: AtomicU64,
        names: Vec<&'static str>,
        completeness: ToolCatalogCompleteness,
    }

    #[async_trait::async_trait]
    impl ToolCatalogProvider for StaticCatalog {
        fn revision(&self) -> u64 {
            self.revision.load(Ordering::Acquire)
        }

        async fn tool_catalog(
            &self,
            _scope: &ToolCatalogScope,
        ) -> Result<ToolCatalogSnapshot, ExtensionError> {
            Ok(ToolCatalogSnapshot {
                revision: self.revision(),
                tools: self
                    .names
                    .iter()
                    .map(|name| Arc::new(NamedTool(name)) as Arc<dyn Tool>)
                    .collect(),
                completeness: self.completeness,
                diagnostics: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn composite_catalog_tracks_revision_partial_state_and_duplicate_winners() {
        let high = Arc::new(StaticCatalog {
            revision: AtomicU64::new(1),
            names: vec!["shared"],
            completeness: ToolCatalogCompleteness::Complete,
        });
        let low = Arc::new(StaticCatalog {
            revision: AtomicU64::new(2),
            names: vec!["shared", "low_only"],
            completeness: ToolCatalogCompleteness::Partial,
        });
        let composite = CompositeToolCatalogProvider::new(vec![
            ("high".into(), high.clone()),
            ("low".into(), low),
        ]);
        let revision = composite.revision();
        let snapshot = composite
            .tool_catalog(&ToolCatalogScope {
                working_dir: Path::new(".").display().to_string(),
                session_store_dir: None,
            })
            .await
            .unwrap();

        assert_eq!(snapshot.revision, revision);
        assert_eq!(snapshot.completeness, ToolCatalogCompleteness::Partial);
        assert_eq!(
            snapshot
                .tools
                .iter()
                .map(|tool| tool.definition().name)
                .collect::<Vec<_>>(),
            ["shared", "low_only"]
        );
        assert_eq!(snapshot.diagnostics.len(), 1);
        assert_eq!(snapshot.diagnostics[0].source, "low");

        high.revision.store(3, Ordering::Release);
        assert_ne!(composite.revision(), revision);
    }
}
