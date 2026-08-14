//! 扩展系统类型定义。
//!
//! 扩展是 astrcode 的主要扩展机制。技能、Agent 配置、自定义工具和斜杠命令
//! 都通过这里定义的稳定契约挂入宿主。
//!
//! 本模块只定义契约（trait、capability、hook 类型）：扩展的发现、加载、
//! 路由与进程管理位于 `astrcode-extensions`。

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
    CustomEventContext, CustomEventDeclaration, CustomEventDisposition, CustomEventEmitError,
    CustomEventEmitter, CustomEventHandler, CustomEventSourceFilter, CustomEventSubscription,
    DEFAULT_CUSTOM_EVENT_DURABLE, DEFAULT_CUSTOM_EVENT_MAX_PAYLOAD_BYTES,
    DEFAULT_CUSTOM_EVENT_SCHEMA_VERSION, LifecycleEvent, MAX_CUSTOM_EVENT_PAYLOAD_BYTES,
    MAX_CUSTOM_EVENT_SUBSCRIPTION_ID_LEN,
};
pub use hooks::{
    CommandAvailability, CommandCompletionContext, CommandCompletionItem, CommandCompletions,
    CommandContext, CommandDiscovery, CommandDiscoveryContext, CommandDiscoveryHandler,
    CommandExecution, CommandHandler, CompactContext, CompactContributions, CompactEvent,
    CompactHandler, CompactPayload, CompactResult, ContinueAfterStopContext,
    ContinueAfterStopHandler, ContinueAfterStopLimit, ContinueAfterStopOptions,
    ContinueAfterStopPayload, ContinueAfterStopRegistration, ContinueAfterStopResult,
    DiscoveredCommand, DiscoveredTool, ExchangeSummary, ExtensionCommandResult, ExtensionError,
    HookContext, HookMode, HookResult, LifecycleContext, LifecycleHandler, LifecyclePayload,
    PostToolUseContext, PostToolUseHandler, PostToolUsePayload, PostToolUseResult,
    PreToolUseAdmission, PreToolUseContext, PreToolUseHandler, PreToolUsePayload,
    PreToolUseRequirement, PreToolUseResult, PreparedProviderContribution, PreparedProviderEffect,
    PromptBuildContext, PromptBuildHandler, PromptBuildPayload, PromptContributions,
    ProviderContext, ProviderContributionHandler, ProviderContributionId, ProviderEvent,
    ProviderHandler, ProviderPayload, ProviderRequestId, ProviderResult, ProviderSettlementContext,
    ProviderSettlementPayload, SessionCommandIntent, SessionCommandKind, SlashCommand,
    StatusItemUpdatePayload, ToolDiscovery, ToolDiscoveryContext, ToolDiscoveryHandler,
    ToolHandler, ToolHookRegistration, ToolHookTarget, ToolInputTransformHandler,
    ToolInputTransformResult, ToolUseRegistration, UserMessageEnvelopeContext,
    UserMessageEnvelopeHandler, UserMessageEnvelopePayload, UserMessageEnvelopeRegistration,
    UserMessageEnvelopeResult,
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

pub use crate::manifest::{ExtensionManifest, ExtensionManifestError};
