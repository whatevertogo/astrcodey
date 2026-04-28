//! Capability router: stable built-in tools + dynamic extension tools.
//!
//! Two-layer design prevents extension reloads from losing core tools.
//! Pattern adopted from astrcode reference.

use std::{collections::HashMap, sync::Arc};

use astrcode_core::tool::{Tool, ToolDefinition, ToolError, ToolResult};
use tokio::sync::RwLock;

/// Routes tool calls to the correct invoker.
///
/// Built-in tools live in the `stable` layer and never change after startup.
/// Extension-registered tools live in the `dynamic` layer and can be refreshed.
pub struct CapabilityRouter {
    stable: RwLock<HashMap<String, Arc<dyn Tool>>>,
    dynamic: RwLock<HashMap<String, Arc<dyn Tool>>>,
}

impl CapabilityRouter {
    pub fn new() -> Self {
        Self {
            stable: RwLock::new(HashMap::new()),
            dynamic: RwLock::new(HashMap::new()),
        }
    }

    /// Register a built-in tool (called once at startup, never removed).
    pub async fn register_stable(&self, tool: Arc<dyn Tool>) {
        let mut stable = self.stable.write().await;
        stable.insert(tool.definition().name.clone(), tool);
    }

    /// Replace all dynamic tools atomically (e.g., on extension reload).
    /// Built-in tools are NEVER affected.
    pub async fn apply_dynamic(&self, tools: Vec<Arc<dyn Tool>>) {
        let mut dynamic = self.dynamic.write().await;
        dynamic.clear();
        for t in tools {
            let name = t.definition().name.clone();
            // Extension order carries priority; keep the first tool for each name.
            dynamic.entry(name).or_insert(t);
        }
    }

    /// List all tool definitions (stable + dynamic, stable first).
    pub async fn list_definitions(&self) -> Vec<ToolDefinition> {
        let stable = self.stable.read().await;
        let dynamic = self.dynamic.read().await;
        let mut defs: Vec<_> = stable.values().map(|t| t.definition()).collect();
        defs.extend(dynamic.values().map(|t| t.definition()));
        defs
    }

    /// Execute a tool by name. Checks dynamic first (extensions override built-ins).
    pub async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &astrcode_core::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let dynamic_tool = { self.dynamic.read().await.get(name).cloned() };
        if let Some(tool) = dynamic_tool {
            return tool.execute(args, ctx).await;
        }

        let stable_tool = { self.stable.read().await.get(name).cloned() };
        if let Some(tool) = stable_tool {
            return tool.execute(args, ctx).await;
        }
        Err(ToolError::NotFound(name.into()))
    }
}

impl Default for CapabilityRouter {
    fn default() -> Self {
        Self::new()
    }
}
