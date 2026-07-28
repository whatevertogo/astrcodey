//! First-party host profile for the server binary.

use std::sync::{Arc, atomic::AtomicU64};

use astrcode_context::{
    context_assembler::LlmContextAssembler,
    post_compact_enricher::DefaultPostCompactEnricher,
    prompt_engine::{DefaultPromptFileProvider, DefaultPromptProvider},
};
use astrcode_extension_sdk::runtime_ports::{CompositeToolCatalogProvider, ToolCatalogProvider};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_session::SessionHostServices;

pub fn first_party_host_services(
    extension_runner: Arc<ExtensionRunner>,
    context_assembler: Arc<LlmContextAssembler>,
    shell_timeout_secs: Arc<AtomicU64>,
) -> SessionHostServices {
    let extension_catalog: Arc<dyn ToolCatalogProvider> = extension_runner.clone();
    let builtin_catalog = astrcode_tools::registry::default_tool_catalog_with_shell_timeout_source(
        shell_timeout_secs,
    );
    let tool_catalog: Arc<dyn ToolCatalogProvider> =
        Arc::new(CompositeToolCatalogProvider::new(vec![
            ("extensions".into(), extension_catalog),
            ("builtins".into(), builtin_catalog),
        ]));

    SessionHostServices::embedded(
        context_assembler,
        Arc::new(DefaultPromptProvider),
        Arc::new(DefaultPromptFileProvider),
    )
    .with_extension_adapter(extension_runner)
    .with_post_compact_enricher(Arc::new(DefaultPostCompactEnricher))
    .with_tool_catalog(tool_catalog)
}
