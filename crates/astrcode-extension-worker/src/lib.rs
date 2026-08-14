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
        config::ModelSelection,
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
            CustomEventDeclaration, CustomEventDisposition, CustomEventSubscription,
            ExtensionCapability, ExtensionCommandResult, ExtensionHttpDispatchRequest,
            ExtensionHttpMethod, ExtensionHttpRequest, ExtensionHttpResponse, ExtensionHttpRoute,
            HookMode, LifecycleEvent, SessionCommandIntent, SessionCommandKind, SlashCommand,
        },
        llm::LlmMessage,
        model_stream::{ModelStream, ModelStreamEvent},
        s5r::{CallContinuation, ErrorPayload, HandlerEffect, HandlerResult},
        session::{SessionMessageOriginDto, SessionPhaseDto, SessionToolSelectionDto},
        tool::{HostResource, ResourceAccess, ToolPlan},
        worker::{
            ContinuationHandlerFn, CustomEventHandlerFn, EventClient, ExtensionHttpClient,
            HookHandlerFn, HostClient, HostConfigureSessionToolsOutput,
            HostConfigureSessionToolsRequest, HostCreateSessionOutput, HostCreateSessionRequest,
            HostEventEmitOutput, HostEventEmitRequest, HostLlmChatOutput, HostLlmChatRequest,
            HostLlmContent, HostLlmMessage, HostLlmRole, HostNetworkRedirectPolicy,
            HostNetworkRequest, HostNetworkResponse, HostOperation, HostProcessHandleOutput,
            HostProcessInputAction, HostProcessInputRequest, HostProcessListOutput,
            HostProcessOutput, HostProcessReadOutput, HostProcessReadRequest, HostProcessRequest,
            HostProcessStartRequest, HostProcessState, HostProcessStatusOutput,
            HostProcessTargetRequest, HostRecycleSessionRequest, HostRootSubmitTurnRequest,
            HostSessionCancelOutput, HostSessionDeliveryOutput, HostSessionEvent,
            HostSessionEventsPageOutput, HostSessionEventsPageRequest, HostSessionExecutionView,
            HostSessionInputRequest, HostSessionProviderMessagesOutput,
            HostSessionReactivateOutput, HostSessionStateOutput, HostSessionStateReadOutput,
            HostSessionStateReadRequest, HostSessionStateWriteRequest, HostSessionSummariesOutput,
            HostSessionSummary, HostSessionTargetRequest, HostSessionTokenUsage,
            HostSessionTokenUsageOutput, HostSessionTranscript, HostSessionTranscriptMessage,
            HostSubmitTurnOutput, HostSubmitTurnRequest, HostToolResultReadOutput,
            HostToolResultReadRequest, HostWorkspaceApplyPatchOutput,
            HostWorkspaceApplyPatchRequest, HostWorkspaceEditOutput, HostWorkspaceEditRequest,
            HostWorkspaceGlobOutput, HostWorkspaceGlobRequest, HostWorkspaceGrepContextLine,
            HostWorkspaceGrepEntry, HostWorkspaceGrepMode, HostWorkspaceGrepOutput,
            HostWorkspaceGrepRequest, HostWorkspaceListEntry, HostWorkspaceListOutput,
            HostWorkspaceListRequest, HostWorkspaceReadOutput, HostWorkspaceReadRequest,
            HostWorkspaceTextChange, HostWorkspaceWriteOutput, HostWorkspaceWriteRequest,
            HttpHandlerFn, ModelClient, NetworkClient, ProcessClient, SessionControlClient,
            SessionHistoryClient, SessionInspectClient, SessionStateClient, ToolPlannerFn,
            ToolResultClient, Worker, WorkerCallContext, WorkerCommandContext,
            WorkerCommandInvocation, WorkerCustomEventContext, WorkerInvocationContext,
            WorkerToolPlanContext, WorkspaceClient, command_handler, continuation_handler,
            continuation_handler_args, custom_event_handler, custom_event_handler_args,
            hook_handler, hook_handler_args, http_handler, llm_chat_request, parse_hook_input,
            parse_tool_arguments, tool_handler, tool_handler_args, tool_planner, tool_planner_args,
            tool_text,
        },
    };
}
