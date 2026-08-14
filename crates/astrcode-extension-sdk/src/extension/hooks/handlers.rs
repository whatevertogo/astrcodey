//! Handler traits implemented by extension authors.

use std::sync::Arc;

use super::{
    commands::{CommandCompletions, ExtensionCommandResult, SlashCommand},
    contexts::{
        CommandCompletionContext, CommandContext, CommandDiscoveryContext, CompactContext,
        ContinueAfterStopContext, LifecycleContext, PostToolUseContext, PreToolUseContext,
        PromptBuildContext, ProviderContext, ProviderSettlementContext, ToolDiscoveryContext,
        UserMessageEnvelopeContext,
    },
    results::{
        CompactResult, ContinueAfterStopResult, HookResult, PostToolUseResult, PreToolUseResult,
        PreparedProviderContribution, ProviderResult, ToolInputTransformResult,
        UserMessageEnvelopeResult,
    },
    types::ExtensionError,
};
use crate::{
    extension::{ToolContext, ToolPlanContext},
    tool::{ToolDefinition, ToolExecutionResult, ToolPlan, ToolPromptMetadata},
};

/// 工具参数变换处理器。
#[async_trait::async_trait]
pub trait ToolInputTransformHandler: Send + Sync {
    async fn transform(
        &self,
        ctx: PreToolUseContext,
    ) -> Result<ToolInputTransformResult, ExtensionError>;
}

/// PreToolUse 准入处理器。
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

/// Stateful request-local contribution with an explicit prepare/acknowledge lifecycle.
#[async_trait::async_trait]
pub trait ProviderContributionHandler: Send + Sync {
    async fn prepare(
        &self,
        ctx: ProviderContext,
    ) -> Result<Option<PreparedProviderContribution>, ExtensionError>;

    /// Acknowledge one exact pending contribution after its provider cycle is durably committed.
    async fn acknowledge(&self, ctx: ProviderSettlementContext) -> Result<(), ExtensionError>;
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

/// 工具执行处理器。
#[async_trait::async_trait]
pub trait ToolHandler: Send + Sync {
    /// Interpret the final tool arguments into the resources required by execution.
    async fn plan(&self, ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError>;

    async fn execute(&self, ctx: ToolContext) -> Result<ToolExecutionResult, ExtensionError>;
}

/// 命令执行处理器。
#[async_trait::async_trait]
pub trait CommandHandler: Send + Sync {
    async fn execute(&self, ctx: CommandContext) -> Result<ExtensionCommandResult, ExtensionError>;

    async fn complete(
        &self,
        _ctx: CommandCompletionContext,
    ) -> Result<CommandCompletions, ExtensionError> {
        Ok(CommandCompletions::default())
    }

    /// Whether this handler implements argument completion.
    ///
    /// The registrar uses this to reject declarations that advertise completion while retaining
    /// the default no-op implementation above.
    fn supports_argument_completions(&self) -> bool {
        false
    }
}

/// 动态工具发现处理器。
#[async_trait::async_trait]
pub trait ToolDiscoveryHandler: Send + Sync {
    async fn discover(&self, ctx: ToolDiscoveryContext) -> Result<ToolDiscovery, ExtensionError>;
}

/// 动态命令发现处理器。
#[async_trait::async_trait]
pub trait CommandDiscoveryHandler: Send + Sync {
    async fn discover(
        &self,
        ctx: CommandDiscoveryContext,
    ) -> Result<CommandDiscovery, ExtensionError>;
}

/// Tool contributed by dynamic discovery.
#[derive(Clone)]
pub struct DiscoveredTool {
    definition: ToolDefinition,
    handler: Arc<dyn ToolHandler>,
    prompt_metadata: Option<ToolPromptMetadata>,
}

impl DiscoveredTool {
    pub fn new(definition: ToolDefinition, handler: Arc<dyn ToolHandler>) -> Self {
        Self {
            definition,
            handler,
            prompt_metadata: None,
        }
    }

    pub fn prompt_metadata(mut self, metadata: ToolPromptMetadata) -> Self {
        self.prompt_metadata = Some(metadata);
        self
    }

    pub fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    pub fn handler(&self) -> &Arc<dyn ToolHandler> {
        &self.handler
    }

    pub fn prompt(&self) -> Option<&ToolPromptMetadata> {
        self.prompt_metadata.as_ref()
    }

    pub fn into_parts(
        self,
    ) -> (
        ToolDefinition,
        Arc<dyn ToolHandler>,
        Option<ToolPromptMetadata>,
    ) {
        (self.definition, self.handler, self.prompt_metadata)
    }
}

/// Complete result of one dynamic tool discovery pass.
#[derive(Clone, Default)]
pub struct ToolDiscovery {
    tools: Vec<DiscoveredTool>,
}

impl ToolDiscovery {
    pub fn new(tools: Vec<DiscoveredTool>) -> Self {
        Self { tools }
    }

    pub fn tools(&self) -> &[DiscoveredTool] {
        &self.tools
    }

    pub fn into_tools(self) -> Vec<DiscoveredTool> {
        self.tools
    }
}

impl From<Vec<DiscoveredTool>> for ToolDiscovery {
    fn from(tools: Vec<DiscoveredTool>) -> Self {
        Self::new(tools)
    }
}

/// Command and handler contributed atomically by dynamic discovery.
#[derive(Clone)]
pub struct DiscoveredCommand {
    command: SlashCommand,
    handler: Arc<dyn CommandHandler>,
}

impl DiscoveredCommand {
    pub fn new(command: SlashCommand, handler: Arc<dyn CommandHandler>) -> Self {
        Self { command, handler }
    }

    pub fn command(&self) -> &SlashCommand {
        &self.command
    }

    pub fn handler(&self) -> &Arc<dyn CommandHandler> {
        &self.handler
    }

    pub fn into_parts(self) -> (SlashCommand, Arc<dyn CommandHandler>) {
        (self.command, self.handler)
    }
}

/// Complete result of one dynamic command discovery pass.
#[derive(Clone, Default)]
pub struct CommandDiscovery {
    commands: Vec<DiscoveredCommand>,
}

impl CommandDiscovery {
    pub fn new(commands: Vec<DiscoveredCommand>) -> Self {
        Self { commands }
    }

    pub fn commands(&self) -> &[DiscoveredCommand] {
        &self.commands
    }

    pub fn into_commands(self) -> Vec<DiscoveredCommand> {
        self.commands
    }
}

impl From<Vec<DiscoveredCommand>> for CommandDiscovery {
    fn from(commands: Vec<DiscoveredCommand>) -> Self {
        Self::new(commands)
    }
}
