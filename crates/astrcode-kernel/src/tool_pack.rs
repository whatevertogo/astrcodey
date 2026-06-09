use std::sync::Arc;

use astrcode_core::tool::Tool;

/// Runtime scope used by a host-provided tool pack to bind tools to a session.
#[derive(Debug, Clone)]
pub struct ToolPackScope<'a> {
    pub working_dir: &'a str,
}

/// A host-provided source of tools.
///
/// Kernel/session code depends on this trait instead of concrete built-in tool
/// crates. Hosts choose which packs to install for each embedding.
pub trait ToolPack: Send + Sync {
    fn tools(&self, scope: &ToolPackScope<'_>) -> Vec<Arc<dyn Tool>>;
}
