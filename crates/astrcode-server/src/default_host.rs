//! First-party host profile for the server binary.

use std::sync::{Arc, atomic::AtomicU64};

use astrcode_context::{
    context_assembler::LlmContextAssembler,
    post_compact_enricher::DefaultPostCompactEnricher,
    prompt_engine::{DefaultPromptFileProvider, DefaultPromptProvider},
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_session::SessionHostServices;

pub fn first_party_host_services(
    extension_runner: Arc<ExtensionRunner>,
    context_assembler: Arc<LlmContextAssembler>,
    shell_timeout_secs: Arc<AtomicU64>,
) -> SessionHostServices {
    SessionHostServices::embedded(
        context_assembler,
        Arc::new(DefaultPromptProvider),
        Arc::new(DefaultPromptFileProvider),
    )
    .with_extension_runner(extension_runner)
    .with_post_compact_enricher(Arc::new(DefaultPostCompactEnricher))
    .with_tool_packs(
        astrcode_tools::registry::default_tool_packs_with_shell_timeout_source(shell_timeout_secs),
    )
}
