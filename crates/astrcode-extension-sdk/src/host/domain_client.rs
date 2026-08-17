use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use super::llm_mapping::llm_chat_request;
use crate::{
    extension::{ExtensionHttpDispatchRequest, ExtensionHttpResponse},
    host::{
        Acknowledgement, EmptyRequest, HostConfigureSessionToolsOutput,
        HostConfigureSessionToolsRequest, HostEventEmitOutput, HostLlmChatOutput,
        HostLlmChatRequest, HostNetworkRequest, HostNetworkResponse, HostOperation,
        HostProcessHandleOutput, HostProcessInputRequest, HostProcessListOutput, HostProcessOutput,
        HostProcessReadOutput, HostProcessReadRequest, HostProcessRequest, HostProcessStartRequest,
        HostProcessStatusOutput, HostProcessTargetRequest, HostSessionCancelOutput,
        HostSessionDeliveryOutput, HostSessionExecutionView, HostSessionInputRequest,
        HostSessionProviderMessagesOutput, HostSessionStateReadOutput, HostSessionStateReadRequest,
        HostSessionStateWriteRequest, HostSessionSummariesOutput, HostSessionTokenUsageOutput,
        HostSessionTranscript, HostToolResultReadOutput, HostToolResultReadRequest,
        HostWorkspaceApplyPatchOutput, HostWorkspaceApplyPatchRequest, HostWorkspaceEditOutput,
        HostWorkspaceEditRequest, HostWorkspaceGlobOutput, HostWorkspaceGlobRequest,
        HostWorkspaceGrepOutput, HostWorkspaceGrepRequest, HostWorkspaceListOutput,
        HostWorkspaceListRequest, HostWorkspaceReadOutput, HostWorkspaceReadRequest,
        HostWorkspaceWriteOutput, HostWorkspaceWriteRequest,
    },
    llm::LlmMessage,
    model_stream::ModelStream,
    session::{
        HostCreateSessionOutput, HostCreateSessionRequest, HostRecycleSessionRequest,
        HostRootSubmitTurnRequest, HostSessionEventsPageOutput, HostSessionEventsPageRequest,
        HostSessionReactivateOutput, HostSessionStateOutput, HostSessionTargetRequest,
        HostSubmitTurnOutput, HostSubmitTurnRequest,
    },
    wire::{
        HostOp, WireErrorCode, operations,
        protocol::ErrorPayload,
        session_inspect::{
            HostSessionInspectRequest, SessionHistorySnapshotOutput, SessionInspectListOutput,
            SessionInspectProviderMessagesOutput, SessionInspectReadModelOutput,
            SessionInspectSnapshotOutput,
        },
    },
};

#[async_trait]
pub trait HostClientTransport: Clone + Send + Sync {
    type Error;

    async fn invoke(&self, operation: HostOperation, input: Value) -> Result<Value, Self::Error>;

    async fn invoke_stream(
        &self,
        operation: HostOperation,
        _input: Value,
    ) -> Result<ModelStream, Self::Error> {
        Err(Self::client_error(
            WireErrorCode::StreamNotSupported,
            format!(
                "{} transport does not expose incremental streaming",
                operation.wire_name()
            ),
        ))
    }

    fn client_error(code: WireErrorCode, message: String) -> Self::Error;

    fn payload_error(error: ErrorPayload) -> Self::Error;
}

macro_rules! domain_client {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy)]
        pub struct $name<T> {
            transport: T,
        }

        impl<T> $name<T> {
            #[doc(hidden)]
            pub const fn new(transport: T) -> Self {
                Self { transport }
            }
        }
    };
}

domain_client!(ModelClient);
domain_client!(EventClient);
domain_client!(SessionControlClient);
domain_client!(SessionHistoryClient);
domain_client!(SessionStateClient);
domain_client!(SessionInspectClient);
domain_client!(ToolResultClient);
domain_client!(WorkspaceClient);
domain_client!(ProcessClient);
domain_client!(NetworkClient);
domain_client!(ExtensionHttpClient);

impl<T> ModelClient<T> {
    pub(crate) const fn transport(&self) -> &T {
        &self.transport
    }
}

impl<T: HostClientTransport> EventClient<T> {
    pub async fn emit(
        &self,
        request: crate::host::HostEventEmitRequest,
    ) -> Result<HostEventEmitOutput, T::Error> {
        invoke::<operations::EventEmit, _>(&self.transport, &request).await
    }
}

impl<T: HostClientTransport> ModelClient<T> {
    pub async fn main_chat(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<HostLlmChatOutput, T::Error> {
        self.main_chat_request(llm_chat_request(messages)).await
    }

    pub async fn main_chat_request(
        &self,
        request: HostLlmChatRequest,
    ) -> Result<HostLlmChatOutput, T::Error> {
        invoke::<operations::LlmMainChat, _>(&self.transport, &request).await
    }

    pub async fn small_chat(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<HostLlmChatOutput, T::Error> {
        self.small_chat_request(llm_chat_request(messages)).await
    }

    pub async fn small_chat_request(
        &self,
        request: HostLlmChatRequest,
    ) -> Result<HostLlmChatOutput, T::Error> {
        invoke::<operations::LlmSmallChat, _>(&self.transport, &request).await
    }

    /// Starts a main-model stream and exposes each event as it arrives.
    pub async fn main_chat_events(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<ModelStream, T::Error> {
        self.main_chat_events_request(llm_chat_request(messages))
            .await
    }

    pub async fn main_chat_events_request(
        &self,
        request: HostLlmChatRequest,
    ) -> Result<ModelStream, T::Error> {
        invoke_stream::<operations::LlmMainChat, _>(&self.transport, &request).await
    }

    /// Starts a small-model stream and exposes each event as it arrives.
    pub async fn small_chat_events(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<ModelStream, T::Error> {
        self.small_chat_events_request(llm_chat_request(messages))
            .await
    }

    pub async fn small_chat_events_request(
        &self,
        request: HostLlmChatRequest,
    ) -> Result<ModelStream, T::Error> {
        invoke_stream::<operations::LlmSmallChat, _>(&self.transport, &request).await
    }

    /// Runs the main model stream and returns its completed response.
    pub async fn main_chat_collected(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<HostLlmChatOutput, T::Error> {
        self.main_chat_collected_request(llm_chat_request(messages))
            .await
    }

    pub async fn main_chat_collected_request(
        &self,
        request: HostLlmChatRequest,
    ) -> Result<HostLlmChatOutput, T::Error> {
        collect_stream::<operations::LlmMainChat, _>(&self.transport, &request).await
    }

    /// Runs the small model stream and returns its completed response.
    pub async fn small_chat_collected(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<HostLlmChatOutput, T::Error> {
        self.small_chat_collected_request(llm_chat_request(messages))
            .await
    }

    pub async fn small_chat_collected_request(
        &self,
        request: HostLlmChatRequest,
    ) -> Result<HostLlmChatOutput, T::Error> {
        collect_stream::<operations::LlmSmallChat, _>(&self.transport, &request).await
    }
}

impl<T: HostClientTransport> SessionControlClient<T> {
    pub async fn create_root(&self) -> Result<HostCreateSessionOutput, T::Error> {
        invoke::<operations::SessionRootCreate, _>(&self.transport, &EmptyRequest::default()).await
    }

    pub async fn submit_root_turn(
        &self,
        request: HostRootSubmitTurnRequest,
    ) -> Result<HostSubmitTurnOutput, T::Error> {
        invoke::<operations::SessionRootSubmitTurn, _>(&self.transport, &request).await
    }

    pub async fn root_state(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionStateOutput, T::Error> {
        invoke::<operations::SessionRootState, _>(&self.transport, &request).await
    }

    pub async fn inject_or_start(
        &self,
        request: HostSessionInputRequest,
    ) -> Result<HostSessionDeliveryOutput, T::Error> {
        invoke::<operations::SessionControlInjectOrStart, _>(&self.transport, &request).await
    }

    /// Queues input behind the active turn (started automatically when it finishes),
    /// or starts a turn immediately when the session is idle.
    pub async fn queue_or_start(
        &self,
        request: HostSessionInputRequest,
    ) -> Result<HostSessionDeliveryOutput, T::Error> {
        invoke::<operations::SessionControlQueueOrStart, _>(&self.transport, &request).await
    }

    /// Appends input to the active turn only; fails with `no_active_turn` when the
    /// session is idle instead of starting or queueing a new turn.
    pub async fn defer_context(
        &self,
        request: HostSessionInputRequest,
    ) -> Result<HostSessionDeliveryOutput, T::Error> {
        invoke::<operations::SessionControlDeferContext, _>(&self.transport, &request).await
    }

    pub async fn interrupt_and_submit(
        &self,
        request: HostSessionInputRequest,
    ) -> Result<HostSessionDeliveryOutput, T::Error> {
        invoke::<operations::SessionControlInterruptAndSubmit, _>(&self.transport, &request).await
    }

    pub async fn cancel_turn(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionCancelOutput, T::Error> {
        invoke::<operations::SessionControlCancelTurn, _>(&self.transport, &request).await
    }

    pub async fn execution_view(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionExecutionView, T::Error> {
        invoke::<operations::SessionControlExecutionView, _>(&self.transport, &request).await
    }

    pub async fn state(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionStateOutput, T::Error> {
        invoke::<operations::SessionControlState, _>(&self.transport, &request).await
    }

    pub async fn reactivate(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionReactivateOutput, T::Error> {
        invoke::<operations::SessionControlReactivate, _>(&self.transport, &request).await
    }

    pub async fn create_child(
        &self,
        request: HostCreateSessionRequest,
    ) -> Result<HostCreateSessionOutput, T::Error> {
        invoke::<operations::SessionControlCreate, _>(&self.transport, &request).await
    }

    pub async fn submit_turn(
        &self,
        request: HostSubmitTurnRequest,
    ) -> Result<HostSubmitTurnOutput, T::Error> {
        invoke::<operations::SessionControlSubmitTurn, _>(&self.transport, &request).await
    }

    pub async fn configure_tools(
        &self,
        request: HostConfigureSessionToolsRequest,
    ) -> Result<HostConfigureSessionToolsOutput, T::Error> {
        invoke::<operations::SessionControlConfigureTools, _>(&self.transport, &request).await
    }

    pub async fn recycle(&self, request: HostRecycleSessionRequest) -> Result<(), T::Error> {
        invoke_ack::<operations::SessionControlDispose, _>(&self.transport, &request).await
    }
}

impl<T: HostClientTransport> SessionHistoryClient<T> {
    pub async fn list_summaries(&self) -> Result<HostSessionSummariesOutput, T::Error> {
        invoke::<operations::SessionHistoryList, _>(&self.transport, &EmptyRequest::default()).await
    }

    pub async fn transcript(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionTranscript, T::Error> {
        invoke::<operations::SessionHistoryTranscript, _>(&self.transport, &request).await
    }

    pub async fn provider_messages(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionProviderMessagesOutput, T::Error> {
        invoke::<operations::SessionHistoryProviderMessages, _>(&self.transport, &request).await
    }

    pub async fn token_usage(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionTokenUsageOutput, T::Error> {
        invoke::<operations::SessionHistoryTokenUsage, _>(&self.transport, &request).await
    }

    pub async fn events_page(
        &self,
        request: HostSessionEventsPageRequest,
    ) -> Result<HostSessionEventsPageOutput, T::Error> {
        invoke::<operations::SessionReadEvents, _>(&self.transport, &request).await
    }

    pub async fn snapshot(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<SessionHistorySnapshotOutput, T::Error> {
        invoke::<operations::SessionHistorySnapshot, _>(&self.transport, &request).await
    }
}

impl<T: HostClientTransport> SessionStateClient<T> {
    pub async fn read(
        &self,
        request: HostSessionStateReadRequest,
    ) -> Result<HostSessionStateReadOutput, T::Error> {
        invoke::<operations::SessionStateRead, _>(&self.transport, &request).await
    }

    pub async fn write(&self, request: HostSessionStateWriteRequest) -> Result<(), T::Error> {
        invoke_ack::<operations::SessionStateWrite, _>(&self.transport, &request).await
    }
}

impl<T: HostClientTransport> SessionInspectClient<T> {
    pub async fn list(&self) -> Result<SessionInspectListOutput, T::Error> {
        invoke::<operations::SessionInspectList, _>(&self.transport, &EmptyRequest::default()).await
    }

    pub async fn snapshot(
        &self,
        session_id: &str,
    ) -> Result<SessionInspectSnapshotOutput, T::Error> {
        self.inspect::<operations::SessionInspectSnapshot>(session_id)
            .await
    }

    pub async fn read_model(
        &self,
        session_id: &str,
    ) -> Result<SessionInspectReadModelOutput, T::Error> {
        self.inspect::<operations::SessionInspectReadModel>(session_id)
            .await
    }

    pub async fn provider_messages(
        &self,
        session_id: &str,
    ) -> Result<SessionInspectProviderMessagesOutput, T::Error> {
        self.inspect::<operations::SessionInspectProviderMessages>(session_id)
            .await
    }

    async fn inspect<Op>(&self, session_id: &str) -> Result<Op::Response, T::Error>
    where
        Op: HostOp<Request = HostSessionInspectRequest>,
    {
        invoke::<Op, _>(
            &self.transport,
            &HostSessionInspectRequest {
                session_id: session_id.into(),
            },
        )
        .await
    }
}

impl<T: HostClientTransport> ToolResultClient<T> {
    pub async fn read(
        &self,
        request: HostToolResultReadRequest,
    ) -> Result<HostToolResultReadOutput, T::Error> {
        invoke::<operations::ToolResultRead, _>(&self.transport, &request).await
    }
}

impl<T: HostClientTransport> WorkspaceClient<T> {
    pub async fn apply_patch(
        &self,
        request: HostWorkspaceApplyPatchRequest,
    ) -> Result<HostWorkspaceApplyPatchOutput, T::Error> {
        invoke::<operations::WorkspaceApplyPatch, _>(&self.transport, &request).await
    }

    pub async fn read(
        &self,
        request: HostWorkspaceReadRequest,
    ) -> Result<HostWorkspaceReadOutput, T::Error> {
        invoke::<operations::WorkspaceRead, _>(&self.transport, &request).await
    }

    pub async fn write(
        &self,
        request: HostWorkspaceWriteRequest,
    ) -> Result<HostWorkspaceWriteOutput, T::Error> {
        invoke::<operations::WorkspaceWrite, _>(&self.transport, &request).await
    }

    pub async fn edit(
        &self,
        request: HostWorkspaceEditRequest,
    ) -> Result<HostWorkspaceEditOutput, T::Error> {
        invoke::<operations::WorkspaceEdit, _>(&self.transport, &request).await
    }

    pub async fn list(
        &self,
        request: HostWorkspaceListRequest,
    ) -> Result<HostWorkspaceListOutput, T::Error> {
        invoke::<operations::WorkspaceList, _>(&self.transport, &request).await
    }

    pub async fn grep(
        &self,
        request: HostWorkspaceGrepRequest,
    ) -> Result<HostWorkspaceGrepOutput, T::Error> {
        invoke::<operations::WorkspaceGrep, _>(&self.transport, &request).await
    }

    pub async fn glob(
        &self,
        request: HostWorkspaceGlobRequest,
    ) -> Result<HostWorkspaceGlobOutput, T::Error> {
        invoke::<operations::WorkspaceGlob, _>(&self.transport, &request).await
    }
}

impl<T: HostClientTransport> ProcessClient<T> {
    pub async fn spawn(&self, request: HostProcessRequest) -> Result<HostProcessOutput, T::Error> {
        invoke::<operations::ProcessSpawn, _>(&self.transport, &request).await
    }

    pub async fn start(
        &self,
        request: HostProcessStartRequest,
    ) -> Result<HostProcessHandleOutput, T::Error> {
        invoke::<operations::ProcessStart, _>(&self.transport, &request).await
    }

    pub async fn read(
        &self,
        request: HostProcessReadRequest,
    ) -> Result<HostProcessReadOutput, T::Error> {
        invoke::<operations::ProcessRead, _>(&self.transport, &request).await
    }

    pub async fn write(
        &self,
        id: impl Into<String>,
        input: impl Into<String>,
    ) -> Result<(), T::Error> {
        invoke_ack::<operations::ProcessInput, _>(
            &self.transport,
            &HostProcessInputRequest::write(id, input),
        )
        .await
    }

    pub async fn close_stdin(&self, id: impl Into<String>) -> Result<(), T::Error> {
        invoke_ack::<operations::ProcessInput, _>(
            &self.transport,
            &HostProcessInputRequest::close(id),
        )
        .await
    }

    pub async fn status(
        &self,
        request: HostProcessTargetRequest,
    ) -> Result<HostProcessStatusOutput, T::Error> {
        invoke::<operations::ProcessStatus, _>(&self.transport, &request).await
    }

    pub async fn promote(&self, request: HostProcessTargetRequest) -> Result<(), T::Error> {
        invoke_ack::<operations::ProcessPromote, _>(&self.transport, &request).await
    }

    pub async fn kill(&self, request: HostProcessTargetRequest) -> Result<(), T::Error> {
        invoke_ack::<operations::ProcessKill, _>(&self.transport, &request).await
    }

    pub async fn list(&self) -> Result<HostProcessListOutput, T::Error> {
        invoke::<operations::ProcessList, _>(&self.transport, &EmptyRequest::default()).await
    }
}

impl<T: HostClientTransport> NetworkClient<T> {
    pub async fn send(&self, request: HostNetworkRequest) -> Result<HostNetworkResponse, T::Error> {
        invoke::<operations::NetworkClient, _>(&self.transport, &request).await
    }
}

impl<T: HostClientTransport> ExtensionHttpClient<T> {
    pub async fn dispatch_public(
        &self,
        request: ExtensionHttpDispatchRequest,
    ) -> Result<ExtensionHttpResponse, T::Error> {
        invoke::<operations::ExtensionHttpPublic, _>(&self.transport, &request).await
    }
}

async fn invoke<Op, T>(transport: &T, input: &Op::Request) -> Result<Op::Response, T::Error>
where
    T: HostClientTransport,
    Op: HostOp,
{
    let operation = Op::OPERATION;
    let input = serialize_request::<T, _>(operation, input)?;
    let output = transport.invoke(operation, input).await?;
    deserialize_response::<T, _>(operation, output)
}

async fn collect_stream<Op, T>(
    transport: &T,
    input: &Op::Request,
) -> Result<HostLlmChatOutput, T::Error>
where
    T: HostClientTransport,
    Op: HostOp,
{
    let stream = invoke_stream::<Op, T>(transport, input).await?;
    super::llm_mapping::collect_model_stream(stream)
        .await
        .map_err(T::payload_error)
}

async fn invoke_stream<Op, T>(transport: &T, input: &Op::Request) -> Result<ModelStream, T::Error>
where
    T: HostClientTransport,
    Op: HostOp,
{
    let operation = Op::OPERATION;
    let input = serialize_request::<T, _>(operation, input)?;
    transport.invoke_stream(operation, input).await
}

async fn invoke_ack<Op, T>(transport: &T, input: &Op::Request) -> Result<(), T::Error>
where
    T: HostClientTransport,
    Op: HostOp<Response = Acknowledgement>,
{
    let operation = Op::OPERATION;
    if invoke::<Op, _>(transport, input).await?.ok {
        return Ok(());
    }
    Err(T::client_error(
        WireErrorCode::InvalidResponse,
        format!(
            "invalid {} response: expected an `ok: true` acknowledgement",
            operation.wire_name()
        ),
    ))
}

fn serialize_request<T, I>(operation: HostOperation, input: &I) -> Result<Value, T::Error>
where
    T: HostClientTransport,
    I: Serialize + ?Sized,
{
    serde_json::to_value(input).map_err(|error| {
        T::client_error(
            WireErrorCode::SerializationFailed,
            format!(
                "failed to serialize {} request: {error}",
                operation.wire_name()
            ),
        )
    })
}

fn deserialize_response<T, O>(operation: HostOperation, output: Value) -> Result<O, T::Error>
where
    T: HostClientTransport,
    O: DeserializeOwned,
{
    serde_json::from_value(output).map_err(|error| {
        T::client_error(
            WireErrorCode::InvalidResponse,
            format!("invalid {} response: {error}", operation.wire_name()),
        )
    })
}
