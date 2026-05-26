//! s5r 扩展握手 manifest 类型与解析。

use astrcode_core::extension::{ExtensionCapability, ExtensionEventDecl};
use astrcode_extension_sdk::s5r::capability_from_wire;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `Initialize.metadata` 解析出的注册信息。
#[derive(Debug, Clone)]
pub struct ExtensionRegistration {
    pub extension_id: String,
    pub version: String,
    pub capabilities: Vec<ExtensionCapability>,
    pub tools: Vec<ManifestTool>,
    pub commands: Vec<ManifestCommand>,
    pub hooks: Vec<ManifestHook>,
    pub extension_events: Vec<ExtensionEventDecl>,
}

pub mod manifest_types {
    use super::*;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ManifestTool {
        pub name: String,
        pub description: String,
        pub parameters: Value,
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
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ManifestExtensionEvent {
        pub event_type: String,
        #[serde(default = "default_schema_version")]
        pub schema_version: u32,
        #[serde(default = "default_durable")]
        pub durable: bool,
        #[serde(default = "default_max_payload")]
        pub max_payload_bytes: usize,
    }

    fn default_schema_version() -> u32 {
        1
    }
    fn default_durable() -> bool {
        true
    }
    fn default_max_payload() -> usize {
        64 * 1024
    }
}

use manifest_types::{ManifestCommand, ManifestExtensionEvent, ManifestHook, ManifestTool};

/// 从 s5r `InitializeMessage.metadata` 解析注册信息。
pub fn registration_from_s5r_metadata(
    metadata: &Value,
    expected_s5r_version: &str,
) -> Result<ExtensionRegistration, String> {
    let proto = metadata
        .get("protocol")
        .and_then(|p| p.get("s5r"))
        .and_then(|v| v.as_str());
    if proto != Some(expected_s5r_version) {
        return Err(format!(
            "initialize metadata protocol.s5r must be \"{expected_s5r_version}\""
        ));
    }
    registration_from_manifest_value(metadata)
}

fn registration_from_manifest_value(value: &Value) -> Result<ExtensionRegistration, String> {
    let extension_id = value
        .get("extension_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("initialize manifest missing extension_id")?
        .to_string();

    let version = value
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0")
        .to_string();

    let capabilities: Vec<ExtensionCapability> = value
        .get("capabilities")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c.as_str().and_then(capability_from_wire))
                .collect()
        })
        .unwrap_or_default();

    let tools: Vec<ManifestTool> = value
        .get("tools")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let commands: Vec<ManifestCommand> = value
        .get("commands")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let hooks: Vec<ManifestHook> = value
        .get("hooks")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let extension_events: Vec<ExtensionEventDecl> = value
        .get("extension_events")
        .and_then(|v| serde_json::from_value::<Vec<ManifestExtensionEvent>>(v.clone()).ok())
        .map(|evs| {
            evs.into_iter()
                .map(|e| ExtensionEventDecl {
                    event_type: e.event_type,
                    schema_version: e.schema_version,
                    durable: e.durable,
                    max_payload_bytes: e.max_payload_bytes,
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(ExtensionRegistration {
        extension_id,
        version,
        capabilities,
        tools,
        commands,
        hooks,
        extension_events,
    })
}
