use std::sync::Arc;

use astrcode_context::context_assembler::LlmContextAssembler;
use astrcode_core::{
    llm::LlmProvider,
    tool::{AgentSessionControl, FileObservationStore},
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_tools::registry::ToolRegistry;
use tokio::sync::mpsc;

use crate::{
    background::{BackgroundTaskCompletion, BackgroundTaskManager},
    session::Session,
};

/// 会话运行时服务容器。
///
/// 聚合 TurnRunner 所需的全部外部依赖。各字段按 session 生命周期存在，
/// 由上层（ServerRuntime / SessionSpawner）组装后传入 `TurnRunner::new`。
#[derive(Clone)]
pub struct SessionServices {
    pub llm: Arc<dyn LlmProvider>,
    pub tool_registry: Arc<ToolRegistry>,
    pub extension_runner: Arc<ExtensionRunner>,
    pub context_assembler: Arc<LlmContextAssembler>,
    pub session: Arc<Session>,
    pub background_result_tx: Option<mpsc::UnboundedSender<BackgroundTaskCompletion>>,
    pub background_tasks: Arc<parking_lot::Mutex<BackgroundTaskManager>>,
    pub file_observation_store: Arc<dyn FileObservationStore>,
    pub agent_session_control: Option<Arc<dyn AgentSessionControl>>,
}

impl SessionServices {
    pub fn new(
        llm: Arc<dyn LlmProvider>,
        tool_registry: Arc<ToolRegistry>,
        extension_runner: Arc<ExtensionRunner>,
        context_assembler: Arc<LlmContextAssembler>,
        session: Arc<Session>,
        background_tasks: Arc<parking_lot::Mutex<BackgroundTaskManager>>,
        file_observation_store: Arc<dyn FileObservationStore>,
    ) -> Self {
        Self {
            llm,
            tool_registry,
            extension_runner,
            context_assembler,
            session,
            background_result_tx: None,
            background_tasks,
            file_observation_store,
            agent_session_control: None,
        }
    }

    pub fn with_background_result_tx(
        mut self,
        tx: mpsc::UnboundedSender<BackgroundTaskCompletion>,
    ) -> Self {
        self.background_result_tx = Some(tx);
        self
    }

    pub fn with_agent_session_control(
        mut self,
        control: Option<Arc<dyn AgentSessionControl>>,
    ) -> Self {
        self.agent_session_control = control;
        self
    }
}
