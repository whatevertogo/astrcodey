//! First-party coding tools authored as a normal AstrCode Extension.

mod compact;
mod files;
mod process;

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[cfg(test)]
use astrcode_extension_sdk::extension::internal::extension_config;
use astrcode_extension_sdk::{
    builder::manifest,
    extension::{
        Extension, ExtensionCapability, ExtensionConfig, ExtensionError, ExtensionManifest,
        ExtensionStartContext, Registrar,
    },
};
use serde::Deserialize;

const EXTENSION_ID: &str = "astrcode-coding";
const DEFAULT_SHELL_TIMEOUT_SECS: u64 = 120;
const MAX_SHELL_TIMEOUT_SECS: u64 = 600;

pub fn extension() -> Arc<dyn Extension> {
    Arc::new(CodingExtension::default())
}

/// Validate a candidate configuration without constructing extension runtime state.
pub fn validate_config(config: &ExtensionConfig) -> Result<(), ExtensionError> {
    CodingExtension::parse_config(config).map(|_| ())
}

struct CodingExtension {
    default_shell_timeout_secs: Arc<AtomicU64>,
}

impl Default for CodingExtension {
    fn default() -> Self {
        Self {
            default_shell_timeout_secs: Arc::new(AtomicU64::new(DEFAULT_SHELL_TIMEOUT_SECS)),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct CodingConfig {
    shell_timeout_secs: u64,
}

impl Default for CodingConfig {
    fn default() -> Self {
        Self {
            shell_timeout_secs: DEFAULT_SHELL_TIMEOUT_SECS,
        }
    }
}

impl CodingExtension {
    fn parse_config(config: &ExtensionConfig) -> Result<CodingConfig, ExtensionError> {
        let config = config.deserialize_or_default::<CodingConfig>()?;
        if !(1..=MAX_SHELL_TIMEOUT_SECS).contains(&config.shell_timeout_secs) {
            return Err(ExtensionError::InvalidInput {
                code: astrcode_extension_sdk::WireErrorCode::InvalidInput
                    .as_str()
                    .into(),
                message: format!("shellTimeoutSecs must be between 1 and {MAX_SHELL_TIMEOUT_SECS}"),
                hint: Some(format!(
                    "set extensions.{EXTENSION_ID}.shellTimeoutSecs to a valid number of seconds"
                )),
            });
        }
        Ok(config)
    }

    fn apply_config(&self, config: &ExtensionConfig) -> Result<(), ExtensionError> {
        let config = Self::parse_config(config)?;
        self.default_shell_timeout_secs
            .store(config.shell_timeout_secs, Ordering::Release);
        Ok(())
    }
}

#[async_trait::async_trait]
impl Extension for CodingExtension {
    fn manifest(&self) -> ExtensionManifest {
        manifest(EXTENSION_ID)
            .version(env!("CARGO_PKG_VERSION"))
            .description(env!("CARGO_PKG_DESCRIPTION"))
            .capability(ExtensionCapability::WorkspaceRead)
            .capability(ExtensionCapability::WorkspaceWrite)
            .capability(ExtensionCapability::ToolResultRead)
            .capability(ExtensionCapability::ProcessSpawn)
            .capability(ExtensionCapability::SessionHistory)
            .build()
    }

    fn register(&self, registrar: &mut Registrar) {
        compact::register(registrar);
        files::register(registrar);
        process::register(registrar, Arc::clone(&self.default_shell_timeout_secs));
    }

    fn validate_config(&self, config: &ExtensionConfig) -> Result<(), ExtensionError> {
        validate_config(config)
    }

    async fn start(&self, context: ExtensionStartContext) -> Result<(), ExtensionError> {
        self.apply_config(context.config())
    }
}

#[cfg(test)]
mod tests {
    use astrcode_extension_sdk::{
        extension::{ExtensionCapability, Registrar},
        tool::ToolOrigin,
    };

    use super::*;

    #[test]
    fn registration_exposes_only_host_backed_coding_tools() {
        let extension = extension();
        let manifest = extension.manifest();
        let mut registrar = Registrar::new();
        extension.register(&mut registrar);
        let (manifest, registrations) = registrar.finish(manifest).expect("valid registration");

        assert_eq!(manifest.id(), EXTENSION_ID);
        assert_eq!(
            manifest.capabilities(),
            [
                ExtensionCapability::WorkspaceRead,
                ExtensionCapability::WorkspaceWrite,
                ExtensionCapability::ToolResultRead,
                ExtensionCapability::ProcessSpawn,
                ExtensionCapability::SessionHistory,
            ]
        );

        let tools = registrations.tools();
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.definition().name.as_str())
                .collect::<Vec<_>>(),
            [
                "read",
                "read_tool_result",
                "write",
                "edit",
                "patch",
                "glob",
                "grep",
                "shell"
            ]
        );
        for tool in tools {
            let definition = tool.definition();
            assert!(definition.strict, "{} must remain strict", definition.name);
            assert_eq!(definition.origin, ToolOrigin::Bundled);
            assert!(
                definition.parameters.get("maxOutputTokens").is_none()
                    && !definition
                        .parameters
                        .to_string()
                        .contains("maxOutputTokens"),
                "{} must use the platform result budget",
                definition.name
            );
        }
    }

    #[test]
    fn coding_config_validation_is_pure_and_application_updates_the_timeout() {
        let extension = CodingExtension::default();
        extension
            .apply_config(&extension_config(
                EXTENSION_ID,
                serde_json::json!({ "shellTimeoutSecs": 180 }),
            ))
            .expect("valid coding config");
        assert_eq!(
            extension.default_shell_timeout_secs.load(Ordering::Acquire),
            180
        );

        let error = extension
            .validate_config(&extension_config(
                EXTENSION_ID,
                serde_json::json!({ "shellTimeoutSecs": 0 }),
            ))
            .expect_err("zero timeout must fail");
        assert!(matches!(error, ExtensionError::InvalidInput { .. }));
        assert_eq!(
            extension.default_shell_timeout_secs.load(Ordering::Acquire),
            180,
            "validation must not mutate the live timeout"
        );
    }
}
