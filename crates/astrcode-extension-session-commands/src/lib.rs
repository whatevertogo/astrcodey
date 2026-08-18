//! First-party session commands declared entirely through the Extension contract.
//!
//! These commands are pure declarations: their `execution` is
//! `CommandExecution::Host`, so the host parses arguments and executes them
//! behind its session operation gate. No extension handler exists for them.

use std::sync::Arc;

use astrcode_extension_sdk::{
    builder::{command, manifest},
    extension::{
        CommandAvailability, Extension, ExtensionCapability, ExtensionManifest, Registrar,
        SessionCommandKind,
    },
};

const FIRST_PARTY_SESSION_COMMAND_PRIORITY: i32 = 100;

pub fn extension() -> Arc<dyn Extension> {
    Arc::new(SessionCommandsExtension)
}

struct SessionCommandsExtension;

#[async_trait::async_trait]
impl Extension for SessionCommandsExtension {
    fn manifest(&self) -> ExtensionManifest {
        manifest("astrcode-session-commands")
            .version(env!("CARGO_PKG_VERSION"))
            .description(env!("CARGO_PKG_DESCRIPTION"))
            .capability(ExtensionCapability::SessionCommand)
            .build()
    }

    fn register(&self, registrar: &mut Registrar) {
        registrar.host_command(
            command("compact")
                .description("Compact the current session context")
                .requires_idle(true)
                .priority(FIRST_PARTY_SESSION_COMMAND_PRIORITY)
                .host_command(SessionCommandKind::CompactSession)
                .build(),
        );
        registrar.host_command(
            command("model")
                .description("Select the active AI model")
                .priority(FIRST_PARTY_SESSION_COMMAND_PRIORITY)
                .availability(CommandAvailability::InteractiveOnly)
                .host_command(SessionCommandKind::SelectModel)
                .build(),
        );
    }
}
