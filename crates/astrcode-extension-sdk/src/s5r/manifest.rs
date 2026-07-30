//! `Initialize.metadata` wire contract shared by s5r workers and the host.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::extension::{ContinueAfterStopLimit, ExtensionHttpRoute};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeManifest {
    pub extension_id: String,
    #[serde(default)]
    pub version: String,
    pub protocol: InitializeManifestProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_codec: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub tools: Vec<ManifestTool>,
    #[serde(default)]
    pub hooks: Vec<ManifestHook>,
    #[serde(default)]
    pub commands: Vec<ManifestCommand>,
    #[serde(default)]
    pub http_routes: Vec<ManifestHttpRoute>,
    #[serde(default)]
    pub extension_events: Vec<ManifestExtensionEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeManifestProtocol {
    pub s5r: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ManifestCommand {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestHook {
    pub on: String,
    pub mode: String,
    #[serde(default, skip_serializing_if = "ManifestHookOptions::is_empty")]
    pub options: ManifestHookOptions,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ManifestHttpRoute {
    pub route: ExtensionHttpRoute,
    pub handler_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestExtensionEvent {
    pub event_type: String,
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default = "default_durable")]
    pub durable: bool,
    #[serde(default = "default_max_payload")]
    pub max_payload_bytes: usize,
}

const fn default_schema_version() -> u32 {
    1
}

const fn default_durable() -> bool {
    true
}

const fn default_max_payload() -> usize {
    64 * 1024
}
