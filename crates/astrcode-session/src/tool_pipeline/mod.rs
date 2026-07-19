//! Tool execution pipeline — preparation, execution, commit, and persistence.

mod commit;
mod events;
mod execute;
mod prepare;

use std::sync::Arc;

use astrcode_core::{storage::ToolResultArtifactReader, tool::ToolDefinition};
use astrcode_kernel::{ExtensionRuntime, ToolRegistry};
use tokio_util::sync::CancellationToken;

use crate::{
    early_tool_scheduler::EarlyToolScheduler,
    session::Session,
    tool_exec::{ToolCallRuntimeContext, TurnToolContext},
    turn_context::SharedTurnContext,
};

pub struct ToolCalls {
    turn: TurnToolContext,
    tool_registry: Arc<ToolRegistry>,
    extension_runner: Arc<dyn ExtensionRuntime>,
    session: Session,
    cancellation_token: CancellationToken,
}

impl ToolCalls {
    pub fn new(
        turn: TurnToolContext,
        tool_registry: Arc<ToolRegistry>,
        extension_runner: Arc<dyn ExtensionRuntime>,
        session: Session,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            turn,
            tool_registry,
            extension_runner,
            session,
            cancellation_token,
        }
    }

    pub fn list_definitions_with_prompt_metadata(
        &self,
    ) -> Vec<(
        ToolDefinition,
        Option<astrcode_core::tool::ToolPromptMetadata>,
    )> {
        self.tool_registry.list_definitions_with_prompt_metadata()
    }

    pub(crate) fn shared(&self) -> &SharedTurnContext {
        &self.turn.shared
    }

    pub(crate) fn shared_mut(&mut self) -> &mut SharedTurnContext {
        &mut self.turn.shared
    }

    /// 构建工具调用的运行时上下文。
    pub(crate) fn make_runtime_context(
        &self,
        tools: Arc<[ToolDefinition]>,
    ) -> ToolCallRuntimeContext {
        ToolCallRuntimeContext {
            turn: self.turn.clone(),
            tools,
            tool_result_reader: Some(
                Arc::new(self.session.clone()) as Arc<dyn ToolResultArtifactReader>
            ),
            cancellation_token: self.cancellation_token.clone(),
        }
    }

    /// 创建流式工具执行调度器。
    pub(crate) fn create_early_scheduler(
        &self,
        tools: Vec<ToolDefinition>,
        max_parallel: usize,
    ) -> EarlyToolScheduler {
        let tools_arc: Arc<[ToolDefinition]> = Arc::from(tools);
        EarlyToolScheduler::new(
            Arc::clone(&self.tool_registry),
            self.make_runtime_context(tools_arc),
            max_parallel,
        )
    }
}
