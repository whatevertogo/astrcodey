use std::sync::Arc;

use astrcode_core::tool::{SessionOperations, Tool};

use crate::extension::{
    CompactEvent, CompactResult, ContinueAfterStopResult, ExtensionError, LifecycleEvent,
    PostToolUseResult, PreToolUseResult, PromptContributions, ProviderEvent, ProviderResult,
    RuntimeCompactContext, RuntimeContinueAfterStopContext, RuntimeLifecycleContext,
    RuntimePostToolUseContext, RuntimePreToolUseContext, RuntimePromptBuildContext,
    RuntimeProviderContext, RuntimeUserMessageEnvelopeContext, UserMessageEnvelopeResult,
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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ToolCatalogScope {
    pub working_dir: String,
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

/// Supplies prompt fragments contributed by extensions.
#[async_trait::async_trait]
pub trait PromptContributor: Send + Sync {
    async fn collect_prompt_contributions(
        &self,
        _ctx: RuntimePromptBuildContext,
    ) -> Result<PromptContributions, ExtensionError> {
        Ok(PromptContributions::default())
    }
}

/// Dispatches turn and session lifecycle hooks.
#[async_trait::async_trait]
pub trait TurnHooks: Send + Sync {
    async fn emit_pre_tool_use(
        &self,
        _ctx: RuntimePreToolUseContext,
    ) -> Result<PreToolUseResult, ExtensionError> {
        Ok(PreToolUseResult::Allow)
    }

    async fn emit_post_tool_use(
        &self,
        _ctx: RuntimePostToolUseContext,
    ) -> Result<PostToolUseResult, ExtensionError> {
        Ok(PostToolUseResult::Allow)
    }

    async fn emit_provider(
        &self,
        _event: ProviderEvent,
        _ctx: RuntimeProviderContext,
    ) -> Result<ProviderResult, ExtensionError> {
        Ok(ProviderResult::Allow)
    }

    async fn emit_compact(
        &self,
        _event: CompactEvent,
        _ctx: RuntimeCompactContext,
    ) -> Result<CompactResult, ExtensionError> {
        Ok(CompactResult::Allow)
    }

    async fn emit_continue_after_stop(
        &self,
        _ctx: RuntimeContinueAfterStopContext,
    ) -> Result<ContinueAfterStopResult, ExtensionError> {
        Ok(ContinueAfterStopResult::EndTurn)
    }

    async fn emit_user_message_envelope(
        &self,
        _ctx: RuntimeUserMessageEnvelopeContext,
    ) -> Result<UserMessageEnvelopeResult, ExtensionError> {
        Ok(UserMessageEnvelopeResult::Allow)
    }

    async fn emit_lifecycle(
        &self,
        _event: LifecycleEvent,
        _ctx: RuntimeLifecycleContext,
    ) -> Result<(), ExtensionError> {
        Ok(())
    }
}

/// Immutable extension-runtime generation used by one turn or standalone operation.
///
/// The three ports must describe the same published generation. Keeping them in one
/// value prevents a reload from mixing a tool catalog from one generation with prompt
/// contributions or hooks from another.
#[derive(Clone)]
pub struct TurnExtensionView {
    generation: u64,
    tool_catalog: Arc<dyn ToolCatalogProvider>,
    prompt_contributor: Arc<dyn PromptContributor>,
    turn_hooks: Arc<dyn TurnHooks>,
}

impl TurnExtensionView {
    pub fn new(
        generation: u64,
        tool_catalog: Arc<dyn ToolCatalogProvider>,
        prompt_contributor: Arc<dyn PromptContributor>,
        turn_hooks: Arc<dyn TurnHooks>,
    ) -> Self {
        Self {
            generation,
            tool_catalog,
            prompt_contributor,
            turn_hooks,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn tool_catalog(&self) -> &dyn ToolCatalogProvider {
        self.tool_catalog.as_ref()
    }

    pub fn prompt_contributor(&self) -> &dyn PromptContributor {
        self.prompt_contributor.as_ref()
    }

    pub fn turn_hooks(&self) -> &dyn TurnHooks {
        self.turn_hooks.as_ref()
    }

    pub fn turn_hooks_arc(&self) -> Arc<dyn TurnHooks> {
        Arc::clone(&self.turn_hooks)
    }
}

/// Captures one immutable extension view without exposing the concrete runner.
pub trait TurnExtensionViewProvider: RuntimeSnapshotProvider + Send + Sync {
    fn turn_extension_view(&self) -> TurnExtensionView;
}

/// Provides late-bound session operations to tools without coupling Session
/// to a concrete server implementation.
pub trait SessionOperationsProvider: Send + Sync {
    fn session_ops(&self) -> Option<Arc<dyn SessionOperations>> {
        None
    }
}

/// Empty extension runtime for sessions that do not use extensions.
pub struct NoopRuntimePorts;

impl RuntimeSnapshotProvider for NoopRuntimePorts {}

#[async_trait::async_trait]
impl ToolCatalogProvider for NoopRuntimePorts {}

#[async_trait::async_trait]
impl PromptContributor for NoopRuntimePorts {}

#[async_trait::async_trait]
impl TurnHooks for NoopRuntimePorts {}

impl TurnExtensionViewProvider for NoopRuntimePorts {
    fn turn_extension_view(&self) -> TurnExtensionView {
        let noop = Arc::new(Self);
        TurnExtensionView::new(0, noop.clone(), noop.clone(), noop)
    }
}

impl SessionOperationsProvider for NoopRuntimePorts {}
