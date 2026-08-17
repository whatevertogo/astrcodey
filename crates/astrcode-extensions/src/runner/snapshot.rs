use astrcode_extension_sdk::{extension::*, tool::ToolDefinition};

use super::{ExtensionRunner, supervisor::SupervisorState};

#[derive(Debug, Clone, Default)]
pub struct ExtensionRegistrySnapshot {
    pub extensions: Vec<ExtensionDeclarationSnapshot>,
}

#[derive(Debug, Clone)]
pub struct ExtensionDeclarationSnapshot {
    pub id: String,
    pub generation: u64,
    pub runtime_state: ExtensionRuntimeState,
    pub capabilities: Vec<ExtensionCapability>,
    pub required_transport_features: Vec<TransportFeature>,
    pub tools: Vec<ToolDefinition>,
    pub dynamic_tools: bool,
    pub commands: Vec<astrcode_extension_sdk::extension::SlashCommand>,
    pub dynamic_commands: bool,
    pub keybindings: Vec<astrcode_extension_sdk::extension::Keybinding>,
    pub status_items: Vec<astrcode_extension_sdk::extension::StatusItem>,
    pub custom_events: Vec<CustomEventDeclaration>,
    pub custom_event_subscriptions: Vec<CustomEventSubscription>,
    pub http_routes: Vec<ExtensionHttpRoute>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionRuntimeState {
    Initializing,
    Ready,
    Draining,
    Failed,
    Stopped,
}

impl From<&SupervisorState> for ExtensionRuntimeState {
    fn from(state: &SupervisorState) -> Self {
        match state {
            SupervisorState::Initializing => Self::Initializing,
            SupervisorState::Ready => Self::Ready,
            SupervisorState::Draining => Self::Draining,
            SupervisorState::Failed(_) => Self::Failed,
            SupervisorState::Stopped => Self::Stopped,
        }
    }
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
                    let runtime = hosted.supervisor.admission().snapshot();
                    ExtensionDeclarationSnapshot {
                        id: manifest.id().to_owned(),
                        generation: runtime.generation,
                        runtime_state: (&runtime.state).into(),
                        capabilities: manifest.capabilities().to_vec(),
                        required_transport_features: manifest
                            .required_transport_features()
                            .to_vec(),
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
                        custom_events: registrations.custom_event_declarations().to_vec(),
                        custom_event_subscriptions: registrations
                            .custom_event_subscriptions()
                            .iter()
                            .map(|registration| registration.subscription().clone())
                            .collect(),
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
