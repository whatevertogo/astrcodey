//! Extension system type definitions.
//!
//! Extensions are astrcode's primary extension mechanism. Skills, agent configurations, custom
//! tools, and slash commands all hook into the host through the stable contracts defined here.
//!
//! This module only defines contracts (traits, capabilities, hook types): extension discovery,
//! loading, routing, and process management live in `astrcode-extensions`.

mod call_context;
mod events;
mod hooks;
mod http;
mod lifecycle;
mod package_manifest;
mod paths;
mod registrar;
mod registration_validation;
mod runtime;
mod tool_context;
mod tool_plan_context;

/// Runtime-only construction seam. This module is intentionally absent from author preludes.
#[doc(hidden)]
pub mod internal;

pub use astrcode_core::{
    compaction::{CompactStrategy, CompactTrigger},
    tool::SessionToolSelection,
};
pub use call_context::{
    ExtensionCall, ExtensionCallContext, ExtensionStartContext, SessionCallContext,
    WorkspaceCallContext,
};
pub use events::{
    CustomEventContext, CustomEventDeclaration, CustomEventDelivery, CustomEventDisposition,
    CustomEventEmitError, CustomEventEmitter, CustomEventHandler, CustomEventSourceFilter,
    CustomEventSubscription, DEFAULT_CUSTOM_EVENT_MAX_PAYLOAD_BYTES,
    DEFAULT_CUSTOM_EVENT_SCHEMA_VERSION, LifecycleEvent, MAX_CUSTOM_EVENT_PAYLOAD_BYTES,
    MAX_CUSTOM_EVENT_SUBSCRIPTION_ID_LEN,
};
pub use hooks::{
    CommandAvailability, CommandCompletionContext, CommandCompletionItem, CommandCompletions,
    CommandContext, CommandDiscovery, CommandDiscoveryContext, CommandDiscoveryHandler,
    CommandExecution, CommandHandler, CompactContributions, CompactEvent, CompactRetainedContext,
    ContinueAfterStopContext, ContinueAfterStopHandler, ContinueAfterStopLimit,
    ContinueAfterStopOptions, ContinueAfterStopPayload, ContinueAfterStopRegistration,
    ContinueAfterStopResult, DiscoveredCommand, DiscoveredTool, ExchangeSummary,
    ExtensionCommandResult, ExtensionError, HookContext, HookMode, HookResult, LifecycleContext,
    LifecycleHandler, LifecyclePayload, PostCompactContext, PostCompactHandler, PostCompactPayload,
    PostToolUseContext, PostToolUseHandler, PostToolUsePayload, PostToolUseResult,
    PreCompactContext, PreCompactHandler, PreCompactPayload, PreCompactResult, PreToolUseAdmission,
    PreToolUseContext, PreToolUseHandler, PreToolUsePayload, PreToolUseRequirement,
    PreToolUseResult, PreparedProviderContribution, PreparedProviderEffect, PromptBuildContext,
    PromptBuildHandler, PromptBuildPayload, PromptContributions, ProviderContext,
    ProviderContributionHandler, ProviderContributionId, ProviderEvent, ProviderHandler,
    ProviderPayload, ProviderRequestId, ProviderResult, ProviderSettlementContext,
    ProviderSettlementPayload, SessionCommandKind, SlashCommand, StatusItemUpdatePayload,
    ToolDiscovery, ToolDiscoveryContext, ToolDiscoveryHandler, ToolHandler, ToolHookRegistration,
    ToolHookTarget, ToolInputTransformHandler, ToolInputTransformResult, ToolUseRegistration,
    UserMessageEnvelopeContext, UserMessageEnvelopeHandler, UserMessageEnvelopePayload,
    UserMessageEnvelopeRegistration, UserMessageEnvelopeResult,
};
pub use http::{
    DEFAULT_EXTENSION_HTTP_BODY_BYTES, ExtensionHttpAccess, ExtensionHttpDispatchRequest,
    ExtensionHttpHandler, ExtensionHttpMethod, ExtensionHttpRequest, ExtensionHttpResponse,
    ExtensionHttpRoute, ExtensionHttpRouteRegistration, HttpContext, MAX_EXTENSION_HTTP_BODY_BYTES,
};
pub use lifecycle::Extension;
pub use package_manifest::{ExtensionPackageManifest, ExtensionPackageProtocol};
pub use paths::{ExtensionPathError, ExtensionPaths};
pub use registrar::{
    CustomEventRegistration, ExtensionRegistrations, Keybinding, Registrar, RegistrationError,
    StatusItem, ToolRegistration,
};
pub use runtime::{
    ExtensionCapability, ExtensionConfig, ExtensionConfigError, ExtensionStopContext,
    ExtensionTaskError, ExtensionTasks, StopReason,
};
pub use tool_context::ToolContext;
pub use tool_plan_context::ToolPlanContext;

pub use crate::{
    manifest::{ExtensionManifest, ExtensionManifestError},
    transport::TransportFeature,
};

/// Deserialize tool arguments, producing an `InvalidInput` with the JSON path and a schema hint
/// on failure.
///
/// Shared by [`ToolContext::arguments`] and [`ToolPlanContext::arguments`].
pub(crate) fn parse_tool_arguments<T: serde::de::DeserializeOwned>(
    tool_name: &str,
    arguments: &serde_json::Value,
) -> Result<T, ExtensionError> {
    serde_path_to_error::deserialize(arguments).map_err(|error| {
        let path = error.path().to_string();
        let path = if path.is_empty() { "$" } else { path.as_str() };
        ExtensionError::invalid_input(
            format!(
                "tool `{tool_name}` arguments at `{path}`: {}",
                error.into_inner()
            ),
            format!("check the `{tool_name}` tool arguments against its declared JSON schema"),
        )
    })
}
