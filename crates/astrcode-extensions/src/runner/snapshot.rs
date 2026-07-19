use astrcode_extension_sdk::{extension::*, tool::ToolDefinition};

use super::ExtensionRunner;

#[derive(Debug, Clone, Default)]
pub struct ExtensionRegistrySnapshot {
    pub extensions: Vec<ExtensionDeclarationSnapshot>,
}

#[derive(Debug, Clone)]
pub struct ExtensionDeclarationSnapshot {
    pub id: String,
    pub capabilities: Vec<ExtensionCapability>,
    pub tools: Vec<ToolDefinition>,
    pub dynamic_tools: bool,
    pub commands: Vec<astrcode_extension_sdk::extension::SlashCommand>,
    pub dynamic_commands: bool,
    pub keybindings: Vec<astrcode_extension_sdk::extension::Keybinding>,
    pub status_items: Vec<astrcode_extension_sdk::extension::StatusItem>,
    pub events: Vec<ExtensionEventDecl>,
    pub http_routes: Vec<ExtensionHttpRoute>,
}

impl ExtensionRunner {
    pub async fn registry_snapshot(&self) -> ExtensionRegistrySnapshot {
        let records = self.records.read().await;
        let extensions = records
            .iter()
            .map(|record| ExtensionDeclarationSnapshot {
                id: record.id.clone(),
                capabilities: record.capabilities.clone(),
                tools: record
                    .reg
                    .tools()
                    .iter()
                    .map(|(definition, _)| definition.clone())
                    .collect(),
                dynamic_tools: !record.reg.tool_discoveries().is_empty(),
                commands: record
                    .reg
                    .commands()
                    .iter()
                    .map(|(command, _)| command.clone())
                    .collect(),
                dynamic_commands: !record.reg.command_discoveries().is_empty(),
                keybindings: record.reg.keybindings().to_vec(),
                status_items: record.reg.status_items().to_vec(),
                events: record.reg.extension_event_decls().to_vec(),
                http_routes: record
                    .reg
                    .http_routes()
                    .iter()
                    .map(|registration| registration.route.clone())
                    .collect(),
            })
            .collect();
        ExtensionRegistrySnapshot { extensions }
    }
}
