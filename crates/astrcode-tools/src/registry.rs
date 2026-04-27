//! Tool registry for managing built-in and extension-registered tools.

use std::sync::Arc;

use astrcode_core::tool::{Tool, ToolDefinition, ToolError, ToolResult};

/// Registry of available tools (built-in + extension-registered).
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn list_definitions(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(|t| t.definition()).collect()
    }

    pub async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        for tool in &self.tools {
            if tool.definition().name == name {
                return tool.execute(args).await;
            }
        }
        Err(ToolError::NotFound(name.into()))
    }

    pub fn find_definition(&self, name: &str) -> Option<ToolDefinition> {
        self.tools
            .iter()
            .map(|tool| tool.definition())
            .find(|definition| definition.name == name)
    }

    /// Drain all registered tools into a Vec (consumes the registry).
    pub fn into_tools(self) -> Vec<std::sync::Arc<dyn Tool>> {
        self.tools
    }
}

impl ToolRegistry {
    /// Register all built-in tools with sensible defaults.
    pub fn register_builtins(&mut self, working_dir: std::path::PathBuf, timeout_secs: u64) {
        use std::sync::Arc;

        // File tools
        self.register(Arc::new(super::files::ReadFileTool {
            working_dir: working_dir.clone(),
        }));
        self.register(Arc::new(super::files::WriteFileTool {
            working_dir: working_dir.clone(),
        }));
        self.register(Arc::new(super::files::EditFileTool {
            working_dir: working_dir.clone(),
        }));
        self.register(Arc::new(super::files::ApplyPatchTool {
            working_dir: working_dir.clone(),
        }));
        self.register(Arc::new(super::files::FindFilesTool {
            working_dir: working_dir.clone(),
        }));
        self.register(Arc::new(super::files::GrepTool {
            working_dir: working_dir.clone(),
        }));

        // Shell tool
        self.register(Arc::new(super::shell_tool::ShellTool {
            working_dir: working_dir.clone(),
            timeout_secs,
        }));

        // TODO: Agent tools (spawn/send/observe/close) — re-enable when wired
        // TODO: Plan/mode tools (taskWrite/enterPlanMode/exitPlanMode/upsertSessionPlan) —
        // re-enable when wired
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_expose_apply_patch_after_parser_is_wired() {
        let mut registry = ToolRegistry::new();
        registry.register_builtins(std::path::PathBuf::from("."), 30);

        let names = registry
            .list_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();

        assert!(names.iter().any(|name| name == "apply_patch"));
        assert!(names.iter().any(|name| name == "editFile"));
        assert!(names.iter().any(|name| name == "shell"));
    }
}
