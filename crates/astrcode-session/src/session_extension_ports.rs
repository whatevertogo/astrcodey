//! Capability-segregated extension boundary consumed by Session.

use std::sync::Arc;

use astrcode_extension_sdk::runtime_ports::{
    NoopRuntimePorts, PromptContributor, RuntimeSnapshotProvider, RuntimeSnapshotState,
    SessionOperationsProvider, ToolCatalogProvider, TurnHooks,
};

/// Groups narrow extension ports without making Session depend on a concrete
/// extension runner.
pub struct SessionExtensionPorts {
    runtime_snapshot: Arc<dyn RuntimeSnapshotProvider>,
    tool_catalog: Arc<dyn ToolCatalogProvider>,
    prompt_contributor: Arc<dyn PromptContributor>,
    turn_hooks: Arc<dyn TurnHooks>,
    session_operations: Arc<dyn SessionOperationsProvider>,
}

impl SessionExtensionPorts {
    pub fn new(
        runtime_snapshot: Arc<dyn RuntimeSnapshotProvider>,
        tool_catalog: Arc<dyn ToolCatalogProvider>,
        prompt_contributor: Arc<dyn PromptContributor>,
        turn_hooks: Arc<dyn TurnHooks>,
        session_operations: Arc<dyn SessionOperationsProvider>,
    ) -> Self {
        Self {
            runtime_snapshot,
            tool_catalog,
            prompt_contributor,
            turn_hooks,
            session_operations,
        }
    }

    pub fn from_adapter<T>(adapter: Arc<T>) -> Self
    where
        T: ToolCatalogProvider
            + PromptContributor
            + RuntimeSnapshotProvider
            + TurnHooks
            + SessionOperationsProvider
            + 'static,
    {
        Self::new(
            adapter.clone(),
            adapter.clone(),
            adapter.clone(),
            adapter.clone(),
            adapter,
        )
    }

    #[cfg(test)]
    pub(crate) fn with_turn_hooks(turn_hooks: Arc<dyn TurnHooks>) -> Self {
        let noop = Arc::new(NoopRuntimePorts);
        Self::new(noop.clone(), noop.clone(), noop.clone(), turn_hooks, noop)
    }

    pub(crate) fn runtime_snapshot_state(&self) -> RuntimeSnapshotState {
        self.runtime_snapshot.runtime_snapshot_state()
    }

    pub(crate) fn tool_catalog(&self) -> &dyn ToolCatalogProvider {
        self.tool_catalog.as_ref()
    }

    pub(crate) fn prompt_contributor(&self) -> &dyn PromptContributor {
        self.prompt_contributor.as_ref()
    }

    pub(crate) fn turn_hooks(&self) -> &dyn TurnHooks {
        self.turn_hooks.as_ref()
    }

    pub(crate) fn turn_hooks_arc(&self) -> Arc<dyn TurnHooks> {
        Arc::clone(&self.turn_hooks)
    }

    pub(crate) fn session_operations(&self) -> &dyn SessionOperationsProvider {
        self.session_operations.as_ref()
    }
}

impl Default for SessionExtensionPorts {
    fn default() -> Self {
        Self::from_adapter(Arc::new(NoopRuntimePorts))
    }
}
