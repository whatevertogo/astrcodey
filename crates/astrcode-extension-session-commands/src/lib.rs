//! First-party session commands declared entirely through the Extension contract.

use std::sync::Arc;

use astrcode_extension_sdk::{
    builder::{command, manifest},
    extension::{
        CommandAvailability, CommandContext, CommandHandler, Extension, ExtensionCapability,
        ExtensionCommandResult, ExtensionError, ExtensionManifest, Registrar, SessionCommandIntent,
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
        registrar.command(
            command("compact")
                .description("Compact the current session context")
                .requires_idle(true)
                .priority(FIRST_PARTY_SESSION_COMMAND_PRIORITY)
                .host_command(SessionCommandKind::CompactSession)
                .build(),
            Arc::new(CompactCommand),
        );
        registrar.command(
            command("model")
                .description("Select the active AI model")
                .priority(FIRST_PARTY_SESSION_COMMAND_PRIORITY)
                .availability(CommandAvailability::InteractiveOnly)
                .host_command(SessionCommandKind::SelectModel)
                .build(),
            Arc::new(SelectModelCommand),
        );
    }
}

struct CompactCommand;

#[async_trait::async_trait]
impl CommandHandler for CompactCommand {
    async fn execute(
        &self,
        context: CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        let argument = context.argument().trim();
        let keep_recent_turns = if argument.is_empty() {
            None
        } else {
            Some(argument.parse().map_err(|_| {
                invalid_command_input("compact expects an optional non-negative integer")
            })?)
        };
        Ok(ExtensionCommandResult::host_command(
            SessionCommandIntent::CompactSession { keep_recent_turns },
        ))
    }
}

struct SelectModelCommand;

#[async_trait::async_trait]
impl CommandHandler for SelectModelCommand {
    async fn execute(
        &self,
        context: CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        if !context.argument().trim().is_empty() {
            return Err(invalid_command_input("model does not accept arguments"));
        }
        Ok(ExtensionCommandResult::host_command(
            SessionCommandIntent::SelectModel,
        ))
    }
}

fn invalid_command_input(message: impl Into<String>) -> ExtensionError {
    ExtensionError::invalid_input(message, None)
}
