//! First-party bundled extension source.
//!
//! This crate is the composition root for extensions shipped with AstrCode.
//! `astrcode-extensions` owns the extension runtime, while this crate decides
//! which first-party extensions are linked into a binary.

use std::collections::BTreeMap;

use astrcode_extensions::loader::{
    DiscoverExtensionsResult, ExtensionCandidate, ExtensionLoadContext, ExtensionSource,
};

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
    async fn discover(&self, _ctx: &ExtensionLoadContext) -> DiscoverExtensionsResult {
        let extensions = bundled_extensions(&self.extension_states);
        let candidates = extensions
            .into_iter()
            .map(|extension| {
                let extension_id = extension.id().to_string();
                ExtensionCandidate::ready(
                    format!("bundled:{extension_id}"),
                    format!("{}:{extension_id}", env!("CARGO_PKG_VERSION")),
                    extension,
                )
            })
            .collect();
        DiscoverExtensionsResult {
            candidates,
            errors: Vec::new(),
            failures: Vec::new(),
        }
    }

    fn owns_source_key(&self, source_key: &str) -> bool {
        source_key.starts_with("bundled:")
    }
}

/// Return all enabled first-party bundled extensions in precedence order.
///
/// Earlier entries keep precedence when multiple extensions expose the
/// same tool name.
pub fn bundled_extensions(
    extension_states: &BTreeMap<String, bool>,
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
    #[cfg(feature = "ask-user")]
    if is_enabled(extension_states, "astrcode-ask-user") {
        extensions.push(astrcode_extension_ask_user::extension());
    }
    #[cfg(feature = "goal")]
    if is_enabled(extension_states, "astrcode-goal") {
        extensions.push(astrcode_extension_goal::extension());
    }
    #[cfg(feature = "memory")]
    if is_enabled(extension_states, "astrcode.memory") {
        extensions.push(astrcode_extension_memory::extension());
    }
    #[cfg(feature = "channels")]
    if is_enabled(extension_states, "astrcode-channels") {
        extensions.push(astrcode_extension_channels::extension());
    }
    #[cfg(feature = "web-tools")]
    if is_enabled(extension_states, "astrcode-web-tools") {
        extensions.push(astrcode_extension_web_tools::extension());
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
        #[cfg(feature = "ask-user")]
        "astrcode-ask-user",
        #[cfg(feature = "goal")]
        "astrcode-goal",
        #[cfg(feature = "memory")]
        "astrcode.memory",
        #[cfg(feature = "channels")]
        "astrcode-channels",
        #[cfg(feature = "web-tools")]
        "astrcode-web-tools",
    ]
}

fn is_enabled(extension_states: &BTreeMap<String, bool>, extension_id: &str) -> bool {
    extension_enabled(extension_states, extension_id)
}

/// 解析扩展是否启用（config 显式值优先，否则按扩展 id 的默认策略）。
///
/// 与 [`bundled_extensions`] 加载逻辑、HTTP `/api/extensions` 展示共用此函数，
/// 避免「实际未加载但 UI 显示已启用」的分歧。
pub fn extension_enabled(extension_states: &BTreeMap<String, bool>, extension_id: &str) -> bool {
    // memory、channels 扩展默认关闭，其他扩展默认启用
    let default = !matches!(extension_id, "astrcode.memory" | "astrcode-channels");
    extension_states
        .get(extension_id)
        .copied()
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use astrcode_extension_sdk::extension::Registrar;

    use super::*;

    #[test]
    fn extension_enabled_uses_per_extension_defaults_when_unconfigured() {
        let states = BTreeMap::new();
        assert!(extension_enabled(&states, "astrcode-mode"));
        assert!(!extension_enabled(&states, "astrcode.memory"));
        assert!(!extension_enabled(&states, "astrcode-channels"));
    }

    #[test]
    fn extension_enabled_prefers_explicit_config() {
        let states = BTreeMap::from([
            ("astrcode.memory".to_string(), true),
            ("astrcode-mode".to_string(), false),
        ]);
        assert!(extension_enabled(&states, "astrcode.memory"));
        assert!(!extension_enabled(&states, "astrcode-mode"));
    }

    #[test]
    fn bundled_tools_request_provider_strict_tool_use_except_mcp() {
        let states = bundled_extension_ids()
            .into_iter()
            .map(|id| (id.to_string(), true))
            .collect();
        let extensions = bundled_extensions(&states);

        let mut non_strict = Vec::new();
        for extension in extensions {
            if extension.id() == "astrcode-mcp" {
                continue;
            }
            let mut registrar = Registrar::new();
            extension.register(&mut registrar);
            non_strict.extend(
                registrar
                    .tools()
                    .iter()
                    .filter(|(definition, _)| !definition.strict)
                    .map(|(definition, _)| format!("{}:{}", extension.id(), definition.name)),
            );
        }

        assert!(
            non_strict.is_empty(),
            "bundled tools must opt into strict tool use: {non_strict:?}"
        );
    }
}
