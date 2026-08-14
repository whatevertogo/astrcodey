//! Worker 运行时：扩展子进程入口。

mod builder;
mod host;
mod registry;

use std::{collections::BTreeSet, io, sync::Arc};

pub use astrcode_extension_sdk::wire::InvocationCancellation as CancelToken;
use astrcode_extension_sdk::wire::ProcessStdioTransport;
pub use builder::{
    command_handler, continuation_handler, continuation_handler_args, custom_event_handler,
    custom_event_handler_args, hook_handler, hook_handler_args, http_handler, parse_hook_input,
    parse_tool_arguments, tool_handler, tool_handler_args, tool_planner, tool_planner_args,
};
pub use host::{
    EventClient, ExtensionHttpClient, HostClient, HostConfigureSessionToolsOutput,
    HostConfigureSessionToolsRequest, HostCreateSessionOutput, HostCreateSessionRequest,
    HostEventEmitOutput, HostEventEmitRequest, HostLlmChatOutput, HostLlmCollectedStreamOutput,
    HostLlmContent, HostLlmMessage, HostLlmRole, HostNetworkRedirectPolicy, HostNetworkRequest,
    HostNetworkResponse, HostOperation, HostProcessHandleOutput, HostProcessInputAction,
    HostProcessInputRequest, HostProcessIo, HostProcessListOutput, HostProcessOutput,
    HostProcessReadOutput, HostProcessReadRequest, HostProcessRequest, HostProcessResizeRequest,
    HostProcessStartRequest, HostProcessState, HostProcessStatusOutput, HostProcessTargetRequest,
    HostRecycleSessionRequest, HostRootSubmitTurnRequest, HostSessionCancelOutput,
    HostSessionDeliveryOutput, HostSessionEvent, HostSessionEventsPageOutput,
    HostSessionEventsPageRequest, HostSessionExecutionView, HostSessionInputRequest,
    HostSessionProviderMessagesOutput, HostSessionReactivateOutput, HostSessionStateOutput,
    HostSessionStateReadOutput, HostSessionStateReadRequest, HostSessionStateWriteRequest,
    HostSessionSummariesOutput, HostSessionSummary, HostSessionTargetRequest,
    HostSessionTokenUsage, HostSessionTokenUsageOutput, HostSessionTranscript,
    HostSessionTranscriptMessage, HostSubmitTurnOutput, HostSubmitTurnRequest,
    HostToolResultReadOutput, HostToolResultReadRequest, HostWorkspaceApplyPatchOutput,
    HostWorkspaceApplyPatchRequest, HostWorkspaceEditOutput, HostWorkspaceEditRequest,
    HostWorkspaceGlobOutput, HostWorkspaceGlobRequest, HostWorkspaceGrepContextLine,
    HostWorkspaceGrepEntry, HostWorkspaceGrepMode, HostWorkspaceGrepOutput,
    HostWorkspaceGrepRequest, HostWorkspaceListEntry, HostWorkspaceListOutput,
    HostWorkspaceListRequest, HostWorkspaceReadOutput, HostWorkspaceReadRequest,
    HostWorkspaceTextChange, HostWorkspaceWriteOutput, HostWorkspaceWriteRequest, ModelClient,
    NetworkClient, ProcessClient, SessionControlClient, SessionHistoryClient, SessionInspectClient,
    SessionStateClient, ToolResultClient, WorkspaceClient,
};
pub use registry::{
    CommandHandlerFn, ContinuationHandlerFn, CustomEventHandlerFn, HookHandlerFn, HttpHandlerFn,
    ToolHandlerFn, ToolPlannerFn, WorkerCallContext, WorkerCommandContext,
    WorkerCustomEventContext, WorkerInvocationContext, WorkerToolPlanContext,
};
use serde_json::json;

use crate::WireErrorCode;

/// Explicit transport seam for worker integration tests. Not part of the author prelude.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    pub use super::host::{HostApi, invoke_host, with_host_api};
}

use crate::{
    extension::{
        CompactEvent, ContinueAfterStopOptions, CustomEventSubscription, ExtensionCapability,
        HookMode, LifecycleEvent,
    },
    s5r::{HandlerEffect, HandlerResult},
    tool::ToolDefinition,
    worker::{
        host::{V3PeerHostApi, with_host_api},
        registry::HandlerRegistry,
    },
};

pub struct Worker {
    version: String,
    registry: HandlerRegistry,
}

impl Worker {
    pub fn new(extension_id: impl Into<String>, version: impl Into<String>) -> Self {
        let extension_id = extension_id.into();
        Self {
            version: version.into(),
            registry: HandlerRegistry::new(extension_id),
        }
    }

    /// 声明 manifest 能力。
    pub fn capability(&mut self, cap: ExtensionCapability) -> &mut Self {
        self.registry.declare_capability(cap);
        self
    }

    /// 声明可发射的 custom event schema。
    pub fn custom_event(&mut self, event: crate::extension::CustomEventDeclaration) -> &mut Self {
        self.registry.declare_custom_event(event);
        self
    }

    /// Registers a custom-event subscription and its sequential handler.
    pub fn on_custom_event(
        &mut self,
        subscription: CustomEventSubscription,
        handler: CustomEventHandlerFn,
    ) -> Result<&mut Self, ErrorPayload> {
        self.registry.register_custom_event(subscription, handler)?;
        Ok(self)
    }

    /// 注册 tool：manifest 定义与 handler 一次完成，避免两处手动对齐。
    pub fn tool(
        &mut self,
        def: impl Into<ToolDefinition>,
        planner: ToolPlannerFn,
        handler: ToolHandlerFn,
    ) -> Result<&mut Self, ErrorPayload> {
        self.registry.register_tool(def.into(), planner, handler)?;
        Ok(self)
    }

    /// 注册模式可选的宿主事件 hook。
    ///
    /// 固定模式 hook 使用对应的 `on_*` 方法，避免声明一个运行时不会执行的模式。
    pub fn hook(
        &mut self,
        on: LifecycleEvent,
        mode: HookMode,
        handler: HookHandlerFn,
    ) -> Result<&mut Self, ErrorPayload> {
        self.registry.register_hook(on, mode, handler)?;
        Ok(self)
    }

    /// 注册 provider response observer。该 hook 固定为 advisory。
    pub fn on_after_provider_response(
        &mut self,
        handler: HookHandlerFn,
    ) -> Result<&mut Self, ErrorPayload> {
        self.registry
            .register_fixed_hook(LifecycleEvent::AfterProviderResponse, handler)?;
        Ok(self)
    }

    /// 注册同步收集 prompt contributions 的 hook。
    pub fn on_prompt_build(&mut self, handler: HookHandlerFn) -> Result<&mut Self, ErrorPayload> {
        self.registry
            .register_fixed_hook(LifecycleEvent::PromptBuild, handler)?;
        Ok(self)
    }

    /// 注册 pre-compact 决策与 contributions hook。
    pub fn on_pre_compact(&mut self, handler: HookHandlerFn) -> Result<&mut Self, ErrorPayload> {
        self.registry
            .register_compact_hook(CompactEvent::PreCompact, handler)?;
        Ok(self)
    }

    /// 注册 post-compact 同步通知与 contributions hook。
    pub fn on_post_compact(&mut self, handler: HookHandlerFn) -> Result<&mut Self, ErrorPayload> {
        self.registry
            .register_compact_hook(CompactEvent::PostCompact, handler)?;
        Ok(self)
    }

    /// 注册仅由 [`CallContinuation::Hook`](crate::s5r::CallContinuation::Hook) 调用的 handler。
    ///
    /// 该 handler 不会声明为宿主事件订阅；需要订阅宿主事件时使用 [`Self::hook`]。
    pub fn continuation_hook_handler(
        &mut self,
        on: impl Into<String>,
        handler: ContinuationHandlerFn,
    ) -> Result<&mut Self, ErrorPayload> {
        self.registry
            .register_continuation_hook_handler(on, handler)?;
        Ok(self)
    }

    /// 注册 [`continue_after_stop`](crate::extension::ContinueAfterStopHandler) hook。
    ///
    /// `options.max_per_turn` 在线缆 manifest 中表示为一个数字：`-1` 为无限，
    /// 非负数为每 turn 上限。
    pub fn on_continue_after_stop(
        &mut self,
        options: ContinueAfterStopOptions,
        handler: HookHandlerFn,
    ) -> Result<&mut Self, ErrorPayload> {
        self.registry
            .register_continue_after_stop_hook(options, handler)?;
        Ok(self)
    }

    /// 注册 slash command。
    pub fn command(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        handler: CommandHandlerFn,
    ) -> Result<&mut Self, ErrorPayload> {
        self.registry.register_command(name, description, handler)?;
        Ok(self)
    }

    /// 注册 HTTP 路由；manifest 声明与 handler 保持同一数据源。
    pub fn http_route(
        &mut self,
        route: crate::extension::ExtensionHttpRoute,
        handler: HttpHandlerFn,
    ) -> Result<&mut Self, ErrorPayload> {
        self.registry.register_http_route(route, handler)?;
        Ok(self)
    }

    /// Runs this worker over the S5R 3.0 stdio transport.
    pub async fn run_stdio(mut self) -> Result<(), ErrorPayload> {
        use astrcode_extension_sdk::wire::{
            FeatureName, Peer as V3Peer, PeerInfo as V3PeerInfo, WorkerInitialization,
        };

        let extension_id = self.registry.extension_id().to_owned();
        let supported = BTreeSet::from([
            FeatureName::nested_invoke_v1(),
            FeatureName::model_stream_v1(),
            FeatureName::custom_event_v1(),
        ]);
        let mut initialization = WorkerInitialization::new(self.registry.take_manifest());
        initialization.supported_features = supported;
        let peer = V3Peer::new(
            ProcessStdioTransport::new(),
            V3PeerInfo {
                name: extension_id,
                version: Some(self.version),
            },
        )
        .accept(initialization)
        .await
        .map_err(v3_peer_error_to_payload)?
        .accept_activation()
        .await
        .map_err(v3_peer_error_to_payload)?;
        let (_handle, driver) = peer.into_runtime();
        let handler = Arc::new(V3WorkerInvokeHandler {
            registry: Arc::new(self.registry),
        });
        match driver.run(handler).await {
            Ok(()) => Ok(()),
            Err(astrcode_extension_sdk::wire::PeerError::Frame(
                astrcode_extension_sdk::wire::frame::FrameError::Io(error),
            )) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(()),
            Err(error) => Err(v3_peer_error_to_payload(error)),
        }
    }
}

type ErrorPayload = crate::s5r::ErrorPayload;

struct V3WorkerInvokeHandler {
    registry: Arc<HandlerRegistry>,
}

#[async_trait::async_trait]
impl astrcode_extension_sdk::wire::PeerInvokeHandler for V3WorkerInvokeHandler {
    async fn invoke(
        &self,
        invocation: astrcode_extension_sdk::wire::InboundInvoke,
    ) -> Result<
        astrcode_extension_sdk::wire::InvocationResponse,
        astrcode_extension_sdk::wire::protocol::ErrorPayload,
    > {
        if invocation.request.operation == crate::s5r::CAP_RUNTIME_PING {
            return Ok(astrcode_extension_sdk::wire::InvocationResponse::Unary(
                json!({ "ok": true }),
            ));
        }
        match invocation.request.operation.as_str() {
            astrcode_extension_sdk::wire::protocol::CONFORMANCE_UNARY => {
                return Ok(astrcode_extension_sdk::wire::InvocationResponse::Unary(
                    invocation.request.input,
                ));
            },
            astrcode_extension_sdk::wire::protocol::CONFORMANCE_STREAM => {
                let output = invocation.request.input;
                let events = futures_util::stream::iter([
                    astrcode_extension_sdk::wire::protocol::ModelStreamEvent::Started,
                    astrcode_extension_sdk::wire::protocol::ModelStreamEvent::ContentDelta {
                        content: "first".into(),
                    },
                    astrcode_extension_sdk::wire::protocol::ModelStreamEvent::ContentDelta {
                        content: "second".into(),
                    },
                    astrcode_extension_sdk::wire::protocol::ModelStreamEvent::Completed { output },
                ]);
                return Ok(astrcode_extension_sdk::wire::InvocationResponse::Stream(
                    Box::pin(events),
                ));
            },
            astrcode_extension_sdk::wire::protocol::CONFORMANCE_NESTED => {
                let output = invocation
                    .nested
                    .invoke(
                        astrcode_extension_sdk::wire::protocol::CONFORMANCE_HOST_ECHO,
                        invocation.request.input,
                    )
                    .await
                    .map_err(|error| {
                        astrcode_extension_sdk::wire::protocol::ErrorPayload::new(
                            astrcode_extension_sdk::wire::WireErrorCode::NestedFailed,
                            error.to_string(),
                        )
                    })?;
                return Ok(astrcode_extension_sdk::wire::InvocationResponse::Unary(
                    output,
                ));
            },
            astrcode_extension_sdk::wire::protocol::CONFORMANCE_WAIT_FOR_CANCEL => {
                invocation.cancellation.cancelled().await;
                return Err(astrcode_extension_sdk::wire::protocol::ErrorPayload::new(
                    astrcode_extension_sdk::wire::WireErrorCode::Cancelled,
                    "conformance invocation cancelled",
                ));
            },
            astrcode_extension_sdk::wire::protocol::CONFORMANCE_UNKNOWN_ERROR => {
                return Err(astrcode_extension_sdk::wire::protocol::ErrorPayload {
                    code: "future_conformance_error".into(),
                    message: "unknown error code preservation probe".into(),
                    hint: None,
                    retryable: false,
                    details: None,
                });
            },
            _ => {},
        }
        if invocation.request.operation != crate::s5r::CAP_HANDLER_INVOKE {
            return Err(astrcode_extension_sdk::wire::protocol::ErrorPayload::new(
                WireErrorCode::UnknownCapability,
                format!(
                    "worker does not handle capability {}",
                    invocation.request.operation
                ),
            ));
        }
        let token = invocation.cancellation;
        let host_api: Arc<dyn host::HostApi> = Arc::new(V3PeerHostApi::new(invocation.nested));
        let result = with_host_api(
            host_api,
            self.registry
                .dispatch_invoke(invocation.request.input, token),
        )
        .await?;
        let output = serde_json::to_value(result).map_err(|error| {
            astrcode_extension_sdk::wire::protocol::ErrorPayload::new(
                astrcode_extension_sdk::wire::WireErrorCode::SerializationFailed,
                format!("serialize S5R 3.0 handler result: {error}"),
            )
        })?;
        Ok(astrcode_extension_sdk::wire::InvocationResponse::Unary(
            output,
        ))
    }
}

fn v3_peer_error_to_payload(error: astrcode_extension_sdk::wire::PeerError) -> ErrorPayload {
    match error {
        astrcode_extension_sdk::wire::PeerError::Remote(error) => error,
        error => ErrorPayload::new(WireErrorCode::Transport, error.to_string()),
    }
}

pub fn tool_text(content: impl Into<String>, is_error: bool) -> HandlerResult {
    HandlerResult::effect(
        HandlerEffect::ToolOutcome,
        json!({
            "content": content.into(),
            "is_error": is_error,
        }),
    )
}
