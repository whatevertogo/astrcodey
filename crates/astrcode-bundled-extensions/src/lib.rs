//! First-party bundled extension source.
//!
//! This crate is the composition root for extensions shipped with AstrCode.
//! `astrcode-extensions` owns the extension runtime, while this crate decides
//! which first-party extensions are linked into a binary.

use std::{collections::BTreeMap, sync::Arc};

use astrcode_extension_sdk::extension::{Extension, internal::extension_config};
use astrcode_extensions::{
    loader::{DiscoverExtensionsResult, ExtensionCandidate, ExtensionLoadContext, ExtensionSource},
    runner::ExtensionConfigValidationError,
};

type ExtensionFactory = fn() -> Arc<dyn Extension>;

struct BundledExtensionSpec {
    id: &'static str,
    default_enabled: bool,
    factory: ExtensionFactory,
}

const BUNDLED_EXTENSION_CATALOG: &[BundledExtensionSpec] = &[
    #[cfg(feature = "agent-tools")]
    BundledExtensionSpec {
        id: "astrcode-agent-tools",
        default_enabled: true,
        factory: astrcode_extension_agent_tools::extension,
    },
    #[cfg(feature = "coding")]
    BundledExtensionSpec {
        id: "astrcode-coding",
        default_enabled: true,
        factory: astrcode_extension_coding::extension,
    },
    #[cfg(feature = "mcp")]
    BundledExtensionSpec {
        id: "astrcode-mcp",
        default_enabled: true,
        factory: astrcode_extension_mcp::extension,
    },
    #[cfg(feature = "skill")]
    BundledExtensionSpec {
        id: "astrcode-skill",
        default_enabled: true,
        factory: astrcode_extension_skill::extension,
    },
    #[cfg(feature = "todo-tool")]
    BundledExtensionSpec {
        id: "astrcode-todo-tool",
        default_enabled: true,
        factory: astrcode_extension_todo_tool::extension,
    },
    #[cfg(feature = "mode")]
    BundledExtensionSpec {
        id: "astrcode-mode",
        default_enabled: true,
        factory: astrcode_extension_mode::extension,
    },
    #[cfg(feature = "ask-user")]
    BundledExtensionSpec {
        id: "astrcode-ask-user",
        default_enabled: true,
        factory: astrcode_extension_ask_user::extension,
    },
    #[cfg(feature = "goal")]
    BundledExtensionSpec {
        id: "astrcode-goal",
        default_enabled: true,
        factory: astrcode_extension_goal::extension,
    },
    #[cfg(feature = "memory")]
    BundledExtensionSpec {
        id: "astrcode.memory",
        default_enabled: false,
        factory: astrcode_extension_memory::extension,
    },
    #[cfg(feature = "channels")]
    BundledExtensionSpec {
        id: "astrcode-channels",
        default_enabled: false,
        factory: astrcode_extension_channels::extension,
    },
    #[cfg(feature = "web-tools")]
    BundledExtensionSpec {
        id: "astrcode-web-tools",
        default_enabled: true,
        factory: astrcode_extension_web_tools::extension,
    },
    #[cfg(feature = "session-commands")]
    BundledExtensionSpec {
        id: "astrcode-session-commands",
        default_enabled: true,
        factory: astrcode_extension_session_commands::extension,
    },
];

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
        let candidates = BUNDLED_EXTENSION_CATALOG
            .iter()
            .filter(|spec| extension_enabled(&self.extension_states, spec.id))
            .map(|spec| {
                let extension_id = spec.id;
                let factory = spec.factory;
                ExtensionCandidate::lazy(
                    format!("bundled:{extension_id}"),
                    format!("{}:{extension_id}", env!("CARGO_PKG_VERSION")),
                    extension_id,
                    move || async move { Ok(factory()) },
                )
            })
            .collect();
        DiscoverExtensionsResult {
            candidates,
            failures: Vec::new(),
        }
    }
}

/// Return all enabled first-party bundled extensions in precedence order.
///
/// Earlier entries keep precedence when multiple extensions expose the
/// same tool name.
pub fn bundled_extensions(extension_states: &BTreeMap<String, bool>) -> Vec<Arc<dyn Extension>> {
    BUNDLED_EXTENSION_CATALOG
        .iter()
        .filter(|spec| extension_enabled(extension_states, spec.id))
        .map(|spec| (spec.factory)())
        .collect()
}

/// Validate extension-owned config for every first-party extension linked into the binary.
///
/// Validation is independent of enablement so a disabled extension cannot persist config that
/// will fail only when it is enabled later.
pub fn validate_bundled_extension_configs(
    configs: &BTreeMap<String, serde_json::Value>,
) -> Result<(), ExtensionConfigValidationError> {
    for spec in BUNDLED_EXTENSION_CATALOG {
        let extension = (spec.factory)();
        let config = extension_config(
            spec.id,
            configs
                .get(spec.id)
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
        );
        extension
            .validate_config(&config)
            .map_err(|source| ExtensionConfigValidationError::new(spec.id, source))?;
    }
    Ok(())
}

pub fn bundled_extension_ids() -> Vec<&'static str> {
    BUNDLED_EXTENSION_CATALOG
        .iter()
        .map(|spec| spec.id)
        .collect()
}

/// 解析扩展是否启用（config 显式值优先，否则按扩展 id 的默认策略）。
///
/// 与 [`bundled_extensions`] 加载逻辑、HTTP `/api/extensions` 展示共用此函数，
/// 避免「实际未加载但 UI 显示已启用」的分歧。
pub fn extension_enabled(extension_states: &BTreeMap<String, bool>, extension_id: &str) -> bool {
    let default = BUNDLED_EXTENSION_CATALOG
        .iter()
        .find(|spec| spec.id == extension_id)
        .map(|spec| spec.default_enabled)
        .unwrap_or(true);
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
    fn extension_enablement_uses_catalog_defaults_and_explicit_overrides() {
        let states = BTreeMap::from([
            ("astrcode.memory".to_string(), true),
            ("astrcode-mode".to_string(), false),
        ]);
        assert!(extension_enabled(&states, "astrcode.memory"));
        assert!(!extension_enabled(&states, "astrcode-mode"));
        assert!(!extension_enabled(&states, "astrcode-channels"));
        assert!(extension_enabled(&states, "external.extension"));
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
            let manifest = extension.manifest();
            let extension_id = manifest.id().to_owned();
            if extension_id == "astrcode-mcp" {
                continue;
            }
            let mut registrar = Registrar::new();
            extension.register(&mut registrar);
            let (_, registrations) = registrar
                .finish(manifest)
                .expect("bundled extension registrations should match its manifest");
            non_strict.extend(
                registrations
                    .tools()
                    .iter()
                    .filter(|registration| !registration.definition().strict)
                    .map(|registration| {
                        format!("{extension_id}:{}", registration.definition().name)
                    }),
            );
        }

        assert!(
            non_strict.is_empty(),
            "bundled tools must opt into strict tool use: {non_strict:?}"
        );
    }

    #[test]
    fn config_owning_bundled_extensions_reject_invalid_candidates() {
        for spec in BUNDLED_EXTENSION_CATALOG {
            assert_eq!((spec.factory)().manifest().id(), spec.id);
        }
        validate_bundled_extension_configs(&BTreeMap::new())
            .expect("every bundled extension must accept an absent config");

        let invalid_configs: BTreeMap<&str, serde_json::Value> = BTreeMap::from([
            #[cfg(feature = "coding")]
            (
                "astrcode-coding",
                serde_json::json!({ "shellTimeoutSecs": 0 }),
            ),
            #[cfg(feature = "memory")]
            (
                "astrcode.memory",
                serde_json::json!({ "maxContexts": "many" }),
            ),
            #[cfg(feature = "channels")]
            (
                "astrcode-channels",
                serde_json::json!({ "unexpected": true }),
            ),
            #[cfg(feature = "web-tools")]
            (
                "astrcode-web-tools",
                serde_json::json!({ "unexpected": true }),
            ),
        ]);

        for (extension_id, config) in invalid_configs {
            let error = validate_bundled_extension_configs(&BTreeMap::from([(
                extension_id.to_owned(),
                config,
            )]))
            .expect_err("every bundled config owner must reject its invalid candidate");
            assert!(error.to_string().contains(extension_id), "{error}");
        }
    }
}
