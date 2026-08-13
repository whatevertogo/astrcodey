use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Discovery metadata from an extension package's `extension.json`.
///
/// The host treats `extension_id` as authoritative before starting the process. The worker must
/// report the same id during initialization; runtime capabilities, tools, and hooks are declared
/// by that handshake rather than this file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPackageManifest {
    pub extension_id: String,
    pub protocol: ExtensionPackageProtocol,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExtensionPackageProtocol {
    pub s5r: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_keeps_discovery_identity_separate_from_runtime_declarations() {
        let manifest: ExtensionPackageManifest = serde_json::from_value(serde_json::json!({
            "extension_id": "review-extension",
            "protocol": { "s5r": "3.0" },
            "command": ["./review-extension", "serve"],
            "env": { "LOG_LEVEL": "info" }
        }))
        .expect("valid extension package manifest");
        assert_eq!(manifest.extension_id, "review-extension");
        assert_eq!(manifest.protocol.s5r, "3.0");
        assert_eq!(manifest.command, ["./review-extension", "serve"]);
        assert_eq!(manifest.env["LOG_LEVEL"], "info");

        for invalid in [
            serde_json::json!({
                "extension_id": "review-extension",
                "protocol": { "s5r": "3.0" },
                "command": ["./review-extension"],
                "capabilities": ["session_control"]
            }),
            serde_json::json!({
                "protocol": { "s5r": "3.0" },
                "command": ["./review-extension"]
            }),
            serde_json::json!({
                "extension_id": "review-extension",
                "protocol": {},
                "command": ["./review-extension"]
            }),
            serde_json::json!({
                "extension_id": "review-extension",
                "protocol": { "s5r": "3.0", "native": "1.0" },
                "command": ["./review-extension"]
            }),
        ] {
            assert!(serde_json::from_value::<ExtensionPackageManifest>(invalid).is_err());
        }
    }
}
