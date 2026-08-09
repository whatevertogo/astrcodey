use astrcode_core::wire::WireErrorCode;
use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{
    extension::{ExtensionHttpDispatchRequest, ExtensionHttpResponse},
    host::{
        HostConfigureSessionToolsOutput, HostConfigureSessionToolsRequest, HostEventEmitOutput,
        HostLlmChatOutput, HostLlmChatRequest, HostLlmCollectedStreamOutput, HostNetworkRequest,
        HostNetworkResponse, HostOperation, HostProcessOutput, HostProcessRequest,
        HostSessionCancelOutput, HostSessionDeliveryOutput, HostSessionExecutionView,
        HostSessionInputRequest, HostSessionProviderMessagesOutput, HostSessionStateReadOutput,
        HostSessionStateReadRequest, HostSessionStateWriteRequest, HostSessionSummariesOutput,
        HostSessionTokenUsageOutput, HostSessionTranscript, HostWorkspaceEditOutput,
        HostWorkspaceEditRequest, HostWorkspaceGlobOutput, HostWorkspaceGlobRequest,
        HostWorkspaceGrepOutput, HostWorkspaceGrepRequest, HostWorkspaceListOutput,
        HostWorkspaceListRequest, HostWorkspaceReadOutput, HostWorkspaceReadRequest,
        HostWorkspaceWriteOutput, HostWorkspaceWriteRequest,
    },
    llm::LlmMessage,
    session::{
        HostCreateSessionOutput, HostCreateSessionRequest, HostRecycleSessionRequest,
        HostRootSubmitTurnRequest, HostSessionEventsPageOutput, HostSessionEventsPageRequest,
        HostSessionReactivateOutput, HostSessionStateOutput, HostSessionTargetRequest,
        HostSubmitTurnOutput, HostSubmitTurnRequest,
    },
    session_inspect::{
        HostSessionInspectRequest, SessionHistorySnapshotOutput, SessionInspectListOutput,
        SessionInspectProviderMessagesOutput, SessionInspectReadModelOutput,
        SessionInspectSnapshotOutput,
    },
};

#[async_trait]
pub trait HostClientTransport: Clone + Send + Sync {
    type Error;

    async fn invoke(&self, operation: HostOperation, input: Value) -> Result<Value, Self::Error>;

    async fn invoke_collected_stream(
        &self,
        operation: HostOperation,
        input: Value,
    ) -> Result<Value, Self::Error>;

    fn client_error(code: WireErrorCode, message: String) -> Self::Error;
}

macro_rules! domain_client {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy)]
        pub struct $name<T> {
            transport: T,
        }

        impl<T> $name<T> {
            pub(crate) const fn new(transport: T) -> Self {
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
        invoke(&self.transport, HostOperation::EventEmit, &request).await
    }
}

impl<T: HostClientTransport> ModelClient<T> {
    pub async fn main_chat(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<HostLlmChatOutput, T::Error> {
        invoke(
            &self.transport,
            HostOperation::LlmMainChat,
            &HostLlmChatRequest::new(messages),
        )
        .await
    }

    pub async fn small_chat(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<HostLlmChatOutput, T::Error> {
        invoke(
            &self.transport,
            HostOperation::LlmSmallChat,
            &HostLlmChatRequest::new(messages),
        )
        .await
    }

    /// Runs the main model stream and returns all ordered text deltas after completion.
    pub async fn main_chat_stream(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<HostLlmCollectedStreamOutput, T::Error> {
        invoke_collected_stream(
            &self.transport,
            HostOperation::LlmMainChat,
            &HostLlmChatRequest::new(messages),
        )
        .await
    }

    /// Runs the small model stream and returns all ordered text deltas after completion.
    pub async fn small_chat_stream(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<HostLlmCollectedStreamOutput, T::Error> {
        invoke_collected_stream(
            &self.transport,
            HostOperation::LlmSmallChat,
            &HostLlmChatRequest::new(messages),
        )
        .await
    }
}

impl<T: HostClientTransport> SessionControlClient<T> {
    pub async fn create_root(&self) -> Result<HostCreateSessionOutput, T::Error> {
        invoke(
            &self.transport,
            HostOperation::SessionRootCreate,
            &json!({}),
        )
        .await
    }

    pub async fn submit_root_turn(
        &self,
        request: HostRootSubmitTurnRequest,
    ) -> Result<HostSubmitTurnOutput, T::Error> {
        invoke(
            &self.transport,
            HostOperation::SessionRootSubmitTurn,
            &request,
        )
        .await
    }

    pub async fn root_state(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionStateOutput, T::Error> {
        invoke(&self.transport, HostOperation::SessionRootState, &request).await
    }

    pub async fn inject_or_start(
        &self,
        request: HostSessionInputRequest,
    ) -> Result<HostSessionDeliveryOutput, T::Error> {
        invoke(
            &self.transport,
            HostOperation::SessionControlInjectOrStart,
            &request,
        )
        .await
    }

    pub async fn interrupt_and_submit(
        &self,
        request: HostSessionInputRequest,
    ) -> Result<HostSessionDeliveryOutput, T::Error> {
        invoke(
            &self.transport,
            HostOperation::SessionControlInterruptAndSubmit,
            &request,
        )
        .await
    }

    pub async fn cancel_turn(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionCancelOutput, T::Error> {
        invoke(
            &self.transport,
            HostOperation::SessionControlCancelTurn,
            &request,
        )
        .await
    }

    pub async fn execution_view(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionExecutionView, T::Error> {
        invoke(
            &self.transport,
            HostOperation::SessionControlExecutionView,
            &request,
        )
        .await
    }

    pub async fn state(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionStateOutput, T::Error> {
        invoke(
            &self.transport,
            HostOperation::SessionControlState,
            &request,
        )
        .await
    }

    pub async fn reactivate(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionReactivateOutput, T::Error> {
        invoke(
            &self.transport,
            HostOperation::SessionControlReactivate,
            &request,
        )
        .await
    }

    pub async fn create_child(
        &self,
        request: HostCreateSessionRequest,
    ) -> Result<HostCreateSessionOutput, T::Error> {
        invoke(
            &self.transport,
            HostOperation::SessionControlCreate,
            &request,
        )
        .await
    }

    pub async fn submit_turn(
        &self,
        request: HostSubmitTurnRequest,
    ) -> Result<HostSubmitTurnOutput, T::Error> {
        invoke(
            &self.transport,
            HostOperation::SessionControlSubmitTurn,
            &request,
        )
        .await
    }

    pub async fn configure_tools(
        &self,
        request: HostConfigureSessionToolsRequest,
    ) -> Result<HostConfigureSessionToolsOutput, T::Error> {
        invoke(
            &self.transport,
            HostOperation::SessionControlConfigureTools,
            &request,
        )
        .await
    }

    pub async fn recycle(&self, request: HostRecycleSessionRequest) -> Result<(), T::Error> {
        invoke_unit(
            &self.transport,
            HostOperation::SessionControlDispose,
            &request,
        )
        .await
    }
}

impl<T: HostClientTransport> SessionHistoryClient<T> {
    pub async fn list_summaries(&self) -> Result<HostSessionSummariesOutput, T::Error> {
        invoke(
            &self.transport,
            HostOperation::SessionHistoryList,
            &json!({}),
        )
        .await
    }

    pub async fn transcript(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionTranscript, T::Error> {
        invoke(
            &self.transport,
            HostOperation::SessionHistoryTranscript,
            &request,
        )
        .await
    }

    pub async fn provider_messages(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionProviderMessagesOutput, T::Error> {
        invoke(
            &self.transport,
            HostOperation::SessionHistoryProviderMessages,
            &request,
        )
        .await
    }

    pub async fn token_usage(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionTokenUsageOutput, T::Error> {
        invoke(
            &self.transport,
            HostOperation::SessionHistoryTokenUsage,
            &request,
        )
        .await
    }

    pub async fn events_page(
        &self,
        request: HostSessionEventsPageRequest,
    ) -> Result<HostSessionEventsPageOutput, T::Error> {
        invoke(&self.transport, HostOperation::SessionReadEvents, &request).await
    }

    pub async fn snapshot(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<SessionHistorySnapshotOutput, T::Error> {
        invoke(
            &self.transport,
            HostOperation::SessionHistorySnapshot,
            &request,
        )
        .await
    }
}

impl<T: HostClientTransport> SessionStateClient<T> {
    pub async fn read(
        &self,
        request: HostSessionStateReadRequest,
    ) -> Result<HostSessionStateReadOutput, T::Error> {
        invoke(&self.transport, HostOperation::SessionStateRead, &request).await
    }

    pub async fn write(&self, request: HostSessionStateWriteRequest) -> Result<(), T::Error> {
        invoke_unit(&self.transport, HostOperation::SessionStateWrite, &request).await
    }
}

impl<T: HostClientTransport> SessionInspectClient<T> {
    pub async fn list(&self) -> Result<SessionInspectListOutput, T::Error> {
        invoke(
            &self.transport,
            HostOperation::SessionInspectList,
            &json!({}),
        )
        .await
    }

    pub async fn snapshot(
        &self,
        session_id: &str,
    ) -> Result<SessionInspectSnapshotOutput, T::Error> {
        self.inspect(HostOperation::SessionInspectSnapshot, session_id)
            .await
    }

    pub async fn read_model(
        &self,
        session_id: &str,
    ) -> Result<SessionInspectReadModelOutput, T::Error> {
        self.inspect(HostOperation::SessionInspectReadModel, session_id)
            .await
    }

    pub async fn provider_messages(
        &self,
        session_id: &str,
    ) -> Result<SessionInspectProviderMessagesOutput, T::Error> {
        self.inspect(HostOperation::SessionInspectProviderMessages, session_id)
            .await
    }

    async fn inspect<O>(&self, operation: HostOperation, session_id: &str) -> Result<O, T::Error>
    where
        O: DeserializeOwned,
    {
        invoke(
            &self.transport,
            operation,
            &HostSessionInspectRequest {
                session_id: session_id.into(),
            },
        )
        .await
    }
}

impl<T: HostClientTransport> WorkspaceClient<T> {
    pub async fn read(
        &self,
        request: HostWorkspaceReadRequest,
    ) -> Result<HostWorkspaceReadOutput, T::Error> {
        invoke(&self.transport, HostOperation::WorkspaceRead, &request).await
    }

    pub async fn write(
        &self,
        request: HostWorkspaceWriteRequest,
    ) -> Result<HostWorkspaceWriteOutput, T::Error> {
        invoke(&self.transport, HostOperation::WorkspaceWrite, &request).await
    }

    pub async fn edit(
        &self,
        request: HostWorkspaceEditRequest,
    ) -> Result<HostWorkspaceEditOutput, T::Error> {
        invoke(&self.transport, HostOperation::WorkspaceEdit, &request).await
    }

    pub async fn list(
        &self,
        request: HostWorkspaceListRequest,
    ) -> Result<HostWorkspaceListOutput, T::Error> {
        invoke(&self.transport, HostOperation::WorkspaceList, &request).await
    }

    pub async fn grep(
        &self,
        request: HostWorkspaceGrepRequest,
    ) -> Result<HostWorkspaceGrepOutput, T::Error> {
        invoke(&self.transport, HostOperation::WorkspaceGrep, &request).await
    }

    pub async fn glob(
        &self,
        request: HostWorkspaceGlobRequest,
    ) -> Result<HostWorkspaceGlobOutput, T::Error> {
        invoke(&self.transport, HostOperation::WorkspaceGlob, &request).await
    }
}

impl<T: HostClientTransport> ProcessClient<T> {
    pub async fn spawn(&self, request: HostProcessRequest) -> Result<HostProcessOutput, T::Error> {
        invoke(&self.transport, HostOperation::ProcessSpawn, &request).await
    }
}

impl<T: HostClientTransport> NetworkClient<T> {
    pub async fn send(&self, request: HostNetworkRequest) -> Result<HostNetworkResponse, T::Error> {
        invoke(&self.transport, HostOperation::NetworkClient, &request).await
    }
}

impl<T: HostClientTransport> ExtensionHttpClient<T> {
    pub async fn dispatch_public(
        &self,
        request: ExtensionHttpDispatchRequest,
    ) -> Result<ExtensionHttpResponse, T::Error> {
        invoke(
            &self.transport,
            HostOperation::ExtensionHttpPublic,
            &request,
        )
        .await
    }
}

async fn invoke<T, I, O>(transport: &T, operation: HostOperation, input: &I) -> Result<O, T::Error>
where
    T: HostClientTransport,
    I: Serialize + ?Sized,
    O: DeserializeOwned,
{
    let input = serialize_request::<T, _>(operation, input)?;
    let output = transport.invoke(operation, input).await?;
    deserialize_response::<T, _>(operation, output)
}

async fn invoke_collected_stream<T, I, O>(
    transport: &T,
    operation: HostOperation,
    input: &I,
) -> Result<O, T::Error>
where
    T: HostClientTransport,
    I: Serialize + ?Sized,
    O: DeserializeOwned,
{
    let input = serialize_request::<T, _>(operation, input)?;
    let output = transport.invoke_collected_stream(operation, input).await?;
    deserialize_response::<T, _>(operation, output)
}

async fn invoke_unit<T, I>(
    transport: &T,
    operation: HostOperation,
    input: &I,
) -> Result<(), T::Error>
where
    T: HostClientTransport,
    I: Serialize + ?Sized,
{
    let output: Value = invoke(transport, operation, input).await?;
    if output == json!({ "ok": true }) {
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
