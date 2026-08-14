mod shell;

use std::sync::{Arc, atomic::AtomicU64};

use astrcode_extension_sdk::{
    builder::ExtensionToolDefinition,
    extension::Registrar,
    tool::{ToolPromptMetadata, ToolPromptTag},
};

pub(super) fn register(registrar: &mut Registrar, default_shell_timeout_secs: Arc<AtomicU64>) {
    let system_prompt = || ToolPromptMetadata::new(String::new()).prompt_tag(ToolPromptTag::System);
    registrar.tool(
        ExtensionToolDefinition::from_definition(shell::definition()).with_prompt(system_prompt()),
        Arc::new(shell::ShellHandler::new(default_shell_timeout_secs)),
    );
}
