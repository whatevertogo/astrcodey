//! `Initialize.metadata` wire contract shared by s5r workers and the host.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::extension::{
    ContinueAfterStopLimit, CustomEventDeclaration, CustomEventSubscription, ExtensionHttpRoute,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeManifest {
    pub extension_id: String,
    #[serde(default)]
    pub version: String,
    pub protocol: InitializeManifestProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_codec: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wire_features: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub tools: Vec<ManifestTool>,
    #[serde(default)]
    pub hooks: Vec<ManifestHook>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub continuation_hooks: Vec<String>,
    #[serde(default)]
    pub commands: Vec<ManifestCommand>,
    #[serde(default)]
    pub http_routes: Vec<ManifestHttpRoute>,
    #[serde(default)]
    pub custom_events: Vec<CustomEventDeclaration>,
    #[serde(default)]
    pub custom_event_subscriptions: Vec<CustomEventSubscription>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeManifestProtocol {
    pub s5r: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(default)]
    pub strict: bool,
    #[serde(default = "sequential_mode")]
    pub mode: String,
}

fn sequential_mode() -> String {
    "sequential".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestCommand {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestHook {
    pub on: String,
    pub mode: String,
    #[serde(default, skip_serializing_if = "ManifestHookOptions::is_empty")]
    pub options: ManifestHookOptions,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestHookOptions {
    #[serde(default)]
    pub max_per_turn: Option<ContinueAfterStopLimit>,
}

impl ManifestHookOptions {
    fn is_empty(&self) -> bool {
        self.max_per_turn.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestHttpRoute {
    pub route: ExtensionHttpRoute,
    pub handler_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_event_declaration_rejects_unknown_wire_fields() {
        assert!(
            serde_json::from_value::<CustomEventDeclaration>(serde_json::json!({
                "eventType": "test.completed",
                "unexpected": true
            }))
            .is_err()
        );
    }
}
