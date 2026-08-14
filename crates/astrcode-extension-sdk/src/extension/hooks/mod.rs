//! Extension hook types: contexts, handlers, results, commands, and registration structs.

mod commands;
mod contexts;
mod handlers;
mod results;
mod types;

pub use commands::{
    CommandAvailability, CommandCompletionItem, CommandCompletions, CommandExecution,
    ExtensionCommandResult, SessionCommandIntent, SessionCommandKind, SlashCommand,
};
pub use contexts::{
    CommandCompletionContext, CommandContext, CommandDiscoveryContext, CompactContext,
    CompactPayload, ContinueAfterStopContext, ContinueAfterStopPayload, HookContext,
    LifecycleContext, LifecyclePayload, PostToolUseContext, PostToolUsePayload, PreToolUseContext,
    PreToolUsePayload, PromptBuildContext, PromptBuildPayload, ProviderContext, ProviderPayload,
    ProviderSettlementContext, ProviderSettlementPayload, ToolDiscoveryContext,
    UserMessageEnvelopeContext, UserMessageEnvelopePayload,
};
// `extension::internal` is the only public path to dispatcher-owned hook inputs.
pub use contexts::{
    HookInput, RuntimeCompactContext, RuntimeContinueAfterStopContext, RuntimeHookCallContext,
    RuntimeLifecycleContext, RuntimePostToolUseContext, RuntimePreToolUseContext,
    RuntimePromptBuildContext, RuntimeProviderContext, RuntimeProviderSettlementContext,
    RuntimeUserMessageEnvelopeContext,
};
pub use handlers::{
    CommandDiscovery, CommandDiscoveryHandler, CommandHandler, CompactHandler,
    ContinueAfterStopHandler, DiscoveredCommand, DiscoveredTool, LifecycleHandler,
    PostToolUseHandler, PreToolUseHandler, PromptBuildHandler, ProviderContributionHandler,
    ProviderHandler, ToolDiscovery, ToolDiscoveryHandler, ToolHandler, ToolInputTransformHandler,
    UserMessageEnvelopeHandler,
};
pub use results::{
    CompactResult, ContinueAfterStopResult, HookResult, PostToolUseResult, PreToolUseAdmission,
    PreToolUseRequirement, PreToolUseResult, PreparedProviderContribution, PreparedProviderEffect,
    ProviderResult, ToolInputTransformResult, UserMessageEnvelopeResult,
};
pub use types::{
    CompactContributions, CompactEvent, ContinueAfterStopLimit, ContinueAfterStopOptions,
    ContinueAfterStopRegistration, ExchangeSummary, ExtensionError, HookMode, PromptContributions,
    ProviderContributionId, ProviderEvent, ProviderRequestId, StatusItemUpdatePayload,
    ToolHookRegistration, ToolHookTarget, ToolUseRegistration, UserMessageEnvelopeRegistration,
};
