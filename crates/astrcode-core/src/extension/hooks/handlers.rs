//! Handler traits implemented by extension authors.

use std::sync::Arc;

use super::{
    commands::{CommandCompletions, ExtensionCommandResult, SlashCommand},
    contexts::{
        AfterToolResultsContext, CommandContext, CompactContext, ContinueAfterStopContext,
        LifecycleContext, PostToolUseContext, PostToolUseFailureContext, PreToolUseContext,
        PromptBuildContext, ProviderContext, UserMessageEnvelopeContext,
    },
    results::{
        AfterToolResultsResult, CompactResult, ContinueAfterStopResult, HookResult,
        PostToolUseResult, PreToolUseResult, ProviderResult, UserMessageEnvelopeResult,
    },
    types::ExtensionError,
};
use crate::tool::{ToolDefinition, ToolExecutionContext, ToolPromptMetadata, ToolResult};

/// PreToolUse 钩子处理器。
#[async_trait::async_trait]
pub trait PreToolUseHandler: Send + Sync {
    async fn handle(&self, ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError>;
}

/// PostToolUse 钩子处理器。
#[async_trait::async_trait]
pub trait PostToolUseHandler: Send + Sync {
    async fn handle(&self, ctx: PostToolUseContext) -> Result<PostToolUseResult, ExtensionError>;
}

/// Provider 钩子处理器。
#[async_trait::async_trait]
pub trait ProviderHandler: Send + Sync {
    async fn handle(&self, ctx: ProviderContext) -> Result<ProviderResult, ExtensionError>;
}

/// PromptBuild 钩子处理器。
#[async_trait::async_trait]
pub trait PromptBuildHandler: Send + Sync {
    async fn handle(
        &self,
        ctx: PromptBuildContext,
    ) -> Result<super::types::PromptContributions, ExtensionError>;
}

/// Compact 钩子处理器。
#[async_trait::async_trait]
pub trait CompactHandler: Send + Sync {
    async fn handle(&self, ctx: CompactContext) -> Result<CompactResult, ExtensionError>;
}

/// PostToolUseFailure 通知型钩子处理器。
#[async_trait::async_trait]
pub trait PostToolUseFailureHandler: Send + Sync {
    async fn handle(&self, ctx: PostToolUseFailureContext) -> Result<(), ExtensionError>;
}

/// 通用生命周期钩子处理器。
#[async_trait::async_trait]
pub trait LifecycleHandler: Send + Sync {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError>;
}

/// LLM 返回纯文本结束后的继续决策钩子。
#[async_trait::async_trait]
pub trait ContinueAfterStopHandler: Send + Sync {
    async fn handle(
        &self,
        ctx: ContinueAfterStopContext,
    ) -> Result<ContinueAfterStopResult, ExtensionError>;
}

/// 用户消息 envelope 变换钩子。
#[async_trait::async_trait]
pub trait UserMessageEnvelopeHandler: Send + Sync {
    async fn handle(
        &self,
        ctx: UserMessageEnvelopeContext,
    ) -> Result<UserMessageEnvelopeResult, ExtensionError>;
}

/// 工具结果批次落盘后的继续/结束决策钩子。
#[async_trait::async_trait]
pub trait AfterToolResultsHandler: Send + Sync {
    async fn handle(
        &self,
        ctx: AfterToolResultsContext,
    ) -> Result<AfterToolResultsResult, ExtensionError>;
}

/// 工具执行处理器。
#[async_trait::async_trait]
pub trait ToolHandler: Send + Sync {
    async fn execute(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        working_dir: &str,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError>;
}

/// 命令执行处理器。
#[async_trait::async_trait]
pub trait CommandHandler: Send + Sync {
    async fn execute(
        &self,
        command_name: &str,
        args: &str,
        working_dir: &str,
        ctx: &CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError>;

    async fn complete(
        &self,
        _command_name: &str,
        _argument: &str,
        _cursor: usize,
        _working_dir: &str,
        _ctx: &CommandContext,
    ) -> Result<CommandCompletions, ExtensionError> {
        Ok(CommandCompletions::default())
    }
}

/// 动态工具发现处理器。
#[async_trait::async_trait]
pub trait ToolDiscoveryHandler: Send + Sync {
    async fn discover(&self, working_dir: &str) -> Vec<DiscoveredTool>;
}

/// 动态命令发现处理器。
#[async_trait::async_trait]
pub trait CommandDiscoveryHandler: Send + Sync {
    async fn discover(&self, working_dir: &str) -> Vec<(SlashCommand, Arc<dyn CommandHandler>)>;
}

/// Tool contributed by dynamic discovery.
#[derive(Clone)]
pub struct DiscoveredTool {
    pub definition: ToolDefinition,
    pub handler: Arc<dyn ToolHandler>,
    pub prompt_metadata: Option<ToolPromptMetadata>,
}
