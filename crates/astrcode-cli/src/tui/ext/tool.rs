//! ToolRenderer trait, ToolRenderCtx, and ToolRendererRegistry.

use std::{collections::HashMap, sync::Arc};

use astrcode_core::{render::RenderSpec, tool::ToolResult};

/// Per-call render context passed to [`ToolRenderer::render_result`].
pub struct ToolRenderCtx<'a> {
    pub tool_name: &'a str,
}

/// Renderer for a specific tool.
pub trait ToolRenderer: Send + Sync {
    fn tool_name(&self) -> &str;

    /// Render the tool result after completion.
    /// Return `None` to fall back to the default summary.
    fn render_result(&self, result: &ToolResult, ctx: &ToolRenderCtx<'_>) -> Option<RenderSpec>;
}

/// Registry mapping tool names to renderers.
///
/// Later registrations override earlier ones for the same name.
pub struct ToolRendererRegistry {
    by_name: HashMap<String, Arc<dyn ToolRenderer>>,
}

impl ToolRendererRegistry {
    pub fn new() -> Self {
        Self {
            by_name: HashMap::new(),
        }
    }

    /// Register a renderer. Same name overrides the previous entry.
    pub fn register(&mut self, renderer: Arc<dyn ToolRenderer>) {
        self.by_name
            .insert(renderer.tool_name().to_string(), renderer);
    }

    /// Get the renderer for `tool_name` if registered.
    pub fn get(&self, tool_name: &str) -> Option<Arc<dyn ToolRenderer>> {
        self.by_name.get(tool_name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NamedRenderer(String);
    impl ToolRenderer for NamedRenderer {
        fn tool_name(&self) -> &str {
            &self.0
        }
        fn render_result(
            &self,
            _result: &ToolResult,
            _ctx: &ToolRenderCtx<'_>,
        ) -> Option<RenderSpec> {
            None
        }
    }

    #[test]
    fn registry_overrides_by_name() {
        let mut reg = ToolRendererRegistry::new();
        reg.register(Arc::new(NamedRenderer("shell".into())));
        reg.register(Arc::new(NamedRenderer("shell".into())));
        assert!(reg.get("shell").is_some());
        assert!(reg.get("unknown").is_none());
    }
}
