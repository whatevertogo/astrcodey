//! Tool execution pipeline — preparation, execution, commit, and persistence.

mod commit;
mod events;
mod execute;
mod prepare;

use std::sync::Arc;

use astrcode_core::tool::{ToolDefinition, ToolResultArtifactReader};
use astrcode_extension_sdk::runtime_ports::TurnHooks;
use tokio_util::sync::CancellationToken;

use crate::{
    ToolRegistry,
    session::Session,
    tool_exec::{ToolCallRuntimeContext, TurnToolContext},
    turn_context::SharedTurnContext,
};

pub(crate) struct ToolCalls {
    turn: TurnToolContext,
    tool_registry: Arc<ToolRegistry>,
    extension_runner: Arc<dyn TurnHooks>,
    session: Session,
    cancellation_token: CancellationToken,
    max_parallel_tool_calls: usize,
}

impl ToolCalls {
    pub(crate) fn new(
        turn: TurnToolContext,
        tool_registry: Arc<ToolRegistry>,
        extension_runner: Arc<dyn TurnHooks>,
        session: Session,
        cancellation_token: CancellationToken,
        max_parallel_tool_calls: usize,
    ) -> Self {
        Self {
            turn,
            tool_registry,
            extension_runner,
            session,
            cancellation_token,
            max_parallel_tool_calls,
        }
    }

    pub(crate) fn list_definitions_with_prompt_metadata(
        &self,
    ) -> Vec<crate::tool_registry::DefinitionWithPromptMetadata> {
        self.tool_registry.list_definitions_with_prompt_metadata()
    }

    pub(crate) fn shared(&self) -> &SharedTurnContext {
        &self.turn.shared
    }

    pub(crate) fn shared_mut(&mut self) -> &mut SharedTurnContext {
        &mut self.turn.shared
    }

    pub(crate) fn max_parallel_tool_calls(&self) -> usize {
        self.max_parallel_tool_calls
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
}
