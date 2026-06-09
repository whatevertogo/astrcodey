//! Composable kernel primitives for AstrCode hosts.
//!
//! The kernel crate owns runtime-level registries and host-facing composition
//! contracts. Concrete built-in tools, extension loaders, servers, CLIs, and
//! desktop shells register themselves from outside this crate.

pub mod extension_runtime;
pub mod tool_pack;
pub mod tool_registry;

use std::sync::Arc;

pub use extension_runtime::ExtensionRuntime;
pub use tool_pack::{ToolPack, ToolPackScope};
pub use tool_registry::ToolRegistry;

/// Composable kernel surface used by hosts to assemble runtime capabilities.
#[derive(Clone, Default)]
pub struct Kernel {
    tool_packs: Arc<[Arc<dyn ToolPack>]>,
}

impl Kernel {
    pub fn builder() -> KernelBuilder {
        KernelBuilder::default()
    }

    pub fn tool_packs(&self) -> &[Arc<dyn ToolPack>] {
        &self.tool_packs
    }

    pub fn build_tool_registry(&self, scope: &ToolPackScope<'_>) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for pack in self.tool_packs() {
            for tool in pack.tools(scope) {
                registry.register(tool);
            }
        }
        registry
    }
}

/// Builder for an embeddable AstrCode kernel.
#[derive(Default)]
pub struct KernelBuilder {
    tool_packs: Vec<Arc<dyn ToolPack>>,
}

impl KernelBuilder {
    pub fn with_tool_pack(mut self, pack: Arc<dyn ToolPack>) -> Self {
        self.tool_packs.push(pack);
        self
    }

    pub fn build(self) -> Kernel {
        Kernel {
            tool_packs: Arc::from(self.tool_packs),
        }
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::tool::{
        ExecutionMode, Tool, ToolDefinition, ToolError, ToolExecutionContext, ToolOrigin,
        ToolResult,
    };

    use super::*;

    struct StaticToolPack;
    struct StaticTool;

    impl ToolPack for StaticToolPack {
        fn tools(&self, _scope: &ToolPackScope<'_>) -> Vec<Arc<dyn Tool>> {
            vec![Arc::new(StaticTool)]
        }
    }

    #[async_trait::async_trait]
    impl Tool for StaticTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "static".into(),
                description: String::new(),
                parameters: serde_json::json!({"type": "object"}),
                origin: ToolOrigin::Extension,
                execution_mode: ExecutionMode::Sequential,
            }
        }

        async fn execute(
            &self,
            _arguments: serde_json::Value,
            _ctx: &ToolExecutionContext,
        ) -> Result<ToolResult, ToolError> {
            unreachable!("kernel composition test does not execute tools")
        }
    }

    #[test]
    fn builder_installs_tool_packs() {
        let kernel = Kernel::builder()
            .with_tool_pack(Arc::new(StaticToolPack))
            .build();
        let registry = kernel.build_tool_registry(&ToolPackScope {
            working_dir: ".",
            shell_timeout_secs: 1,
        });

        assert!(registry.find_definition("static").is_some());
    }
}
