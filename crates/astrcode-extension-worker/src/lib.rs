//! Worker-side S5R runtime. Authoring contracts remain owned by the SDK and contract crates.

pub(crate) use astrcode_extension_sdk::{
    WireErrorCode, builder, event, extension, host, llm, model_stream, s5r, session, tool,
};

mod worker;

pub use worker::Worker;

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    pub use super::worker::testing::*;
}

pub mod worker_prelude {
    pub use astrcode_extension_sdk::{
        WireErrorCode,
        config::ModelSelection,
        s5r::hooks::{
            ContinueAfterStopHookInput, PostCompactHookInput, PostToolUseHookInput,
            PreCompactHookInput, PromptBuildHookInput, ProviderContributionHookInput,
            ProviderHookInput, ToolUseHookInput,
        },
        wire::session_inspect::{
            HostSessionInspectRequest, SessionHistorySnapshotOutput, SessionInspectListItem,
            SessionInspectListOutput, SessionInspectProviderMessagesOutput,
            SessionInspectReadModel, SessionInspectReadModelOutput, SessionInspectSnapshot,
            SessionInspectSnapshotOutput,
        },
    };

    pub use crate::{
        builder::{command, worker_tool as tool},
        event::EventDeliveryReceipt,
        extension::{
            CommandAvailability, CommandCompletionItem, CommandCompletions, CommandExecution,
            CompactContributions, CompactRetainedContext, CompactTrigger, ContinueAfterStopResult,
            CustomEventDeclaration, CustomEventDelivery, CustomEventDisposition,
            CustomEventSubscription, ExtensionCapability, ExtensionCommandResult,
            ExtensionHttpDispatchRequest, ExtensionHttpMethod, ExtensionHttpRequest,
            ExtensionHttpResponse, ExtensionHttpRoute, HookMode, HookResult, LifecycleEvent,
            PostToolUseResult, PreCompactResult, PreToolUseResult, PromptContributions,
            ProviderResult, SessionCommandKind, SlashCommand, ToolInputTransformResult,
            TransportFeature,
        },
        llm::LlmMessage,
        model_stream::{ModelStream, ModelStreamEvent},
        s5r::{
            CallContinuation, ErrorPayload, HandlerEffect, HandlerResult, ProviderContributionData,
            ProviderContributionEffect,
        },
        session::{SessionMessageOriginDto, SessionPhaseDto, SessionToolSelectionDto},
        tool::{HostResource, ResourceAccess, ToolPlan, ToolPresentation, ToolResult},
        // worker 模块的 pub 面(HostClient、Host* DTO、context、handler 构造器)即
        // worker 作者面,在此整体导出,避免两处手工清单漂移。
        worker::*,
    };
}
