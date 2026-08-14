#[cfg(test)]
use serde_json::json;

use super::domain_client::{
    ExtensionHttpClient as TypedExtensionHttpClient, ModelClient as TypedModelClient,
    NetworkClient as TypedNetworkClient, ProcessClient as TypedProcessClient,
    SessionControlClient as TypedSessionControlClient,
    SessionHistoryClient as TypedSessionHistoryClient,
    SessionInspectClient as TypedSessionInspectClient,
    SessionStateClient as TypedSessionStateClient, ToolResultClient as TypedToolResultClient,
    WorkspaceClient as TypedWorkspaceClient,
};
use crate::host::{ExtensionHost, HostError, HostOperation};
#[cfg(test)]
use crate::{
    extension::ExtensionHttpDispatchRequest,
    host::{
        HostConfigureSessionToolsRequest, HostNetworkRequest, HostProcessReadRequest,
        HostProcessRequest, HostProcessResizeRequest, HostProcessStartRequest,
        HostProcessTargetRequest, HostSessionInputRequest, HostSessionStateReadRequest,
        HostSessionStateWriteRequest, HostToolResultReadRequest, HostWorkspaceApplyPatchRequest,
        HostWorkspaceEditRequest, HostWorkspaceGlobRequest, HostWorkspaceGrepMode,
        HostWorkspaceGrepRequest, HostWorkspaceListRequest, HostWorkspaceReadRequest,
        HostWorkspaceWriteRequest,
    },
    llm::LlmMessage,
    session::{
        HostCreateSessionRequest, HostRecycleSessionRequest, HostRootSubmitTurnRequest,
        HostSessionEventsPageRequest, HostSessionTargetRequest, HostSubmitTurnRequest,
    },
};

pub type ModelClient = TypedModelClient<ExtensionHost>;
pub type SessionControlClient = TypedSessionControlClient<ExtensionHost>;
pub type SessionHistoryClient = TypedSessionHistoryClient<ExtensionHost>;
pub type SessionStateClient = TypedSessionStateClient<ExtensionHost>;
pub type SessionInspectClient = TypedSessionInspectClient<ExtensionHost>;
pub type ToolResultClient = TypedToolResultClient<ExtensionHost>;
pub type WorkspaceClient = TypedWorkspaceClient<ExtensionHost>;
pub type ProcessClient = TypedProcessClient<ExtensionHost>;
pub type NetworkClient = TypedNetworkClient<ExtensionHost>;
pub type ExtensionHttpClient = TypedExtensionHttpClient<ExtensionHost>;

impl TypedModelClient<ExtensionHost> {
    pub fn main_available(&self) -> Result<bool, HostError> {
        self.transport()
            .operation_available(HostOperation::LlmMainChat)
    }

    pub fn small_available(&self) -> Result<bool, HostError> {
        self.transport()
            .operation_available(HostOperation::LlmSmallChat)
    }
}
#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        future::Future,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use serde_json::Value;

    use super::*;
    use crate::{
        extension::ExtensionHttpMethod, host::internal, session::SessionToolSelectionDto,
        wire::WireErrorCode,
    };

    struct RecordingInvoker {
        operations: Mutex<Vec<HostOperation>>,
    }

    struct StaticResponseInvoker(Value);

    #[async_trait]
    impl internal::HostInvoker for RecordingInvoker {
        async fn invoke(
            &self,
            operation: HostOperation,
            _input: Value,
        ) -> Result<Value, HostError> {
            self.operations.lock().unwrap().push(operation);
            Err(HostError::new(
                WireErrorCode::BackendUnavailable,
                "test backend unavailable",
            ))
        }

        async fn invoke_stream(
            &self,
            operation: HostOperation,
            _input: Value,
        ) -> Result<crate::model_stream::ModelStream, HostError> {
            self.operations.lock().unwrap().push(operation);
            Err(HostError::new(
                WireErrorCode::BackendUnavailable,
                "test backend unavailable",
            ))
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[async_trait]
    impl internal::HostInvoker for StaticResponseInvoker {
        async fn invoke(
            &self,
            _operation: HostOperation,
            _input: Value,
        ) -> Result<Value, HostError> {
            Ok(self.0.clone())
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    async fn expect_error_code<T>(
        future: impl Future<Output = Result<T, HostError>>,
        expected: WireErrorCode,
    ) {
        let result = future.await;
        let Err(error) = result else {
            panic!("mock invoker unexpectedly succeeded");
        };
        assert_eq!(error.code_enum(), Some(expected));
    }

    async fn expect_backend_error<T>(future: impl Future<Output = Result<T, HostError>>) {
        expect_error_code(future, WireErrorCode::BackendUnavailable).await;
    }

    fn expect_access_error<T>(result: Result<T, HostError>, expected: WireErrorCode) {
        let Err(error) = result else {
            panic!("host access unexpectedly succeeded");
        };
        assert_eq!(error.code_enum(), Some(expected));
    }

    #[tokio::test]
    async fn domain_clients_route_every_exposed_typed_operation() {
        let expected_operations = [
            HostOperation::LlmMainChat,
            HostOperation::LlmSmallChat,
            HostOperation::LlmMainChat,
            HostOperation::LlmSmallChat,
            HostOperation::ProcessSpawn,
            HostOperation::ProcessStart,
            HostOperation::ProcessRead,
            HostOperation::ProcessInput,
            HostOperation::ProcessInput,
            HostOperation::ProcessResize,
            HostOperation::ProcessStatus,
            HostOperation::ProcessPromote,
            HostOperation::ProcessKill,
            HostOperation::ProcessList,
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
            HostOperation::SessionStateRead,
            HostOperation::SessionStateWrite,
            HostOperation::WorkspaceApplyPatch,
            HostOperation::ToolResultRead,
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
        let covered = expected_operations.iter().copied().collect::<HashSet<_>>();
        let expected = crate::host::internal::HOST_OPERATION_SPECS
            .iter()
            .map(|spec| spec.operation)
            .filter(|operation| *operation != HostOperation::EventEmit)
            .collect::<HashSet<_>>();
        assert_eq!(covered, expected, "in-process client operation coverage");
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
                crate::extension::ExtensionCapability::ToolResultRead,
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
        expect_backend_error(host.models().main_chat_collected(messages())).await;
        expect_backend_error(host.models().small_chat_collected(messages())).await;
        expect_backend_error(
            host.process()
                .unwrap()
                .spawn(HostProcessRequest::new("true")),
        )
        .await;
        expect_backend_error(
            host.process()
                .unwrap()
                .start(HostProcessStartRequest::pipes("true")),
        )
        .await;
        let process_target = || HostProcessTargetRequest {
            id: "process-1".into(),
        };
        expect_backend_error(host.process().unwrap().read(HostProcessReadRequest {
            id: "process-1".into(),
            wait_ms: None,
        }))
        .await;
        expect_backend_error(host.process().unwrap().write("process-1", "continue\n")).await;
        expect_backend_error(host.process().unwrap().close_stdin("process-1")).await;
        expect_backend_error(host.process().unwrap().resize(HostProcessResizeRequest {
            id: "process-1".into(),
            rows: 24,
            cols: 80,
        }))
        .await;
        expect_backend_error(host.process().unwrap().status(process_target())).await;
        expect_backend_error(host.process().unwrap().promote(process_target())).await;
        expect_backend_error(host.process().unwrap().kill(process_target())).await;
        expect_backend_error(host.process().unwrap().list()).await;
        expect_backend_error(
            host.network()
                .unwrap()
                .send(HostNetworkRequest::get("https://example.com")),
        )
        .await;
        expect_backend_error(host.extension_http().unwrap().dispatch_public(
            ExtensionHttpDispatchRequest::new(ExtensionHttpMethod::Get, "/health"),
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
        expect_backend_error(
            host.session_state()
                .unwrap()
                .read(HostSessionStateReadRequest { key: "goal".into() }),
        )
        .await;
        expect_backend_error(
            host.session_state()
                .unwrap()
                .write(HostSessionStateWriteRequest {
                    key: "goal".into(),
                    content: "active".into(),
                }),
        )
        .await;
        expect_backend_error(host.workspace().unwrap().apply_patch(
            HostWorkspaceApplyPatchRequest {
                patch: "*** Begin Patch\n*** End Patch".into(),
            },
        ))
        .await;
        expect_backend_error(
            host.tool_results()
                .unwrap()
                .read(HostToolResultReadRequest {
                    artifact_id: "result.txt".into(),
                    byte_offset: 0,
                    max_bytes: 1_024,
                }),
        )
        .await;
        expect_backend_error(host.workspace().unwrap().read(HostWorkspaceReadRequest {
            path: "notes.txt".into(),
            max_bytes: None,
            line_offset: 0,
            line_limit: None,
        }))
        .await;
        expect_backend_error(host.workspace().unwrap().write(HostWorkspaceWriteRequest {
            path: "notes.txt".into(),
            content: "hello".into(),
            create_dirs: false,
        }))
        .await;
        expect_backend_error(host.workspace().unwrap().edit(HostWorkspaceEditRequest {
            path: "notes.txt".into(),
            old_text: Some("hello".into()),
            new_text: Some("hi".into()),
            replace_all: false,
            edits: Vec::new(),
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
            offset: 0,
            max_matches: None,
            max_bytes: None,
            max_line_chars: None,
            recursive: true,
            multiline: false,
            path_filters: Vec::new(),
            before_context: 0,
            after_context: 0,
            mode: HostWorkspaceGrepMode::FilesWithMatches,
        }))
        .await;
        expect_backend_error(host.workspace().unwrap().glob(HostWorkspaceGlobRequest {
            pattern: "**/*.rs".into(),
            root: None,
            offset: 0,
            max_matches: None,
            respect_gitignore: true,
            include_hidden: true,
            include_directories: true,
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
    async fn unit_responses_require_a_strict_success_acknowledgement() {
        let request = || HostSessionStateWriteRequest {
            key: "goal".into(),
            content: "active".into(),
        };

        for (response, expected_code) in [
            (json!({ "ok": true }), None),
            (json!({ "ok": false }), Some("invalid_host_response")),
            (
                json!({ "ok": true, "extra": true }),
                Some("invalid_host_response"),
            ),
        ] {
            let host = internal::extension_host(
                Arc::new(StaticResponseInvoker(response)),
                internal::HostScope::new([], [HostOperation::SessionStateWrite], true, false),
            );
            let result = host.session_state().unwrap().write(request()).await;
            match expected_code {
                Some(code) => assert_eq!(result.unwrap_err().code, code),
                None => result.unwrap(),
            }
        }
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
        expect_access_error(host.network(), WireErrorCode::PermissionDenied);
        expect_error_code(
            host.models().main_chat(vec![LlmMessage::user("hello")]),
            WireErrorCode::PermissionDenied,
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
        expect_access_error(host.network(), WireErrorCode::BackendUnavailable);

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
        expect_access_error(host.session_control(), WireErrorCode::ContextUnavailable);
        expect_access_error(host.session_history(), WireErrorCode::ContextUnavailable);
        expect_access_error(host.session_state(), WireErrorCode::ContextUnavailable);
        expect_access_error(host.workspace(), WireErrorCode::ContextUnavailable);
        expect_access_error(host.process(), WireErrorCode::ContextUnavailable);

        let host = internal::extension_host(
            invoker.clone(),
            internal::HostScope::new(
                [crate::extension::ExtensionCapability::SessionControl],
                [],
                false,
                true,
            ),
        );
        expect_access_error(host.session_control(), WireErrorCode::ContextUnavailable);

        assert!(invoker.operations.lock().unwrap().is_empty());

        let host = internal::extension_host(
            invoker.clone(),
            internal::HostScope::new(
                [crate::extension::ExtensionCapability::InputDelivery],
                [HostOperation::SessionRootCreate],
                false,
                false,
            ),
        );
        expect_access_error(host.session_control(), WireErrorCode::ContextUnavailable);
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
