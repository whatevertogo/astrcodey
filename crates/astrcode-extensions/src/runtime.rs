//! Shared extension runtime — lazy binding pattern borrowed from pi-mono.
//!
//! Extensions are loaded before the server fully boots. Their registrations
//! (tools, commands) are queued into this runtime. Once the server is ready,
//! `bind_core()` flushes the queues and replaces stub methods with live ones.

use astrcode_core::tool::ToolDefinition;

/// Shared state for all loaded extensions.
///
/// Created by the loader, then passed to `bind_core()` after the server
/// capability router is available.
pub struct ExtensionRuntime {
    /// Tools registered by extensions during loading.
    pending_tools: std::sync::Mutex<Vec<ToolDefinition>>,
}

impl Default for ExtensionRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionRuntime {
    pub fn new() -> Self {
        Self {
            pending_tools: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Queue a tool registration. Called from NativeExtension during factory().
    pub fn register_tool(&self, def: ToolDefinition) {
        self.pending_tools.lock().unwrap().push(def);
    }

    /// Take all pending tool registrations (consumes them).
    pub fn take_pending_tools(&self) -> Vec<ToolDefinition> {
        std::mem::take(&mut *self.pending_tools.lock().unwrap())
    }
}
