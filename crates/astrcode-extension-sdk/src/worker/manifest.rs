//! Worker 握手 manifest 构建（与 handler 注册单一数据源）。

use serde_json::{Value, json};

use crate::{
    extension::{ContinueAfterStopLimit, ExtensionHttpRoute},
    tool::ToolDefinition,
};

#[derive(Debug, Clone, Default)]
pub struct HookManifestOptions {
    pub max_per_turn: Option<ContinueAfterStopLimit>,
}

#[derive(Debug, Clone)]
pub struct HookManifestEntry {
    pub on: String,
    pub mode: String,
    pub options: HookManifestOptions,
}

#[derive(Debug, Clone)]
pub struct CommandManifestEntry {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct HttpRouteManifestEntry {
    pub route: ExtensionHttpRoute,
    pub handler_id: String,
}

#[derive(Debug, Default)]
pub struct ManifestCatalog {
    pub tools: Vec<ToolDefinition>,
    pub hooks: Vec<HookManifestEntry>,
    pub commands: Vec<CommandManifestEntry>,
    pub http_routes: Vec<HttpRouteManifestEntry>,
    pub capabilities: Vec<String>,
    pub extension_events: Vec<Value>,
}

impl ManifestCatalog {
    pub fn to_metadata_value(&self, extension_id: &str, version: &str) -> Value {
        let tools: Vec<Value> = self
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                    "mode": match t.execution_mode {
                        crate::tool::ExecutionMode::Parallel => "parallel",
                        crate::tool::ExecutionMode::Sequential => "sequential",
                    }
                })
            })
            .collect();
        let hooks: Vec<Value> = self
            .hooks
            .iter()
            .map(|h| {
                let mut hook = json!({ "on": h.on, "mode": h.mode });
                if let Some(max_per_turn) = h.options.max_per_turn {
                    hook["options"] = json!({ "max_per_turn": max_per_turn });
                }
                hook
            })
            .collect();
        let commands: Vec<Value> = self
            .commands
            .iter()
            .map(|c| json!({ "name": c.name, "description": c.description }))
            .collect();
        let http_routes: Vec<Value> = self
            .http_routes
            .iter()
            .map(|entry| {
                json!({
                    "route": entry.route,
                    "handler_id": entry.handler_id,
                })
            })
            .collect();
        json!({
            "extension_id": extension_id,
            "version": version,
            "protocol": { "s5r": crate::s5r::S5R_VERSION },
            "capabilities": self.capabilities,
            "tools": tools,
            "hooks": hooks,
            "commands": commands,
            "http_routes": http_routes,
            "extension_events": self.extension_events,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continue_after_stop_limit_serializes_under_hook_options() {
        let catalog = ManifestCatalog {
            hooks: vec![HookManifestEntry {
                on: "continue_after_stop".into(),
                mode: "blocking".into(),
                options: HookManifestOptions {
                    max_per_turn: Some(ContinueAfterStopLimit::unlimited()),
                },
            }],
            ..Default::default()
        };

        let metadata = catalog.to_metadata_value("test-extension", "0.0.0");

        assert_eq!(
            metadata["hooks"][0]["options"]["max_per_turn"],
            serde_json::json!(-1)
        );
    }

    #[test]
    fn generic_hook_omits_empty_options() {
        let catalog = ManifestCatalog {
            hooks: vec![HookManifestEntry {
                on: "turn_end".into(),
                mode: "advisory".into(),
                options: HookManifestOptions::default(),
            }],
            ..Default::default()
        };

        let metadata = catalog.to_metadata_value("test-extension", "0.0.0");

        assert!(metadata["hooks"][0].get("options").is_none());
    }
}
