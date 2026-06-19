use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    extension::{
        AfterToolResultsContext, AfterToolResultsResult, CompactContext, CompactEvent,
        CompactResult, ContinueAfterStopContext, ContinueAfterStopResult, ExtensionError,
        ExtensionEvent, LifecycleContext, PostToolUseContext, PostToolUseFailureContext,
        PostToolUseResult, PreToolUseContext, PreToolUseResult, PromptBuildContext,
        PromptContributions, ProviderContext, ProviderEvent, ProviderResult,
        UserMessageEnvelopeContext, UserMessageEnvelopeResult,
    },
    tool::{SessionOperations, Tool, ToolPromptMetadata},
};

/// Session-facing extension runtime contract.
///
/// Implementations may be in-process runners, IPC bridges, or no-op runtimes.
/// Session code depends on this interface instead of concrete extension loader
/// or runner implementations.
#[async_trait::async_trait]
pub trait ExtensionRuntime: Send + Sync {
    async fn emit_pre_tool_use(
        &self,
        ctx: PreToolUseContext,
    ) -> Result<PreToolUseResult, ExtensionError>;

    async fn emit_post_tool_use(
        &self,
        ctx: PostToolUseContext,
    ) -> Result<PostToolUseResult, ExtensionError>;

    async fn emit_provider(
        &self,
        event: ProviderEvent,
        ctx: ProviderContext,
    ) -> Result<ProviderResult, ExtensionError>;

    async fn collect_prompt_contributions(
        &self,
        ctx: PromptBuildContext,
    ) -> Result<PromptContributions, ExtensionError>;

    async fn emit_compact(
        &self,
        event: CompactEvent,
        ctx: CompactContext,
    ) -> Result<CompactResult, ExtensionError>;

    async fn emit_post_tool_use_failure(&self, ctx: PostToolUseFailureContext);

    async fn emit_continue_after_stop(
        &self,
        ctx: ContinueAfterStopContext,
    ) -> Result<ContinueAfterStopResult, ExtensionError>;

    async fn emit_user_message_envelope(
        &self,
        ctx: UserMessageEnvelopeContext,
    ) -> Result<UserMessageEnvelopeResult, ExtensionError>;

    async fn emit_after_tool_results(
        &self,
        ctx: AfterToolResultsContext,
    ) -> Result<AfterToolResultsResult, ExtensionError>;

    async fn emit_lifecycle(
        &self,
        event: ExtensionEvent,
        ctx: LifecycleContext,
    ) -> Result<(), ExtensionError>;

    async fn collect_tool_adapters(&self, working_dir: &str) -> Vec<Arc<dyn Tool>>;

    async fn collect_tool_prompt_metadata(&self) -> HashMap<String, ToolPromptMetadata>;

    fn session_ops(&self) -> Option<Arc<dyn SessionOperations>>;
}

/// Empty extension runtime for embedded hosts that do not need extensions.
pub struct NoopExtensionRuntime;

#[async_trait::async_trait]
impl ExtensionRuntime for NoopExtensionRuntime {
    async fn emit_pre_tool_use(
        &self,
        _ctx: PreToolUseContext,
    ) -> Result<PreToolUseResult, ExtensionError> {
        Ok(PreToolUseResult::Allow)
    }

    async fn emit_post_tool_use(
        &self,
        _ctx: PostToolUseContext,
    ) -> Result<PostToolUseResult, ExtensionError> {
        Ok(PostToolUseResult::Allow)
    }

    async fn emit_provider(
        &self,
        _event: ProviderEvent,
        _ctx: ProviderContext,
    ) -> Result<ProviderResult, ExtensionError> {
        Ok(ProviderResult::Allow)
    }

    async fn collect_prompt_contributions(
        &self,
        _ctx: PromptBuildContext,
    ) -> Result<PromptContributions, ExtensionError> {
        Ok(PromptContributions::default())
    }

    async fn emit_compact(
        &self,
        _event: CompactEvent,
        _ctx: CompactContext,
    ) -> Result<CompactResult, ExtensionError> {
        Ok(CompactResult::Allow)
    }

    async fn emit_post_tool_use_failure(&self, _ctx: PostToolUseFailureContext) {}

    async fn emit_continue_after_stop(
        &self,
        _ctx: ContinueAfterStopContext,
    ) -> Result<ContinueAfterStopResult, ExtensionError> {
        Ok(ContinueAfterStopResult::EndTurn)
    }

    async fn emit_user_message_envelope(
        &self,
        _ctx: UserMessageEnvelopeContext,
    ) -> Result<UserMessageEnvelopeResult, ExtensionError> {
        Ok(UserMessageEnvelopeResult::Allow)
    }

    async fn emit_after_tool_results(
        &self,
        _ctx: AfterToolResultsContext,
    ) -> Result<AfterToolResultsResult, ExtensionError> {
        Ok(AfterToolResultsResult::Continue)
    }

    async fn emit_lifecycle(
        &self,
        _event: ExtensionEvent,
        _ctx: LifecycleContext,
    ) -> Result<(), ExtensionError> {
        Ok(())
    }

    async fn collect_tool_adapters(&self, _working_dir: &str) -> Vec<Arc<dyn Tool>> {
        Vec::new()
    }

    async fn collect_tool_prompt_metadata(&self) -> HashMap<String, ToolPromptMetadata> {
        HashMap::new()
    }

    fn session_ops(&self) -> Option<Arc<dyn SessionOperations>> {
        None
    }
}
