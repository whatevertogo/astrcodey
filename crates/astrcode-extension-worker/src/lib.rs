//! Worker-side S5R runtime. Authoring contracts remain owned by the SDK and contract crates.

pub(crate) use astrcode_extension_sdk::{
    WireErrorCode, builder, event, extension, host, llm, model_stream, s5r, session, tool,
};

mod worker;

pub use worker::Worker;

pub mod testing {
    pub use super::worker::testing::*;
}

pub mod worker_prelude {
    pub use astrcode_extension_contract::session_inspect::{
        HostSessionInspectRequest, SessionHistorySnapshotOutput, SessionInspectListItem,
        SessionInspectListOutput, SessionInspectProviderMessagesOutput, SessionInspectReadModel,
        SessionInspectReadModelOutput, SessionInspectSnapshot, SessionInspectSnapshotOutput,
    };

    pub use crate::{
        builder::worker_tool as tool,
        event::EventDeliveryReceipt,
        extension::{
            CustomEventDeclaration, CustomEventDisposition, CustomEventSubscription,
            ExtensionCapability, ExtensionHttpDispatchRequest, ExtensionHttpMethod,
            ExtensionHttpRequest, ExtensionHttpResponse, ExtensionHttpRoute, HookMode,
            LifecycleEvent,
        },
        llm::LlmMessage,
        model_stream::{ModelStream, ModelStreamEvent},
        s5r::{CallContinuation, ErrorPayload, HandlerEffect, HandlerResult},
        session::{SessionPhaseDto, SessionToolSelectionDto},
        worker::{
            ContinuationHandlerFn, CustomEventHandlerFn, EventClient, ExtensionHttpClient,
            HookHandlerFn, HostClient, HostConfigureSessionToolsOutput,
            HostConfigureSessionToolsRequest, HostCreateSessionOutput, HostCreateSessionRequest,
            HostEventEmitOutput, HostEventEmitRequest, HostLlmChatOutput,
            HostLlmCollectedStreamOutput, HostLlmContent, HostLlmMessage, HostLlmRole,
            HostNetworkRedirectPolicy, HostNetworkRequest, HostNetworkResponse, HostProcessOutput,
            HostProcessRequest, HostRecycleSessionRequest, HostRootSubmitTurnRequest,
            HostSessionCancelOutput, HostSessionDeliveryOutput, HostSessionEvent,
            HostSessionEventsPageOutput, HostSessionEventsPageRequest, HostSessionExecutionView,
            HostSessionInputRequest, HostSessionProviderMessagesOutput,
            HostSessionReactivateOutput, HostSessionStateOutput, HostSessionStateReadOutput,
            HostSessionStateReadRequest, HostSessionStateWriteRequest, HostSessionSummariesOutput,
            HostSessionSummary, HostSessionTargetRequest, HostSessionTokenUsage,
            HostSessionTokenUsageOutput, HostSessionTranscript, HostSessionTranscriptMessage,
            HostSubmitTurnOutput, HostSubmitTurnRequest, HostWorkspaceEditOutput,
            HostWorkspaceEditRequest, HostWorkspaceGlobOutput, HostWorkspaceGlobRequest,
            HostWorkspaceGrepMatch, HostWorkspaceGrepOutput, HostWorkspaceGrepRequest,
            HostWorkspaceListEntry, HostWorkspaceListOutput, HostWorkspaceListRequest,
            HostWorkspaceReadOutput, HostWorkspaceReadRequest, HostWorkspaceWriteOutput,
            HostWorkspaceWriteRequest, HttpHandlerFn, ModelClient, NetworkClient, ProcessClient,
            SessionControlClient, SessionHistoryClient, SessionInspectClient, SessionStateClient,
            Worker, WorkerCallContext, WorkerCommandContext, WorkerCustomEventContext,
            WorkerInvocationContext, WorkspaceClient, command_handler, continuation_handler,
            continuation_handler_args, custom_event_handler, custom_event_handler_args,
            hook_handler, hook_handler_args, http_handler, parse_hook_input, parse_tool_arguments,
            tool_handler, tool_handler_args, tool_text,
        },
    };
}
