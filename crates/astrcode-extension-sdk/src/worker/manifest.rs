//! Worker 握手 manifest 构建（与 handler 注册单一数据源）。

use serde_json::Value;

use crate::{
    extension::{CustomEventDeclaration, CustomEventSubscription},
    s5r::manifest::{
        InitializeManifest, InitializeManifestProtocol, ManifestCommand, ManifestHook,
        ManifestHttpRoute, ManifestTool,
    },
    tool::ToolDefinition,
};

#[derive(Debug, Default)]
pub(crate) struct ManifestCatalog {
    pub tools: Vec<ToolDefinition>,
    pub hooks: Vec<ManifestHook>,
    pub continuation_hooks: Vec<String>,
    pub commands: Vec<ManifestCommand>,
    pub http_routes: Vec<ManifestHttpRoute>,
    pub capabilities: Vec<String>,
    pub custom_events: Vec<CustomEventDeclaration>,
    pub custom_event_subscriptions: Vec<CustomEventSubscription>,
}

impl ManifestCatalog {
    pub(crate) fn to_metadata_value(
        &self,
        extension_id: &str,
        version: &str,
    ) -> Result<Value, String> {
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
        serde_json::to_value(InitializeManifest {
            extension_id: extension_id.into(),
            version: version.into(),
            protocol: InitializeManifestProtocol {
                s5r: crate::s5r::S5R_VERSION.into(),
            },
            wire_codec: None,
            wire_features: vec![crate::s5r::WIRE_FEATURE_PARENT_INVOKE_ID.into()],
            capabilities: self.capabilities.clone(),
            tools,
            hooks: self.hooks.clone(),
            continuation_hooks: self.continuation_hooks.clone(),
            commands: self.commands.clone(),
            http_routes: self.http_routes.clone(),
            custom_events: self.custom_events.clone(),
            custom_event_subscriptions: self.custom_event_subscriptions.clone(),
        })
        .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{builder::worker_tool as tool, extension::ContinueAfterStopLimit};

    #[test]
    fn tool_strict_declaration_is_included_in_manifest() {
        let catalog = ManifestCatalog {
            tools: vec![tool("strictTool").strict().build().into()],
            ..Default::default()
        };

        let metadata = catalog
            .to_metadata_value("test-extension", "0.0.0")
            .unwrap();

        assert_eq!(metadata["tools"][0]["strict"], true);
        assert_eq!(
            metadata["wire_features"],
            serde_json::json!([crate::s5r::WIRE_FEATURE_PARENT_INVOKE_ID])
        );
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
    fn custom_event_contracts_are_serialized_into_the_manifest() {
        let mut catalog = ManifestCatalog::default();
        catalog.custom_events.push(CustomEventDeclaration {
            event_type: "typed.event".into(),
            schema_version: 2,
            durable: false,
            max_payload_bytes: 4096,
        });
        catalog.custom_event_subscriptions.push(
            CustomEventSubscription::from_extension("producer", "typed.event")
                .named("consume-typed-event"),
        );
        let metadata = catalog
            .to_metadata_value("test-extension", "0.0.0")
            .unwrap();
        assert_eq!(metadata["custom_events"][0]["eventType"], "typed.event");
        assert_eq!(metadata["custom_events"][0]["schemaVersion"], 2);
        assert_eq!(metadata["custom_events"][0]["durable"], false);
        assert_eq!(metadata["custom_events"][0]["maxPayloadBytes"], 4096);
        assert_eq!(
            metadata["custom_event_subscriptions"][0],
            serde_json::json!({
                "id": "consume-typed-event",
                "eventType": "typed.event",
                "source": {
                    "kind": "extension",
                    "extensionId": "producer"
                }
            })
        );
    }
}
