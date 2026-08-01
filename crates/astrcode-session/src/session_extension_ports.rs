//! Capability-segregated extension boundary consumed by Session.

use std::sync::Arc;

use astrcode_extension_sdk::runtime_ports::{
    NoopRuntimePorts, PromptContributor, RuntimeSnapshotProvider, RuntimeSnapshotState,
    SessionOperationsProvider, TurnHooks,
};

/// Groups narrow extension ports without making Session depend on a concrete
/// extension runner.
pub struct SessionExtensionPorts {
    runtime_snapshot: Arc<dyn RuntimeSnapshotProvider>,
    prompt_contributor: Arc<dyn PromptContributor>,
    turn_hooks: Arc<dyn TurnHooks>,
    session_operations: Arc<dyn SessionOperationsProvider>,
}

impl SessionExtensionPorts {
    /// Combines ports whose observable state never changes after construction.
    ///
    /// 注意：该构造器不提供 runtime snapshot——传入的端口都是构造后不可变的，
    /// 没有可观测的运行时快照状态，因此 `runtime_snapshot` 固定为 `NoopRuntimePorts`
    /// （调用 [`Self::runtime_snapshot_state`] 会得到 noop 的默认状态）。
    pub fn from_immutable_ports(
        prompt_contributor: Arc<dyn PromptContributor>,
        turn_hooks: Arc<dyn TurnHooks>,
        session_operations: Arc<dyn SessionOperationsProvider>,
    ) -> Self {
        Self {
            runtime_snapshot: Arc::new(NoopRuntimePorts),
            prompt_contributor,
            turn_hooks,
            session_operations,
        }
    }

    pub fn from_adapter<T>(adapter: Arc<T>) -> Self
    where
        T: PromptContributor
            + RuntimeSnapshotProvider
            + TurnHooks
            + SessionOperationsProvider
            + 'static,
    {
        Self {
            runtime_snapshot: adapter.clone(),
            prompt_contributor: adapter.clone(),
            turn_hooks: adapter.clone(),
            session_operations: adapter,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_turn_hooks(turn_hooks: Arc<dyn TurnHooks>) -> Self {
        let noop = Arc::new(NoopRuntimePorts);
        Self::from_immutable_ports(noop.clone(), turn_hooks, noop)
    }

    pub(crate) fn runtime_snapshot_state(&self) -> RuntimeSnapshotState {
        self.runtime_snapshot.runtime_snapshot_state()
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
