//! Capability-segregated extension boundary consumed by Session.

use std::sync::Arc;

use astrcode_extension_sdk::runtime_ports::{
    NoopRuntimePorts, PromptContributor, RuntimeSnapshotProvider, RuntimeSnapshotState,
    SessionOperationsProvider, TurnExtensionView, TurnExtensionViewProvider, TurnHooks,
};

struct FixedTurnExtensionViewProvider {
    view: TurnExtensionView,
}

impl RuntimeSnapshotProvider for FixedTurnExtensionViewProvider {}

impl TurnExtensionViewProvider for FixedTurnExtensionViewProvider {
    fn turn_extension_view(&self) -> TurnExtensionView {
        self.view.clone()
    }
}

/// Groups narrow extension ports without making Session depend on a concrete
/// extension runner.
pub struct SessionExtensionPorts {
    turn_view_provider: Arc<dyn TurnExtensionViewProvider>,
    session_operations: Arc<dyn SessionOperationsProvider>,
}

impl SessionExtensionPorts {
    /// Combines ports whose observable state never changes after construction.
    ///
    /// 传入端口在构造后不可变，因此它们共享 generation 0 的固定视图。
    pub fn from_immutable_ports(
        prompt_contributor: Arc<dyn PromptContributor>,
        turn_hooks: Arc<dyn TurnHooks>,
        session_operations: Arc<dyn SessionOperationsProvider>,
    ) -> Self {
        let noop = Arc::new(NoopRuntimePorts);
        let view = TurnExtensionView::new(
            0,
            noop,
            Arc::clone(&prompt_contributor),
            Arc::clone(&turn_hooks),
        );
        Self {
            turn_view_provider: Arc::new(FixedTurnExtensionViewProvider { view }),
            session_operations,
        }
    }

    pub fn from_adapter<T>(adapter: Arc<T>) -> Self
    where
        T: TurnExtensionViewProvider + SessionOperationsProvider + 'static,
    {
        Self {
            turn_view_provider: adapter.clone(),
            session_operations: adapter,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_turn_hooks(turn_hooks: Arc<dyn TurnHooks>) -> Self {
        let noop = Arc::new(NoopRuntimePorts);
        Self::from_immutable_ports(noop.clone(), turn_hooks, noop)
    }

    pub(crate) fn turn_extension_view(&self) -> TurnExtensionView {
        self.turn_view_provider.turn_extension_view()
    }

    pub(crate) fn runtime_snapshot_state(&self) -> RuntimeSnapshotState {
        self.turn_view_provider.runtime_snapshot_state()
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
