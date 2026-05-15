use std::sync::Arc;

use astrcode_context::context_engine::LlmContextAssembler;
use astrcode_core::llm::LlmProvider;
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_tools::registry::ToolRegistry;
use tokio::sync::mpsc;

use crate::{
    compact::AutoCompactFailureTracker, session::Session, tool_types::BackgroundTaskCompletion,
};

#[derive(Clone)]
pub struct SessionContext {
    pub llm: Arc<dyn LlmProvider>,
    pub tool_registry: Arc<ToolRegistry>,
    pub extension_runner: Arc<ExtensionRunner>,
    pub context_assembler: Arc<LlmContextAssembler>,
    pub session: Arc<Session>,
    pub auto_compact_failures: Arc<AutoCompactFailureTracker>,
    pub background_result_tx: Option<mpsc::UnboundedSender<BackgroundTaskCompletion>>,
    pub background_tasks: Arc<parking_lot::Mutex<crate::background::BackgroundTaskManager>>,
    pub agent_session_control: Option<Arc<dyn astrcode_core::tool::AgentSessionControl>>,
}
