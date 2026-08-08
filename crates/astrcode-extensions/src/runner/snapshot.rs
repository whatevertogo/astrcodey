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
        let extensions = loop {
            let generation_pin = self.extension_view().await;
            let hosted_extensions = self.registry.extensions.read().await;
            let publication = self.registry.publication.lock();
            if !publication.is_stable_generation(generation_pin.generation()) {
                continue;
            }
            break hosted_extensions
                .iter()
                .map(|hosted| {
                    let manifest = &hosted.manifest;
                    let registrations = &manifest.registrations;
                    ExtensionDeclarationSnapshot {
                        id: manifest.id().to_owned(),
                        capabilities: manifest.capabilities().to_vec(),
                        tools: registrations
                            .tools()
                            .iter()
                            .map(|registration| registration.definition().clone())
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
        };
        ExtensionRegistrySnapshot { extensions }
    }
}
