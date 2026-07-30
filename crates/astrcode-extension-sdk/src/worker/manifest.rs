//! Worker 握手 manifest 构建（与 handler 注册单一数据源）。

use serde_json::Value;

use crate::{
    extension::ExtensionEventDecl,
    s5r::manifest::{
        InitializeManifest, InitializeManifestProtocol, ManifestCommand, ManifestExtensionEvent,
        ManifestHook, ManifestHttpRoute, ManifestTool,
    },
    tool::ToolDefinition,
};

#[derive(Debug, Default)]
pub(crate) struct ManifestCatalog {
    pub tools: Vec<ToolDefinition>,
    pub hooks: Vec<ManifestHook>,
    pub commands: Vec<ManifestCommand>,
    pub http_routes: Vec<ManifestHttpRoute>,
    pub capabilities: Vec<String>,
    pub extension_events: Vec<ExtensionEventDecl>,
    invalid_extension_event: Option<String>,
}

impl ManifestCatalog {
    pub(crate) fn push_legacy_extension_event(&mut self, event: Value) {
        match serde_json::from_value::<ManifestExtensionEvent>(event) {
            Ok(event) => self.extension_events.push(event.into()),
            Err(error) if self.invalid_extension_event.is_none() => {
                self.invalid_extension_event = Some(error.to_string());
            },
            Err(_) => {},
        }
    }

    pub(crate) fn to_metadata_value(
        &self,
        extension_id: &str,
        version: &str,
    ) -> Result<Value, String> {
        if let Some(error) = &self.invalid_extension_event {
            return Err(format!(
                "invalid legacy extension event declaration: {error}"
            ));
        }
        let tools = self
            .tools
            .iter()
            .map(|tool| ManifestTool {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
                strict: tool.strict,
                mode: match tool.execution_mode {
                    crate::tool::ExecutionMode::Parallel => "parallel".into(),
                    crate::tool::ExecutionMode::Sequential => "sequential".into(),
                },
            })
            .collect();
        let extension_events = self
            .extension_events
            .iter()
            .map(ManifestExtensionEvent::from)
            .collect();
        serde_json::to_value(InitializeManifest {
            extension_id: extension_id.into(),
            version: version.into(),
            protocol: InitializeManifestProtocol {
                s5r: crate::s5r::S5R_VERSION.into(),
            },
            wire_codec: None,
            capabilities: self.capabilities.clone(),
            tools,
            hooks: self.hooks.clone(),
            commands: self.commands.clone(),
            http_routes: self.http_routes.clone(),
            extension_events,
        })
        .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{builder::tool, extension::ContinueAfterStopLimit};

    #[test]
    fn tool_strict_declaration_is_included_in_manifest() {
        let catalog = ManifestCatalog {
            tools: vec![tool("strictTool").strict().build()],
            ..Default::default()
        };

        let metadata = catalog
            .to_metadata_value("test-extension", "0.0.0")
            .unwrap();

        assert_eq!(metadata["tools"][0]["strict"], true);
    }

    #[test]
    fn continue_after_stop_limit_serializes_under_hook_options() {
        let catalog = ManifestCatalog {
            hooks: vec![ManifestHook {
                on: "continue_after_stop".into(),
                mode: "blocking".into(),
                options: crate::s5r::manifest::ManifestHookOptions {
                    max_per_turn: Some(ContinueAfterStopLimit::unlimited()),
                },
            }],
            ..Default::default()
        };

        let metadata = catalog
            .to_metadata_value("test-extension", "0.0.0")
            .unwrap();

        assert_eq!(
            metadata["hooks"][0]["options"]["max_per_turn"],
            serde_json::json!(-1)
        );
    }

    #[test]
    fn generic_hook_omits_empty_options() {
        let catalog = ManifestCatalog {
            hooks: vec![ManifestHook {
                on: "turn_end".into(),
                mode: "advisory".into(),
                options: crate::s5r::manifest::ManifestHookOptions::default(),
            }],
            ..Default::default()
        };

        let metadata = catalog
            .to_metadata_value("test-extension", "0.0.0")
            .unwrap();

        assert!(metadata["hooks"][0].get("options").is_none());
    }

    #[test]
    fn legacy_extension_event_values_are_validated_into_the_typed_manifest() {
        let mut catalog = ManifestCatalog::default();
        catalog.push_legacy_extension_event(serde_json::json!({
            "event_type": "legacy.event"
        }));
        let metadata = catalog
            .to_metadata_value("test-extension", "0.0.0")
            .unwrap();
        assert_eq!(
            metadata["extension_events"][0]["event_type"],
            "legacy.event"
        );
        assert_eq!(metadata["extension_events"][0]["schema_version"], 1);

        catalog.push_legacy_extension_event(serde_json::json!({
            "event_tipe": "typo"
        }));
        assert!(
            catalog
                .to_metadata_value("test-extension", "0.0.0")
                .unwrap_err()
                .contains("event_type")
        );
    }
}
