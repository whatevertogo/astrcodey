//! Tool registry for kernel-managed tool dispatch.

use std::{collections::BTreeMap, path::Path, sync::Arc};

use astrcode_core::{
    extension::ChildToolPolicy,
    tool::{
        ExecutionMode, Tool, ToolDefinition, ToolError, ToolExecutionContext, ToolPromptMetadata,
        ToolResult,
    },
    tool_access::ResourceAccess,
};

/// Registered tool plus the metadata cached from its implementation.
#[derive(Clone)]
struct RegisteredTool {
    tool: Arc<dyn Tool>,
    definition: ToolDefinition,
    prompt_metadata: Option<ToolPromptMetadata>,
}

/// Registry of tools available to a session runtime.
#[derive(Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, RegisteredTool>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let mut definition = tool.definition();
        definition.execution_mode = tool.execution_mode();
        let name = definition.name.clone();
        let prompt_metadata = tool.prompt_metadata();
        if self.tools.contains_key(&name) {
            tracing::warn!("Tool '{}' already registered, overwriting", name);
        }
        self.tools.insert(
            name,
            RegisteredTool {
                tool,
                definition,
                prompt_metadata,
            },
        );
    }

    pub fn list_definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|entry| entry.definition.clone())
            .collect()
    }

    pub fn list_definitions_with_prompt_metadata(
        &self,
    ) -> Vec<(ToolDefinition, Option<ToolPromptMetadata>)> {
        self.tools
            .values()
            .map(|entry| (entry.definition.clone(), entry.prompt_metadata.clone()))
            .collect()
    }

    pub async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        match self.tools.get(name) {
            Some(entry) => entry.tool.execute(args, ctx).await,
            None => Err(ToolError::NotFound(name.into())),
        }
    }

    pub fn execution_mode(&self, name: &str) -> ExecutionMode {
        self.tools
            .get(name)
            .map(|entry| entry.definition.execution_mode)
            .unwrap_or(ExecutionMode::Sequential)
    }

    pub fn resource_accesses(
        &self,
        name: &str,
        args: &serde_json::Value,
        working_dir: &Path,
    ) -> Result<Vec<ResourceAccess>, ToolError> {
        match self.tools.get(name) {
            Some(entry) => entry.tool.resource_accesses(args, working_dir),
            None => Err(ToolError::NotFound(name.into())),
        }
    }

    pub fn find_definition(&self, name: &str) -> Option<ToolDefinition> {
        self.tools.get(name).map(|entry| entry.definition.clone())
    }

    pub fn into_tools(self) -> Vec<Arc<dyn Tool>> {
        self.tools.into_values().map(|entry| entry.tool).collect()
    }

    pub fn unregister(&mut self, name: &str) {
        self.tools.remove(name);
    }

    pub fn clone_with_child_policy(&self, policy: Option<&ChildToolPolicy>) -> Self {
        let mut cloned = self.clone();
        if let Some(policy) = policy {
            cloned.apply_child_tool_policy(policy);
        }
        cloned
    }

    fn apply_child_tool_policy(&mut self, policy: &ChildToolPolicy) {
        match policy {
            ChildToolPolicy::Deny { tools } => {
                for name in tools {
                    if self.find_definition(name).is_none() {
                        tracing::debug!(tool = %name, "deny policy mentions unknown tool, skipping");
                        continue;
                    }
                    self.unregister(name);
                }
            },
            ChildToolPolicy::Allow { tools } => {
                let allow: std::collections::HashSet<&str> =
                    tools.iter().map(String::as_str).collect();
                let to_remove: Vec<String> = self
                    .tools
                    .keys()
                    .filter(|name| !allow.contains(name.as_str()))
                    .cloned()
                    .collect();
                for name in to_remove {
                    self.unregister(&name);
                }
            },
        }
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

    struct NamedTool(&'static str, ExecutionMode);

    #[async_trait::async_trait]
    impl Tool for NamedTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.0.to_string(),
                description: String::new(),
                parameters: serde_json::json!({"type": "object"}),
                origin: astrcode_core::tool::ToolOrigin::Extension,
                execution_mode: ExecutionMode::Sequential,
            }
        }

        fn execution_mode(&self) -> ExecutionMode {
            self.1
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
            _ctx: &ToolExecutionContext,
        ) -> Result<ToolResult, ToolError> {
            unreachable!("registry tests do not execute tools")
        }
    }

    #[test]
    fn list_definitions_is_sorted_by_tool_name() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(NamedTool("zeta", ExecutionMode::Sequential)));
        registry.register(Arc::new(NamedTool("alpha", ExecutionMode::Sequential)));
        registry.register(Arc::new(NamedTool("middle", ExecutionMode::Sequential)));

        let names = registry
            .list_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();

        assert_eq!(names, ["alpha", "middle", "zeta"]);
    }

    #[test]
    fn list_definitions_carries_tool_execution_mode() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(NamedTool("parallel", ExecutionMode::Parallel)));

        let definition = registry.find_definition("parallel").unwrap();
        assert_eq!(definition.execution_mode, ExecutionMode::Parallel);
    }

    #[test]
    fn clone_with_child_policy_filters_without_rebuilding_tools() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(NamedTool("read", ExecutionMode::Parallel)));
        registry.register(Arc::new(NamedTool("shell", ExecutionMode::Sequential)));

        let filtered = registry.clone_with_child_policy(Some(&ChildToolPolicy::Deny {
            tools: vec!["shell".into()],
        }));

        assert!(registry.find_definition("shell").is_some());
        assert!(filtered.find_definition("shell").is_none());
        assert!(filtered.find_definition("read").is_some());
    }
}
