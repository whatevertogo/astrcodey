//! MessageRenderer trait and MessageRendererRegistry.

use std::{collections::HashMap, sync::Arc};

use astrcode_core::render::RenderSpec;

/// Options passed to a message renderer.
#[derive(Default)]
pub struct MessageRenderOpts;

/// Renderer for a custom message type.
///
/// Mirrors pi-mono `MessageRenderer<T>(message, {expanded}, theme) -> Component | undefined`.
pub trait MessageRenderer: Send + Sync {
    /// Render the payload. Return `None` to fall back to markdown.
    fn render(&self, payload: &serde_json::Value, opts: &MessageRenderOpts) -> Option<RenderSpec>;
}

/// Registry mapping custom_type strings to renderers.
pub struct MessageRendererRegistry {
    by_type: HashMap<String, Arc<dyn MessageRenderer>>,
}

impl MessageRendererRegistry {
    pub fn new() -> Self {
        Self {
            by_type: HashMap::new(),
        }
    }

    /// Register a renderer. Same `custom_type` overrides the previous entry.
    #[cfg(test)]
    pub fn register(&mut self, custom_type: impl Into<String>, renderer: Arc<dyn MessageRenderer>) {
        self.by_type.insert(custom_type.into(), renderer);
    }

    pub fn get(&self, custom_type: &str) -> Option<Arc<dyn MessageRenderer>> {
        self.by_type.get(custom_type).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoRenderer;
    impl MessageRenderer for EchoRenderer {
        fn render(
            &self,
            _payload: &serde_json::Value,
            _opts: &MessageRenderOpts,
        ) -> Option<RenderSpec> {
            Some(RenderSpec::Text {
                text: "echo".into(),
                tone: Default::default(),
            })
        }
    }

    #[test]
    fn custom_type_dispatch() {
        let mut reg = MessageRendererRegistry::new();
        reg.register("my_type", Arc::new(EchoRenderer));
        assert!(reg.get("my_type").is_some());
        assert!(reg.get("other_type").is_none());
    }

    #[test]
    fn later_registration_overrides_earlier() {
        let mut reg = MessageRendererRegistry::new();
        reg.register("t", Arc::new(EchoRenderer));
        reg.register("t", Arc::new(EchoRenderer));
        assert!(reg.get("t").is_some());
    }
}
