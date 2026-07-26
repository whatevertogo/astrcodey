use std::sync::Arc;

use astrcode_core::{
    extension::{
        AfterToolResultsContext, AfterToolResultsResult, CompactContext, CompactEvent,
        CompactResult, ContinueAfterStopContext, ContinueAfterStopResult, ExtensionError,
        ExtensionEvent, LifecycleContext, PostToolUseContext, PostToolUseFailureContext,
        PostToolUseResult, PreToolUseContext, PreToolUseResult, PromptBuildContext,
        PromptContributions, ProviderContext, ProviderEvent, ProviderResult,
        UserMessageEnvelopeContext, UserMessageEnvelopeResult,
    },
    tool::{SessionOperations, Tool},
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
    pub extension_id: String,
    pub message: String,
}

#[derive(Clone)]
pub struct ToolCatalogSnapshot {
    pub tools: Vec<Arc<dyn Tool>>,
    pub completeness: ToolCatalogCompleteness,
    pub diagnostics: Vec<ToolCatalogDiagnostic>,
}

impl ToolCatalogSnapshot {
    pub fn complete(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self {
            tools,
            completeness: ToolCatalogCompleteness::Complete,
            diagnostics: Vec::new(),
        }
    }
}

/// Supplies the tool catalog visible to a session.
#[async_trait::async_trait]
pub trait ToolCatalogProvider: Send + Sync {
    async fn tool_catalog(
        &self,
        _working_dir: &str,
    ) -> Result<ToolCatalogSnapshot, ExtensionError> {
        Ok(ToolCatalogSnapshot::complete(Vec::new()))
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

    async fn emit_post_tool_use_failure(&self, _ctx: PostToolUseFailureContext) {}

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

    async fn emit_after_tool_results(
        &self,
        _ctx: AfterToolResultsContext,
    ) -> Result<AfterToolResultsResult, ExtensionError> {
        Ok(AfterToolResultsResult::Continue)
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
