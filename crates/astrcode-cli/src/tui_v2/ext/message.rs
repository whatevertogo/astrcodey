//! MessageRenderer trait and MessageRendererRegistry.
//!
//! Mapped from pi-mono `MessageRenderer<T>` + `registerMessageRenderer`.

use std::{collections::HashMap, sync::Arc};

use astrcode_core::render::RenderSpec;

/// Options passed to a message renderer.
pub struct MessageRenderOpts {
    pub expanded: bool,
}

/// Renderer for a custom message type.
///
/// Mirrors pi-mono `MessageRenderer<T>(message, {expanded}, theme) -> Component | undefined`.
pub trait MessageRenderer: Send + Sync {
    fn custom_type(&self) -> &str;

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

    /// Register a renderer. Same custom_type overrides the previous entry.
    pub fn register(&mut self, renderer: Arc<dyn MessageRenderer>) {
        self.by_type
            .insert(renderer.custom_type().to_string(), renderer);
    }

    pub fn get(&self, custom_type: &str) -> Option<Arc<dyn MessageRenderer>> {
        self.by_type.get(custom_type).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoRenderer(String);
    impl MessageRenderer for EchoRenderer {
        fn custom_type(&self) -> &str {
            &self.0
        }
        fn render(
            &self,
            _payload: &serde_json::Value,
            _opts: &MessageRenderOpts,
        ) -> Option<RenderSpec> {
            Some(RenderSpec::Text {
                text: self.0.clone(),
                tone: Default::default(),
            })
        }
    }

    #[test]
    fn custom_type_dispatch() {
        let mut reg = MessageRendererRegistry::new();
        reg.register(Arc::new(EchoRenderer("my_type".into())));
        assert!(reg.get("my_type").is_some());
        assert!(reg.get("other_type").is_none());
    }

    #[test]
    fn later_registration_overrides_earlier() {
        let mut reg = MessageRendererRegistry::new();
        reg.register(Arc::new(EchoRenderer("t".into())));
        reg.register(Arc::new(EchoRenderer("t".into())));
        assert!(reg.get("t").is_some());
    }
}
