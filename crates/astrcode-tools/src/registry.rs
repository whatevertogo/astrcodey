//! Tool registry for managing built-in and extension-registered tools.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use astrcode_core::tool::{
    BackgroundPolicy, ExecutionMode, Tool, ToolDefinition, ToolError, ToolExecutionContext,
    ToolPromptMetadata, ToolResult,
};

/// 单个已注册工具的全部元数据与实例。
struct RegisteredTool {
    tool: Arc<dyn Tool>,
    definition: ToolDefinition,
    prompt_metadata: Option<ToolPromptMetadata>,
}

/// Registry of available tools (built-in + extension-registered).
///
/// 用 `BTreeMap` 同时承载 O(log n) 命名查找、按名稱有序遍历，以及单一事实
/// 来源——避免之前 `HashMap` + sorted `Vec` 双结构在 `register` 时做 O(n)
/// sorted insert。`list_definitions()` 走迭代，仍按名稱有序输出。
pub struct ToolRegistry {
    tools: BTreeMap<String, RegisteredTool>,
}

impl ToolRegistry {
    /// 创建一个空的工具注册表。
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }

    /// 注册一个工具。如果同名工具已存在，会覆盖并输出警告日志。
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

    /// 返回所有已注册工具的定义列表（按工具名升序）。
    pub fn list_definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|entry| entry.definition.clone())
            .collect()
    }

    /// 返回所有已注册工具的定义及提示词元数据（按工具名升序）。
    pub fn list_definitions_with_prompt_metadata(
        &self,
    ) -> Vec<(ToolDefinition, Option<ToolPromptMetadata>)> {
        self.tools
            .values()
            .map(|entry| (entry.definition.clone(), entry.prompt_metadata.clone()))
            .collect()
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
            Some(entry) => entry.tool.execute(args, ctx).await,
            None => Err(ToolError::NotFound(name.into())),
        }
    }

    /// 返回指定工具的执行模式，未找到时保守地按顺序执行处理。
    pub fn execution_mode(&self, name: &str) -> ExecutionMode {
        self.tools
            .get(name)
            .map(|entry| entry.definition.execution_mode)
            .unwrap_or(ExecutionMode::Sequential)
    }

    /// 按名称查找工具定义，未找到返回 `None`。
    pub fn find_definition(&self, name: &str) -> Option<ToolDefinition> {
        self.tools.get(name).map(|entry| entry.definition.clone())
    }

    /// 按名称查询工具的后台化策略，未找到返回 `Never`。
    pub fn background_policy(&self, name: &str) -> BackgroundPolicy {
        self.tools
            .get(name)
            .map(|entry| entry.tool.background_policy())
            .unwrap_or(BackgroundPolicy::Never)
    }

    /// Drain all registered tools into a Vec (consumes the registry).
    pub fn into_tools(self) -> Vec<std::sync::Arc<dyn Tool>> {
        self.tools.into_values().map(|entry| entry.tool).collect()
    }

    /// 按名称移除一个已注册的工具。
    ///
    /// 用于子 agent 场景：从工具列表中排除不允许递归调用的工具
    /// （如 `agent`），使递归在架构层面不可能发生。
    pub fn unregister(&mut self, name: &str) {
        self.tools.remove(name);
    }
}

/// Build the core builtin tool set.
pub fn builtin_tools(working_dir: PathBuf, timeout_secs: u64) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(super::files::ReadFileTool {
            working_dir: working_dir.clone(),
        }) as Arc<dyn Tool>,
        Arc::new(super::files::WriteFileTool {
            working_dir: working_dir.clone(),
        }) as Arc<dyn Tool>,
        Arc::new(super::files::EditFileTool {
            working_dir: working_dir.clone(),
        }) as Arc<dyn Tool>,
        Arc::new(super::files::ApplyPatchTool {
            working_dir: working_dir.clone(),
        }) as Arc<dyn Tool>,
        Arc::new(super::files::FindFilesTool {
            working_dir: working_dir.clone(),
        }) as Arc<dyn Tool>,
        Arc::new(super::files::GrepTool {
            working_dir: working_dir.clone(),
        }) as Arc<dyn Tool>,
        Arc::new(super::shell_tool::ShellTool {
            working_dir,
            timeout_secs,
        }) as Arc<dyn Tool>,
        Arc::new(super::task_tool::TaskTool) as Arc<dyn Tool>,
    ]
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
    fn builtins_expose_patch_after_parser_is_wired() {
        let mut registry = ToolRegistry::new();
        for tool in builtin_tools(std::path::PathBuf::from("."), 30) {
            registry.register(tool);
        }

        let names = registry
            .list_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();

        assert!(names.iter().any(|name| name == "patch"));
        assert!(names.iter().any(|name| name == "edit"));
        assert!(names.iter().any(|name| name == "shell"));
    }

    #[test]
    fn builtins_carry_builtin_origin() {
        let mut registry = ToolRegistry::new();
        for tool in builtin_tools(std::path::PathBuf::from("."), 30) {
            registry.register(tool);
        }

        assert!(
            registry
                .list_definitions()
                .iter()
                .all(|definition| definition.origin == astrcode_core::tool::ToolOrigin::Builtin)
        );
    }

    #[test]
    fn readonly_builtins_are_marked_parallel() {
        let mut registry = ToolRegistry::new();
        for tool in builtin_tools(std::path::PathBuf::from("."), 30) {
            registry.register(tool);
        }

        for name in ["find", "grep", "read"] {
            let definition = registry.find_definition(name).unwrap();
            assert_eq!(definition.execution_mode, ExecutionMode::Parallel);
        }
        for name in ["edit", "patch", "shell", "write"] {
            let definition = registry.find_definition(name).unwrap();
            assert_eq!(definition.execution_mode, ExecutionMode::Sequential);
        }
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
                    origin: astrcode_core::tool::ToolOrigin::Extension,
                    execution_mode: ExecutionMode::Sequential,
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

    #[test]
    fn list_definitions_carries_tool_execution_mode() {
        struct ParallelNamedTool;

        #[async_trait::async_trait]
        impl Tool for ParallelNamedTool {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "parallel".into(),
                    description: String::new(),
                    parameters: serde_json::json!({"type": "object"}),
                    origin: astrcode_core::tool::ToolOrigin::Builtin,
                    execution_mode: ExecutionMode::Sequential,
                }
            }

            fn execution_mode(&self) -> ExecutionMode {
                ExecutionMode::Parallel
            }

            async fn execute(
                &self,
                _arguments: serde_json::Value,
                _ctx: &ToolExecutionContext,
            ) -> Result<ToolResult, ToolError> {
                unreachable!("registry metadata test does not execute tools")
            }
        }

        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(ParallelNamedTool));

        let definition = registry.find_definition("parallel").unwrap();
        assert_eq!(definition.execution_mode, ExecutionMode::Parallel);
    }
}
