//! ToolRenderer trait, ToolRenderCtx, and ToolRendererRegistry.
//!
//! Mapped from pi-mono ToolRenderContext + ToolDefinition.renderCall/renderResult.

use std::{any::Any, collections::HashMap, sync::Arc};

use astrcode_core::{render::RenderSpec, tool::ToolResult};

/// Whether the tool renders its own outer frame or uses the default colored header box.
/// Mirrors pi-mono `renderShell: "default" | "self"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderShell {
    /// Standard colored header box wraps the tool output.
    Default,
    /// Tool renders its own framing (e.g. diff hunks).
    SelfRendered,
}

/// Per-call render context passed to ToolRenderer methods.
///
/// Mirrors pi-mono `ToolRenderContext<TState, TArgs>`.
pub struct ToolRenderCtx<'a> {
    pub call_id: &'a str,
    pub tool_name: &'a str,
    /// Tool call arguments. None until ToolCallRequested arrives.
    pub args: Option<&'a serde_json::Value>,
    /// True once ToolCallRequested has been received.
    pub args_complete: bool,
    /// True once ToolCallStarted has been received.
    pub execution_started: bool,
    /// True while the result is still streaming (partial).
    pub is_partial: bool,
    /// True if the result is an error.
    pub is_error: bool,
    /// Whether the tool row is in expanded view.
    pub expanded: bool,
    /// Per-call persistent state slot. Survives across render_call → render_result.
    /// Initialized to `Box<()>` by ToolRow; renderers can downcast and replace.
    pub state: &'a mut Box<dyn Any + Send>,
}

/// Renderer for a specific tool.
pub trait ToolRenderer: Send + Sync {
    fn tool_name(&self) -> &str;

    fn render_shell(&self) -> RenderShell {
        RenderShell::Default
    }

    /// Render the tool call (args summary) while the tool is running.
    fn render_call(&self, ctx: &mut ToolRenderCtx) -> RenderSpec;

    /// Render the tool result after completion.
    /// Return `None` to fall back to the default summary.
    fn render_result(&self, result: &ToolResult, ctx: &mut ToolRenderCtx) -> Option<RenderSpec>;
}

/// Registry mapping tool names to renderers.
///
/// Later registrations override earlier ones for the same name (pi-mono semantics).
pub struct ToolRendererRegistry {
    by_name: HashMap<String, Arc<dyn ToolRenderer>>,
    fallback: Arc<dyn ToolRenderer>,
}

impl ToolRendererRegistry {
    pub fn new(fallback: Arc<dyn ToolRenderer>) -> Self {
        Self {
            by_name: HashMap::new(),
            fallback,
        }
    }

    /// Register a renderer. Same name overrides the previous entry.
    pub fn register(&mut self, renderer: Arc<dyn ToolRenderer>) {
        self.by_name
            .insert(renderer.tool_name().to_string(), renderer);
    }

    /// Get the renderer for `tool_name`, or the fallback if not found.
    pub fn get_or_fallback(&self, tool_name: &str) -> Arc<dyn ToolRenderer> {
        self.by_name
            .get(tool_name)
            .cloned()
            .unwrap_or_else(|| Arc::clone(&self.fallback))
    }

    /// Get the renderer for `tool_name` if registered.
    pub fn get(&self, tool_name: &str) -> Option<Arc<dyn ToolRenderer>> {
        self.by_name.get(tool_name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::ext::fallback::DefaultToolRenderer;

    struct NamedRenderer(String);
    impl ToolRenderer for NamedRenderer {
        fn tool_name(&self) -> &str {
            &self.0
        }
        fn render_call(&self, _ctx: &mut ToolRenderCtx) -> RenderSpec {
            RenderSpec::Text {
                text: self.0.clone(),
                tone: Default::default(),
            }
        }
        fn render_result(
            &self,
            _result: &ToolResult,
            _ctx: &mut ToolRenderCtx,
        ) -> Option<RenderSpec> {
            None
        }
    }

    #[test]
    fn registry_overrides_by_name() {
        let fallback = Arc::new(DefaultToolRenderer);
        let mut reg = ToolRendererRegistry::new(fallback);
        reg.register(Arc::new(NamedRenderer("shell".into())));
        reg.register(Arc::new(NamedRenderer("shell".into())));
        // Should still have exactly one entry for "shell".
        assert!(reg.get("shell").is_some());
        assert!(reg.get("unknown").is_none());
    }

    #[test]
    fn fallback_when_unknown_tool() {
        let fallback = Arc::new(DefaultToolRenderer);
        let reg = ToolRendererRegistry::new(fallback);
        // get_or_fallback always returns something.
        let _ = reg.get_or_fallback("nonexistent_tool_xyz");
    }
}
