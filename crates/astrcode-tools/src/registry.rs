//! Tool registry for managing built-in and extension-registered tools.

use std::{collections::HashMap, sync::Arc};

use astrcode_core::tool::{
    ExecutionMode, Tool, ToolDefinition, ToolError, ToolExecutionContext, ToolResult,
};

/// Registry of available tools (built-in + extension-registered).
///
/// 使用 HashMap 按工具名索引，O(1) 查找替代 Vec 的 O(n) 线性扫描。
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// 创建一个空的工具注册表。
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// 注册一个工具。如果同名工具已存在，会覆盖并输出警告日志。
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.definition().name.clone();
        if self.tools.contains_key(&name) {
            tracing::warn!("Tool '{}' already registered, overwriting", name);
        }
        self.tools.insert(name, tool);
    }

    /// 返回所有已注册工具的定义列表。
    pub fn list_definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = self
            .tools
            .values()
            .map(|tool| tool.definition())
            .collect::<Vec<_>>();
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        definitions
    }

    /// 按名称执行已注册的工具。
    ///
    /// - `name`：工具名称
    /// - `args`：传递给工具的 JSON 参数
    /// - `ctx`：工具执行上下文
    ///
    /// 如果工具未找到，返回 `ToolError::NotFound`。
    pub async fn execute(
        &self,
        name: &str,
        args: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        match self.tools.get(name) {
            Some(tool) => tool.execute(args, ctx).await,
            None => Err(ToolError::NotFound(name.into())),
        }
    }

    /// 返回指定工具的执行模式，未找到时保守地按顺序执行处理。
    pub fn execution_mode(&self, name: &str) -> ExecutionMode {
        self.tools
            .get(name)
            .map(|tool| tool.execution_mode())
            .unwrap_or(ExecutionMode::Sequential)
    }

    /// 按名称查找工具定义，未找到返回 `None`。
    pub fn find_definition(&self, name: &str) -> Option<ToolDefinition> {
        self.tools.get(name).map(|t| t.definition())
    }

    /// Drain all registered tools into a Vec (consumes the registry).
    pub fn into_tools(self) -> Vec<std::sync::Arc<dyn Tool>> {
        self.tools.into_values().collect()
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

    #[test]
    fn list_definitions_is_sorted_by_tool_name() {
        struct NamedTool(&'static str);

        #[async_trait::async_trait]
        impl Tool for NamedTool {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: self.0.to_string(),
                    description: String::new(),
                    parameters: serde_json::json!({"type": "object"}),
                    is_builtin: false,
                }
            }

            async fn execute(
                &self,
                _arguments: serde_json::Value,
                _ctx: &ToolExecutionContext,
            ) -> Result<ToolResult, ToolError> {
                unreachable!("registry ordering test does not execute tools")
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(NamedTool("zeta")));
        registry.register(Arc::new(NamedTool("alpha")));
        registry.register(Arc::new(NamedTool("middle")));

        let names = registry
            .list_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();

        assert_eq!(names, ["alpha", "middle", "zeta"]);
    }
}
