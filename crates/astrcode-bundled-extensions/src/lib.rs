//! First-party bundled extension source.
//!
//! This crate is the composition root for extensions shipped with AstrCode.
//! `astrcode-extensions` owns the extension runtime, while this crate decides
//! which first-party extensions are linked into a binary.

use astrcode_extensions::loader::{ExtensionLoadContext, ExtensionSource, LoadExtensionsResult};

/// Source for all enabled first-party bundled extensions.
pub struct BundledExtensionSource;

#[async_trait::async_trait]
impl ExtensionSource for BundledExtensionSource {
    async fn load(&self, _ctx: &ExtensionLoadContext) -> LoadExtensionsResult {
        LoadExtensionsResult {
            extensions: bundled_extensions(),
            errors: Vec::new(),
        }
    }
}

/// Return all enabled first-party bundled extensions in precedence order.
///
/// Earlier entries keep precedence when multiple extensions expose the
/// same tool name.
pub fn bundled_extensions() -> Vec<std::sync::Arc<dyn astrcode_core::extension::Extension>> {
    vec![
        #[cfg(feature = "agent-tools")]
        astrcode_extension_agent_tools::extension(),
        #[cfg(feature = "mcp")]
        astrcode_extension_mcp::extension(),
        #[cfg(feature = "skill")]
        astrcode_extension_skill::extension(),
        #[cfg(feature = "todo-tool")]
        astrcode_extension_todo_tool::extension(),
        #[cfg(feature = "mode")]
        astrcode_extension_mode::extension(),
    ]
}
