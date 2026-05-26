//! Worker 握手 manifest 构建（与 handler 注册单一数据源）。

use serde_json::{Value, json};

use crate::tool::ToolDefinition;

#[derive(Debug, Clone)]
pub struct HookManifestEntry {
    pub on: String,
    pub mode: String,
}

#[derive(Debug, Clone)]
pub struct CommandManifestEntry {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Default)]
pub struct ManifestCatalog {
    pub tools: Vec<ToolDefinition>,
    pub hooks: Vec<HookManifestEntry>,
    pub commands: Vec<CommandManifestEntry>,
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
            .map(|h| json!({ "on": h.on, "mode": h.mode }))
            .collect();
        let commands: Vec<Value> = self
            .commands
            .iter()
            .map(|c| json!({ "name": c.name, "description": c.description }))
            .collect();
        json!({
            "extension_id": extension_id,
            "version": version,
            "protocol": { "s5r": crate::s5r::S5R_VERSION },
            "capabilities": self.capabilities,
            "tools": tools,
            "hooks": hooks,
            "commands": commands,
            "extension_events": self.extension_events,
        })
    }

    pub fn validate_handlers_registered(
        &self,
        tools: &[String],
        hooks: &[String],
        commands: &[String],
    ) -> Result<(), String> {
        for name in tools {
            if !self.tools.iter().any(|t| &t.name == name) {
                return Err(format!(
                    "tool handler {name} registered without manifest entry"
                ));
            }
        }
        for on in hooks {
            if !self.hooks.iter().any(|h| &h.on == on) {
                return Err(format!(
                    "hook handler {on} registered without manifest entry"
                ));
            }
        }
        for name in commands {
            if !self.commands.iter().any(|c| &c.name == name) {
                return Err(format!(
                    "command handler {name} registered without manifest entry"
                ));
            }
        }
        Ok(())
    }
}
