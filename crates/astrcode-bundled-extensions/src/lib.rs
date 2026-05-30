//! First-party bundled extension source.
//!
//! This crate is the composition root for extensions shipped with AstrCode.
//! `astrcode-extensions` owns the extension runtime, while this crate decides
//! which first-party extensions are linked into a binary.

use std::collections::BTreeMap;

use astrcode_extensions::loader::{ExtensionLoadContext, ExtensionSource, LoadExtensionsResult};

/// Source for all enabled first-party bundled extensions.
pub struct BundledExtensionSource {
    extension_states: BTreeMap<String, bool>,
}

impl BundledExtensionSource {
    pub fn new(extension_states: BTreeMap<String, bool>) -> Self {
        Self { extension_states }
    }
}

impl Default for BundledExtensionSource {
    fn default() -> Self {
        Self::new(BTreeMap::new())
    }
}

#[async_trait::async_trait]
impl ExtensionSource for BundledExtensionSource {
    async fn load(&self, _ctx: &ExtensionLoadContext) -> LoadExtensionsResult {
        let mut errors = Vec::new();
        let extensions = bundled_extensions(&self.extension_states, &mut errors);
        LoadExtensionsResult { extensions, errors }
    }
}

/// Return all enabled first-party bundled extensions in precedence order.
///
/// Earlier entries keep precedence when multiple extensions expose the
/// same tool name.
pub fn bundled_extensions(
    extension_states: &BTreeMap<String, bool>,
    _errors: &mut Vec<String>,
) -> Vec<std::sync::Arc<dyn astrcode_extension_sdk::extension::Extension>> {
    let mut extensions = Vec::new();

    #[cfg(feature = "agent-tools")]
    if is_enabled(extension_states, "astrcode-agent-tools") {
        extensions.push(astrcode_extension_agent_tools::extension());
    }
    #[cfg(feature = "mcp")]
    if is_enabled(extension_states, "astrcode-mcp") {
        extensions.push(astrcode_extension_mcp::extension());
    }
    #[cfg(feature = "skill")]
    if is_enabled(extension_states, "astrcode-skill") {
        extensions.push(astrcode_extension_skill::extension());
    }
    #[cfg(feature = "todo-tool")]
    if is_enabled(extension_states, "astrcode-todo-tool") {
        extensions.push(astrcode_extension_todo_tool::extension());
    }
    #[cfg(feature = "mode")]
    if is_enabled(extension_states, "astrcode-mode") {
        extensions.push(astrcode_extension_mode::extension());
    }
    #[cfg(feature = "memory")]
    if is_enabled(extension_states, "astrcode.memory") {
        extensions.push(astrcode_extension_memory::extension());
    }
    #[cfg(feature = "channels")]
    if is_enabled(extension_states, "astrcode-channels") {
        extensions.push(astrcode_extension_channels::extension());
    }

    extensions
}

pub fn bundled_extension_ids() -> Vec<&'static str> {
    vec![
        #[cfg(feature = "agent-tools")]
        "astrcode-agent-tools",
        #[cfg(feature = "mcp")]
        "astrcode-mcp",
        #[cfg(feature = "skill")]
        "astrcode-skill",
        #[cfg(feature = "todo-tool")]
        "astrcode-todo-tool",
        #[cfg(feature = "mode")]
        "astrcode-mode",
        #[cfg(feature = "memory")]
        "astrcode.memory",
        #[cfg(feature = "channels")]
        "astrcode-channels",
    ]
}

fn is_enabled(extension_states: &BTreeMap<String, bool>, extension_id: &str) -> bool {
    // memory 扩展默认关闭，其他扩展默认启用
    let default = extension_id != "astrcode.memory";
    extension_states
        .get(extension_id)
        .copied()
        .unwrap_or(default)
}
