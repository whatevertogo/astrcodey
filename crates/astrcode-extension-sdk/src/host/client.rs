use serde_json::json;

use crate::{
    extension::{ExtensionHttpRequest, ExtensionHttpResponse},
    host::{
        ExtensionHost, HostConfigureSessionToolsOutput, HostConfigureSessionToolsRequest,
        HostError, HostLlmChatOutput, HostLlmChatRequest, HostLlmCollectedStreamOutput,
        HostNetworkRequest, HostNetworkResponse, HostOperation, HostProcessOutput,
        HostProcessRequest, HostSessionCancelOutput, HostSessionDeliveryOutput,
        HostSessionExecutionView, HostSessionInputRequest, HostSessionProviderMessagesOutput,
        HostSessionSummariesOutput, HostSessionTokenUsageOutput, HostSessionTranscript,
        HostWorkspaceEditOutput, HostWorkspaceEditRequest, HostWorkspaceGlobOutput,
        HostWorkspaceGlobRequest, HostWorkspaceGrepOutput, HostWorkspaceGrepRequest,
        HostWorkspaceListOutput, HostWorkspaceListRequest, HostWorkspaceReadOutput,
        HostWorkspaceReadRequest, HostWorkspaceWriteOutput, HostWorkspaceWriteRequest,
    },
    llm::LlmMessage,
    session::{
        HostCreateSessionOutput, HostCreateSessionRequest, HostRecycleSessionRequest,
        HostRootSubmitTurnRequest, HostSessionEventsPageOutput, HostSessionEventsPageRequest,
        HostSessionReactivateOutput, HostSessionStateOutput, HostSessionTargetRequest,
        HostSubmitTurnOutput, HostSubmitTurnRequest,
    },
    session_inspect::{
        SessionHistorySnapshotOutput, SessionInspectListOutput,
        SessionInspectProviderMessagesOutput, SessionInspectReadModelOutput,
        SessionInspectSnapshotOutput,
    },
};

macro_rules! domain_client {
    ($name:ident) => {
        #[derive(Clone)]
        pub struct $name {
            host: ExtensionHost,
        }

        impl $name {
            pub(super) fn new(host: &ExtensionHost) -> Self {
                Self { host: host.clone() }
            }
        }
    };
}

domain_client!(ModelClient);
domain_client!(SessionControlClient);
domain_client!(SessionHistoryClient);
domain_client!(SessionInspectClient);
domain_client!(WorkspaceClient);
domain_client!(ProcessClient);
domain_client!(NetworkClient);
domain_client!(ExtensionHttpClient);

impl ModelClient {
    pub fn main_available(&self) -> Result<bool, HostError> {
        self.host.operation_available(HostOperation::LlmMainChat)
    }

    pub fn small_available(&self) -> Result<bool, HostError> {
        self.host.operation_available(HostOperation::LlmSmallChat)
    }

    pub async fn main_chat(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<HostLlmChatOutput, HostError> {
        self.host
            .invoke(
                HostOperation::LlmMainChat,
                &HostLlmChatRequest::new(messages),
            )
            .await
    }

    pub async fn small_chat(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<HostLlmChatOutput, HostError> {
        self.host
            .invoke(
                HostOperation::LlmSmallChat,
                &HostLlmChatRequest::new(messages),
            )
            .await
    }

    /// Runs the main model stream and returns all ordered text deltas after completion.
    pub async fn main_chat_stream(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<HostLlmCollectedStreamOutput, HostError> {
        self.host
            .invoke_collected_stream(
                HostOperation::LlmMainChat,
                &HostLlmChatRequest::new(messages),
            )
            .await
    }

    /// Runs the small model stream and returns all ordered text deltas after completion.
    pub async fn small_chat_stream(
        &self,
        messages: Vec<LlmMessage>,
    ) -> Result<HostLlmCollectedStreamOutput, HostError> {
        self.host
            .invoke_collected_stream(
                HostOperation::LlmSmallChat,
                &HostLlmChatRequest::new(messages),
            )
            .await
    }
}

impl ProcessClient {
    pub async fn spawn(&self, request: HostProcessRequest) -> Result<HostProcessOutput, HostError> {
        self.host
            .invoke(HostOperation::ProcessSpawn, &request)
            .await
    }
}

impl NetworkClient {
    pub async fn send(
        &self,
        request: HostNetworkRequest,
    ) -> Result<HostNetworkResponse, HostError> {
        self.host
            .invoke(HostOperation::NetworkClient, &request)
            .await
    }
}

impl ExtensionHttpClient {
    pub async fn dispatch_public(
        &self,
        request: ExtensionHttpRequest,
    ) -> Result<ExtensionHttpResponse, HostError> {
        self.host
            .invoke(HostOperation::ExtensionHttpPublic, &request)
            .await
    }
}

impl SessionControlClient {
    pub async fn create_root(&self) -> Result<HostCreateSessionOutput, HostError> {
        self.host
            .invoke(HostOperation::SessionRootCreate, &json!({}))
            .await
    }

    pub async fn submit_root_turn(
        &self,
        request: HostRootSubmitTurnRequest,
    ) -> Result<HostSubmitTurnOutput, HostError> {
        self.host
            .invoke(HostOperation::SessionRootSubmitTurn, &request)
            .await
    }

    pub async fn root_state(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionStateOutput, HostError> {
        self.host
            .invoke(HostOperation::SessionRootState, &request)
            .await
    }

    pub async fn inject_or_start(
        &self,
        request: HostSessionInputRequest,
    ) -> Result<HostSessionDeliveryOutput, HostError> {
        self.host
            .invoke(HostOperation::SessionControlInjectOrStart, &request)
            .await
    }

    pub async fn interrupt_and_submit(
        &self,
        request: HostSessionInputRequest,
    ) -> Result<HostSessionDeliveryOutput, HostError> {
        self.host
            .invoke(HostOperation::SessionControlInterruptAndSubmit, &request)
            .await
    }

    pub async fn cancel_turn(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionCancelOutput, HostError> {
        self.host
            .invoke(HostOperation::SessionControlCancelTurn, &request)
            .await
    }

    pub async fn execution_view(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionExecutionView, HostError> {
        self.host
            .invoke(HostOperation::SessionControlExecutionView, &request)
            .await
    }

    pub async fn state(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionStateOutput, HostError> {
        self.host
            .invoke(HostOperation::SessionControlState, &request)
            .await
    }

    pub async fn reactivate(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionReactivateOutput, HostError> {
        self.host
            .invoke(HostOperation::SessionControlReactivate, &request)
            .await
    }

    pub async fn create_child(
        &self,
        request: HostCreateSessionRequest,
    ) -> Result<HostCreateSessionOutput, HostError> {
        self.host
            .invoke(HostOperation::SessionControlCreate, &request)
            .await
    }

    pub async fn submit_turn(
        &self,
        request: HostSubmitTurnRequest,
    ) -> Result<HostSubmitTurnOutput, HostError> {
        self.host
            .invoke(HostOperation::SessionControlSubmitTurn, &request)
            .await
    }

    pub async fn configure_tools(
        &self,
        request: HostConfigureSessionToolsRequest,
    ) -> Result<HostConfigureSessionToolsOutput, HostError> {
        self.host
            .invoke(HostOperation::SessionControlConfigureTools, &request)
            .await
    }

    pub async fn recycle(&self, request: HostRecycleSessionRequest) -> Result<(), HostError> {
        self.host
            .invoke_unit(HostOperation::SessionControlDispose, &request)
            .await
    }
}

impl SessionHistoryClient {
    pub async fn list_summaries(&self) -> Result<HostSessionSummariesOutput, HostError> {
        self.host
            .invoke(HostOperation::SessionHistoryList, &json!({}))
            .await
    }

    pub async fn transcript(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionTranscript, HostError> {
        self.host
            .invoke(HostOperation::SessionHistoryTranscript, &request)
            .await
    }

    pub async fn provider_messages(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionProviderMessagesOutput, HostError> {
        self.host
            .invoke(HostOperation::SessionHistoryProviderMessages, &request)
            .await
    }

    pub async fn token_usage(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionTokenUsageOutput, HostError> {
        self.host
            .invoke(HostOperation::SessionHistoryTokenUsage, &request)
            .await
    }

    pub async fn events_page(
        &self,
        request: HostSessionEventsPageRequest,
    ) -> Result<HostSessionEventsPageOutput, HostError> {
        self.host
            .invoke(HostOperation::SessionReadEvents, &request)
            .await
    }

    pub async fn snapshot(
        &self,
        request: HostSessionTargetRequest,
    ) -> Result<SessionHistorySnapshotOutput, HostError> {
        self.host
            .invoke(HostOperation::SessionHistorySnapshot, &request)
            .await
    }
}

impl WorkspaceClient {
    pub async fn read(
        &self,
        request: HostWorkspaceReadRequest,
    ) -> Result<HostWorkspaceReadOutput, HostError> {
        self.host
            .invoke(HostOperation::WorkspaceRead, &request)
            .await
    }

    pub async fn write(
        &self,
        request: HostWorkspaceWriteRequest,
    ) -> Result<HostWorkspaceWriteOutput, HostError> {
        self.host
            .invoke(HostOperation::WorkspaceWrite, &request)
            .await
    }

    pub async fn edit(
        &self,
        request: HostWorkspaceEditRequest,
    ) -> Result<HostWorkspaceEditOutput, HostError> {
        self.host
            .invoke(HostOperation::WorkspaceEdit, &request)
            .await
    }

    pub async fn list(
        &self,
        request: HostWorkspaceListRequest,
    ) -> Result<HostWorkspaceListOutput, HostError> {
        self.host
            .invoke(HostOperation::WorkspaceList, &request)
            .await
    }

    pub async fn grep(
        &self,
        request: HostWorkspaceGrepRequest,
    ) -> Result<HostWorkspaceGrepOutput, HostError> {
        self.host
            .invoke(HostOperation::WorkspaceGrep, &request)
            .await
    }

    pub async fn glob(
        &self,
        request: HostWorkspaceGlobRequest,
    ) -> Result<HostWorkspaceGlobOutput, HostError> {
        self.host
            .invoke(HostOperation::WorkspaceGlob, &request)
            .await
    }
}

impl SessionInspectClient {
    pub async fn list(&self) -> Result<SessionInspectListOutput, HostError> {
        self.host
            .invoke(HostOperation::SessionInspectList, &json!({}))
            .await
    }

    pub async fn snapshot(
        &self,
        session_id: &str,
    ) -> Result<SessionInspectSnapshotOutput, HostError> {
        self.inspect(HostOperation::SessionInspectSnapshot, session_id)
            .await
    }

    pub async fn read_model(
        &self,
        session_id: &str,
    ) -> Result<SessionInspectReadModelOutput, HostError> {
        self.inspect(HostOperation::SessionInspectReadModel, session_id)
            .await
    }

    pub async fn provider_messages(
        &self,
        session_id: &str,
    ) -> Result<SessionInspectProviderMessagesOutput, HostError> {
        self.inspect(HostOperation::SessionInspectProviderMessages, session_id)
            .await
    }

    async fn inspect<T>(&self, operation: HostOperation, session_id: &str) -> Result<T, HostError>
    where
        T: serde::de::DeserializeOwned,
    {
        self.host
            .invoke(operation, &json!({ "session_id": session_id }))
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use serde_json::Value;

    use super::*;
    use crate::{
        extension::ExtensionHttpMethod,
        host::{HostErrorClass, internal},
        session::SessionToolSelectionDto,
    };

    struct RecordingInvoker {
        operations: Mutex<Vec<HostOperation>>,
    }

    #[async_trait]
    impl internal::HostInvoker for RecordingInvoker {
        async fn invoke(
            &self,
            operation: HostOperation,
            _input: Value,
        ) -> Result<Value, HostError> {
            self.operations.lock().unwrap().push(operation);
            Err(HostError::new(
                "backend_unavailable",
                "test backend unavailable",
            ))
        }

        async fn invoke_collected_stream(
            &self,
            operation: HostOperation,
            _input: Value,
        ) -> Result<Value, HostError> {
            self.operations.lock().unwrap().push(operation);
            Err(HostError::new(
                "backend_unavailable",
                "test backend unavailable",
            ))
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    async fn expect_error_class<T>(
        future: impl Future<Output = Result<T, HostError>>,
        expected: HostErrorClass,
    ) {
        let result = future.await;
        let Err(error) = result else {
            panic!("mock invoker unexpectedly succeeded");
        };
        assert_eq!(error.class(), expected);
    }

    async fn expect_backend_error<T>(future: impl Future<Output = Result<T, HostError>>) {
        expect_error_class(future, HostErrorClass::BackendUnavailable).await;
    }

    fn expect_access_error<T>(result: Result<T, HostError>, expected: HostErrorClass) {
        let Err(error) = result else {
            panic!("host access unexpectedly succeeded");
        };
        assert_eq!(error.class(), expected);
    }

    #[tokio::test]
    async fn domain_clients_route_every_exposed_typed_operation() {
        let expected_operations = [
            HostOperation::LlmMainChat,
            HostOperation::LlmSmallChat,
            HostOperation::LlmMainChat,
            HostOperation::LlmSmallChat,
            HostOperation::ProcessSpawn,
            HostOperation::NetworkClient,
            HostOperation::ExtensionHttpPublic,
            HostOperation::SessionRootCreate,
            HostOperation::SessionRootSubmitTurn,
            HostOperation::SessionRootState,
            HostOperation::SessionControlInjectOrStart,
            HostOperation::SessionControlInterruptAndSubmit,
            HostOperation::SessionControlCancelTurn,
            HostOperation::SessionControlExecutionView,
            HostOperation::SessionControlState,
            HostOperation::SessionControlReactivate,
            HostOperation::SessionControlCreate,
            HostOperation::SessionControlSubmitTurn,
            HostOperation::SessionControlConfigureTools,
            HostOperation::SessionControlDispose,
            HostOperation::SessionHistoryList,
            HostOperation::SessionHistoryTranscript,
            HostOperation::SessionHistoryProviderMessages,
            HostOperation::SessionHistoryTokenUsage,
            HostOperation::SessionHistorySnapshot,
            HostOperation::SessionReadEvents,
            HostOperation::WorkspaceRead,
            HostOperation::WorkspaceWrite,
            HostOperation::WorkspaceEdit,
            HostOperation::WorkspaceList,
            HostOperation::WorkspaceGrep,
            HostOperation::WorkspaceGlob,
            HostOperation::SessionInspectList,
            HostOperation::SessionInspectSnapshot,
            HostOperation::SessionInspectReadModel,
            HostOperation::SessionInspectProviderMessages,
        ];
        let invoker = Arc::new(RecordingInvoker {
            operations: Mutex::new(Vec::new()),
        });
        let scope = internal::HostScope::new(
            [
                crate::extension::ExtensionCapability::MainModel,
                crate::extension::ExtensionCapability::SmallModel,
                crate::extension::ExtensionCapability::ProcessSpawn,
                crate::extension::ExtensionCapability::NetworkClient,
                crate::extension::ExtensionCapability::PublicHttpDispatch,
                crate::extension::ExtensionCapability::InputDelivery,
                crate::extension::ExtensionCapability::SessionControl,
                crate::extension::ExtensionCapability::SessionHistory,
                crate::extension::ExtensionCapability::WorkspaceRead,
                crate::extension::ExtensionCapability::WorkspaceWrite,
                crate::extension::ExtensionCapability::SessionInspect,
            ],
            expected_operations.iter().copied(),
            true,
            true,
        );
        let host = internal::extension_host(invoker.clone(), scope);
        assert!(host.models().main_available().unwrap());
        assert!(host.models().small_available().unwrap());
        let target = || HostSessionTargetRequest {
            target_session_id: "child-1".into(),
        };
        let input = || HostSessionInputRequest {
            target_session_id: "child-1".into(),
            content: "continue".into(),
        };

        let messages = || vec![LlmMessage::user("hello")];
        expect_backend_error(host.models().main_chat(messages())).await;
        expect_backend_error(host.models().small_chat(messages())).await;
        expect_backend_error(host.models().main_chat_stream(messages())).await;
        expect_backend_error(host.models().small_chat_stream(messages())).await;
        expect_backend_error(
            host.process()
                .unwrap()
                .spawn(HostProcessRequest::new("true")),
        )
        .await;
        expect_backend_error(
            host.network()
                .unwrap()
                .send(HostNetworkRequest::get("https://example.com")),
        )
        .await;
        expect_backend_error(host.extension_http().unwrap().dispatch_public(
            ExtensionHttpRequest::new(ExtensionHttpMethod::Get, "/health"),
        ))
        .await;
        expect_backend_error(host.session_control().unwrap().create_root()).await;
        expect_backend_error(
            host.session_control()
                .unwrap()
                .submit_root_turn(HostRootSubmitTurnRequest::new("root-1", "continue")),
        )
        .await;
        expect_backend_error(host.session_control().unwrap().root_state(
            HostSessionTargetRequest {
                target_session_id: "root-1".into(),
            },
        ))
        .await;
        expect_backend_error(host.session_control().unwrap().inject_or_start(input())).await;
        expect_backend_error(
            host.session_control()
                .unwrap()
                .interrupt_and_submit(input()),
        )
        .await;
        expect_backend_error(host.session_control().unwrap().cancel_turn(target())).await;
        expect_backend_error(host.session_control().unwrap().execution_view(target())).await;
        expect_backend_error(host.session_control().unwrap().state(target())).await;
        expect_backend_error(host.session_control().unwrap().reactivate(target())).await;
        expect_backend_error(
            host.session_control()
                .unwrap()
                .create_child(HostCreateSessionRequest::new("child")),
        )
        .await;
        expect_backend_error(
            host.session_control()
                .unwrap()
                .submit_turn(HostSubmitTurnRequest::background("child-1", "review")),
        )
        .await;
        expect_backend_error(host.session_control().unwrap().configure_tools(
            HostConfigureSessionToolsRequest {
                session_id: "child-1".into(),
                selection: SessionToolSelectionDto::no_tools(),
            },
        ))
        .await;
        expect_backend_error(
            host.session_control()
                .unwrap()
                .recycle(HostRecycleSessionRequest::new("child-1")),
        )
        .await;
        expect_backend_error(host.session_history().unwrap().list_summaries()).await;
        expect_backend_error(host.session_history().unwrap().transcript(target())).await;
        expect_backend_error(host.session_history().unwrap().provider_messages(target())).await;
        expect_backend_error(host.session_history().unwrap().token_usage(target())).await;
        expect_backend_error(host.session_history().unwrap().snapshot(target())).await;
        expect_backend_error(
            host.session_history()
                .unwrap()
                .events_page(HostSessionEventsPageRequest::new("child-1")),
        )
        .await;
        expect_backend_error(host.workspace().unwrap().read(HostWorkspaceReadRequest {
            path: "notes.txt".into(),
            max_bytes: None,
        }))
        .await;
        expect_backend_error(host.workspace().unwrap().write(HostWorkspaceWriteRequest {
            path: "notes.txt".into(),
            content: "hello".into(),
        }))
        .await;
        expect_backend_error(host.workspace().unwrap().edit(HostWorkspaceEditRequest {
            path: "notes.txt".into(),
            old_text: "hello".into(),
            new_text: "hi".into(),
            replace_all: false,
        }))
        .await;
        expect_backend_error(host.workspace().unwrap().list(HostWorkspaceListRequest {
            path: ".".into(),
            depth: 1,
            limit: None,
        }))
        .await;
        expect_backend_error(host.workspace().unwrap().grep(HostWorkspaceGrepRequest {
            pattern: "hello".into(),
            path: None,
            max_matches: None,
            max_bytes: None,
            max_line_chars: None,
        }))
        .await;
        expect_backend_error(host.workspace().unwrap().glob(HostWorkspaceGlobRequest {
            pattern: "**/*.rs".into(),
            root: None,
            max_matches: None,
            include_ignored: false,
        }))
        .await;
        expect_backend_error(host.session_inspect().unwrap().list()).await;
        expect_backend_error(host.session_inspect().unwrap().snapshot("session-1")).await;
        expect_backend_error(host.session_inspect().unwrap().read_model("session-1")).await;
        expect_backend_error(
            host.session_inspect()
                .unwrap()
                .provider_messages("session-1"),
        )
        .await;

        assert_eq!(*invoker.operations.lock().unwrap(), expected_operations);
    }

    #[tokio::test]
    async fn scoped_preflight_distinguishes_permission_backend_and_context_failures() {
        let invoker = Arc::new(RecordingInvoker {
            operations: Mutex::new(Vec::new()),
        });

        let host = internal::extension_host(
            invoker.clone(),
            internal::HostScope::new(
                [],
                [HostOperation::LlmMainChat, HostOperation::NetworkClient],
                true,
                true,
            ),
        );
        expect_access_error(host.network(), HostErrorClass::PermissionDenied);
        expect_error_class(
            host.models().main_chat(vec![LlmMessage::user("hello")]),
            HostErrorClass::PermissionDenied,
        )
        .await;

        let host = internal::extension_host(
            invoker.clone(),
            internal::HostScope::new(
                [crate::extension::ExtensionCapability::NetworkClient],
                [],
                true,
                true,
            ),
        );
        expect_access_error(host.network(), HostErrorClass::BackendUnavailable);

        let host = internal::extension_host(
            invoker.clone(),
            internal::HostScope::new(
                [
                    crate::extension::ExtensionCapability::SessionControl,
                    crate::extension::ExtensionCapability::SessionHistory,
                    crate::extension::ExtensionCapability::WorkspaceWrite,
                    crate::extension::ExtensionCapability::ProcessSpawn,
                ],
                [
                    HostOperation::SessionControlState,
                    HostOperation::SessionHistorySnapshot,
                    HostOperation::WorkspaceWrite,
                ],
                false,
                false,
            ),
        );
        expect_access_error(host.session_control(), HostErrorClass::ContextUnavailable);
        expect_access_error(host.session_history(), HostErrorClass::ContextUnavailable);
        expect_access_error(host.workspace(), HostErrorClass::ContextUnavailable);
        expect_access_error(host.process(), HostErrorClass::ContextUnavailable);

        assert!(invoker.operations.lock().unwrap().is_empty());

        let host = internal::extension_host(
            invoker.clone(),
            internal::HostScope::new(
                [crate::extension::ExtensionCapability::InputDelivery],
                [HostOperation::SessionRootCreate],
                false,
                true,
            ),
        );
        let session_control = host
            .session_control()
            .unwrap_or_else(|_| panic!("root session domain should be start-scoped"));
        expect_backend_error(session_control.create_root()).await;
        assert_eq!(
            *invoker.operations.lock().unwrap(),
            [HostOperation::SessionRootCreate]
        );
    }
}
