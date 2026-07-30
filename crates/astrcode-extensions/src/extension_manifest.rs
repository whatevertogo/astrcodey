//! s5r 扩展握手 manifest 类型与解析。

use astrcode_extension_sdk::{
    extension::{ExtensionCapability, ExtensionEventDecl},
    s5r::{
        capability_from_wire,
        manifest::{
            InitializeManifest, ManifestCommand, ManifestHook, ManifestHttpRoute, ManifestTool,
        },
    },
};
use serde_json::Value;

/// `Initialize.metadata` 解析出的注册信息。
#[derive(Debug, Clone)]
pub struct ExtensionRegistration {
    pub extension_id: String,
    pub capabilities: Vec<ExtensionCapability>,
    pub tools: Vec<ManifestTool>,
    pub commands: Vec<ManifestCommand>,
    pub hooks: Vec<ManifestHook>,
    pub http_routes: Vec<ManifestHttpRoute>,
    pub extension_events: Vec<ExtensionEventDecl>,
}

/// 从 s5r `InitializeMessage.metadata` 解析注册信息。
pub fn registration_from_s5r_metadata(
    metadata: &Value,
    expected_s5r_version: &str,
) -> Result<ExtensionRegistration, String> {
    let manifest: InitializeManifest = serde_json::from_value(metadata.clone())
        .map_err(|error| format!("invalid initialize manifest: {error}"))?;
    if manifest.protocol.s5r != expected_s5r_version {
        return Err(format!(
            "initialize metadata protocol.s5r must be \"{expected_s5r_version}\""
        ));
    }
    registration_from_manifest(manifest)
}

fn registration_from_manifest(
    manifest: InitializeManifest,
) -> Result<ExtensionRegistration, String> {
    let extension_id = manifest.extension_id.trim();
    if extension_id.is_empty() {
        return Err("initialize manifest missing extension_id".into());
    }
    let extension_id = extension_id.to_owned();

    let capabilities = manifest
        .capabilities
        .into_iter()
        .map(|name| {
            capability_from_wire(&name)
                .ok_or_else(|| format!("initialize manifest has unknown capability \"{name}\""))
        })
        .collect::<Result<_, _>>()?;

    let extension_events = manifest
        .extension_events
        .into_iter()
        .map(|event| ExtensionEventDecl {
            event_type: event.event_type,
            schema_version: event.schema_version,
            durable: event.durable,
            max_payload_bytes: event.max_payload_bytes,
        })
        .collect();

    Ok(ExtensionRegistration {
        extension_id,
        capabilities,
        tools: manifest.tools,
        commands: manifest.commands,
        hooks: manifest.hooks,
        http_routes: manifest.http_routes,
        extension_events,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn s5r_initialize_manifest_is_strict_and_preserves_legacy_defaults() {
        let invalid_manifests = [
            (
                json!({
                    "extension_id": "top-level-typo",
                    "protocol": {"s5r": astrcode_extension_sdk::s5r::S5R_VERSION},
                    "capabilites": []
                }),
                "unknown field `capabilites`",
            ),
            (
                json!({
                    "extension_id": "nested-typo",
                    "protocol": {"s5r": astrcode_extension_sdk::s5r::S5R_VERSION},
                    "tools": [{
                        "name": "tool",
                        "description": "",
                        "parameters": {"type": "object"},
                        "strcit": true
                    }]
                }),
                "unknown field `strcit`",
            ),
            (
                json!({
                    "extension_id": "bad-capability",
                    "protocol": {"s5r": astrcode_extension_sdk::s5r::S5R_VERSION},
                    "capabilities": ["not_a_capability"]
                }),
                "unknown capability",
            ),
        ];
        for (manifest, expected) in invalid_manifests {
            let error =
                registration_from_s5r_metadata(&manifest, astrcode_extension_sdk::s5r::S5R_VERSION)
                    .unwrap_err();
            assert!(error.contains(expected), "{error}");
        }

        let registration = registration_from_s5r_metadata(
            &json!({
                "extension_id": "legacy-defaults",
                "protocol": {"s5r": astrcode_extension_sdk::s5r::S5R_VERSION},
                "wire_codec": "json",
                "tools": [
                    {
                        "name": "legacy",
                        "description": "",
                        "parameters": {"type": "object"}
                    },
                    {
                        "name": "strict",
                        "description": "",
                        "parameters": {"type": "object"},
                        "strict": true
                    }
                ],
                "commands": [{"name": "legacy-command"}],
                "hooks": [{"on": "turn_end", "mode": "non_blocking"}],
                "extension_events": [{"event_type": "legacy.event"}]
            }),
            astrcode_extension_sdk::s5r::S5R_VERSION,
        )
        .expect("manifest should parse");

        assert!(!registration.tools[0].strict);
        assert_eq!(registration.tools[0].mode, "sequential");
        assert!(registration.tools[1].strict);
        assert!(registration.capabilities.is_empty());
        assert!(registration.http_routes.is_empty());
        assert_eq!(registration.commands[0].description, "");
        assert!(registration.hooks[0].options.max_per_turn.is_none());
        assert_eq!(registration.extension_events[0].schema_version, 1);
        assert!(registration.extension_events[0].durable);
        assert_eq!(
            registration.extension_events[0].max_payload_bytes,
            64 * 1024
        );
    }
}
