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
        let hosted_extensions = self.registry.extensions.read().await;
        let extensions = hosted_extensions
            .iter()
            .map(|hosted| {
                let manifest = &hosted.manifest;
                let registrations = &manifest.registrations;
                ExtensionDeclarationSnapshot {
                    id: manifest.id.clone(),
                    capabilities: manifest.capabilities.clone(),
                    tools: registrations
                        .tools()
                        .iter()
                        .map(|(definition, _)| definition.clone())
                        .collect(),
                    dynamic_tools: !registrations.tool_discoveries().is_empty(),
                    commands: registrations
                        .commands()
                        .iter()
                        .map(|(command, _)| command.clone())
                        .collect(),
                    dynamic_commands: !registrations.command_discoveries().is_empty(),
                    keybindings: registrations.keybindings().to_vec(),
                    status_items: registrations.status_items().to_vec(),
                    events: registrations.extension_event_decls().to_vec(),
                    http_routes: registrations
                        .http_routes()
                        .iter()
                        .map(|registration| registration.route.clone())
                        .collect(),
                }
            })
            .collect();
        ExtensionRegistrySnapshot { extensions }
    }
}
