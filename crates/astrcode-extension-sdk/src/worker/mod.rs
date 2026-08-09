//! Worker 运行时：扩展子进程入口。

mod builder;
mod host;
mod manifest;
mod registry;

use std::sync::Arc;

use astrcode_core::wire::WireErrorCode;
pub use builder::{
    command_handler, handler_err, hook_handler, hook_handler_args, http_handler, parse_hook_input,
    parse_tool_arguments, tool_handler, tool_handler_args,
};
pub use host::{
    EventClient, ExtensionHttpClient, HostClient, HostConfigureSessionToolsOutput,
    HostConfigureSessionToolsRequest, HostCreateSessionOutput, HostCreateSessionRequest,
    HostEventEmitOutput, HostEventEmitRequest, HostLlmChatOutput, HostLlmCollectedStreamOutput,
    HostLlmContent, HostLlmMessage, HostLlmRole, HostLlmTextDelta, HostNetworkRedirectPolicy,
    HostNetworkRequest, HostNetworkResponse, HostProcessOutput, HostProcessRequest,
    HostRecycleSessionRequest, HostRootSubmitTurnRequest, HostSessionCancelOutput,
    HostSessionDeliveryOutput, HostSessionEvent, HostSessionEventsPageOutput,
    HostSessionEventsPageRequest, HostSessionExecutionView, HostSessionInputRequest,
    HostSessionProviderMessagesOutput, HostSessionReactivateOutput, HostSessionStateOutput,
    HostSessionStateReadOutput, HostSessionStateReadRequest, HostSessionStateWriteRequest,
    HostSessionSummariesOutput, HostSessionSummary, HostSessionTargetRequest,
    HostSessionTokenUsage, HostSessionTokenUsageOutput, HostSessionTranscript,
    HostSessionTranscriptMessage, HostSubmitTurnOutput, HostSubmitTurnRequest,
    HostWorkspaceEditOutput, HostWorkspaceEditRequest, HostWorkspaceGlobOutput,
    HostWorkspaceGlobRequest, HostWorkspaceGrepMatch, HostWorkspaceGrepOutput,
    HostWorkspaceGrepRequest, HostWorkspaceListEntry, HostWorkspaceListOutput,
    HostWorkspaceListRequest, HostWorkspaceReadOutput, HostWorkspaceReadRequest,
    HostWorkspaceWriteOutput, HostWorkspaceWriteRequest, ModelClient, NetworkClient, ProcessClient,
    SessionControlClient, SessionHistoryClient, SessionInspectClient, SessionStateClient,
    WorkspaceClient,
};
pub use registry::{
    CommandHandlerFn, HookHandlerFn, HttpHandlerFn, ToolHandlerFn, WorkerCallContext,
};
use serde_json::json;

/// Explicit transport seam for worker integration tests. Not part of the author prelude.
pub mod testing {
    pub use super::host::{HostApi, invoke_host, with_host_api};
}

use crate::{
    extension::{
        CompactEvent, ContinueAfterStopOptions, CustomEventSubscription, ExtensionCapability,
        HookMode, LifecycleEvent,
    },
    runtime::{CancelToken, InvokeHandler, InvokeReply, Peer, ProcessStdioTransport},
    s5r::{HandlerDescriptor, HandlerId, HandlerKind, HandlerResult, PeerInfo, S5R_STACK},
    tool::ToolDefinition,
    worker::{
        host::{PeerHostApi, set_host_api},
        manifest::ManifestCatalog,
        registry::{HandlerRegistry, registration_metadata},
    },
};

pub struct Worker {
    extension_id: String,
    version: String,
    registry: HandlerRegistry,
}

impl Worker {
    pub fn new(extension_id: impl Into<String>) -> Self {
        let extension_id = extension_id.into();
        Self {
            version: "0.1.0".into(),
            registry: HandlerRegistry::new(extension_id.clone()),
            extension_id,
        }
    }

    pub fn version(&mut self, version: impl Into<String>) -> &mut Self {
        self.version = version.into();
        self
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
        handler: HookHandlerFn,
    ) -> Result<&mut Self, ErrorPayload> {
        self.registry.register_custom_event(subscription, handler)?;
        Ok(self)
    }

    /// 注册 tool：manifest 定义与 handler 一次完成，避免两处手动对齐。
    pub fn tool(
        &mut self,
        def: impl Into<ToolDefinition>,
        handler: ToolHandlerFn,
    ) -> Result<&mut Self, ErrorPayload> {
        self.registry.register_tool(def.into(), handler)?;
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
        handler: HookHandlerFn,
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

    pub async fn run_stdio(self) -> Result<(), ErrorPayload> {
        let transport = ProcessStdioTransport::new();
        let peer = Peer::new(
            transport,
            PeerInfo {
                name: self.extension_id.clone(),
                role: "plugin".into(),
                version: Some(S5R_STACK.into()),
            },
        );
        peer.start().await.map_err(|e| {
            crate::s5r::ErrorPayload::new(WireErrorCode::PeerStartFailed, e.to_string())
        })?;
        set_host_api(Arc::new(PeerHostApi::new(Arc::clone(&peer)))).map_err(|_| {
            crate::s5r::ErrorPayload::new(
                WireErrorCode::HostApiAlreadySet,
                "host API already initialized",
            )
        })?;

        let registry = Arc::new(self.registry);
        let invoke_handler: InvokeHandler = {
            let registry = Arc::clone(&registry);
            Arc::new(move |invoke, token| {
                let registry = Arc::clone(&registry);
                Box::pin(handle_worker_invoke(registry, invoke, token))
            })
        };
        peer.set_invoke_handler(invoke_handler);

        let metadata = registration_metadata(&self.extension_id, &self.version, registry.catalog())
            .map_err(|error| {
                ErrorPayload::new(
                    WireErrorCode::ManifestSerializeFailed,
                    format!("failed to serialize initialize manifest: {error}"),
                )
            })?;
        let handlers = build_handler_descriptors(registry.catalog(), &self.extension_id);
        peer.initialize(handlers, metadata).await.map_err(|e| {
            crate::s5r::ErrorPayload::new(WireErrorCode::InitializeFailed, e.to_string())
        })?;

        peer.wait_closed().await;
        Ok(())
    }
}

type ErrorPayload = crate::s5r::ErrorPayload;

async fn handle_worker_invoke(
    registry: Arc<HandlerRegistry>,
    invoke: crate::s5r::InvokeMsg,
    token: CancelToken,
) -> Result<InvokeReply, ErrorPayload> {
    let result = registry.dispatch_invoke(invoke, token).await?;
    let output = serde_json::to_value(result).map_err(|error| {
        ErrorPayload::new(
            WireErrorCode::SerializationFailed,
            format!("serialize handler result: {error}"),
        )
    })?;
    Ok(InvokeReply::Value(output))
}

fn build_handler_descriptors(
    catalog: &ManifestCatalog,
    extension_id: &str,
) -> Vec<HandlerDescriptor> {
    let mut out = Vec::new();
    for tool in &catalog.tools {
        out.push(HandlerDescriptor {
            handler_id: HandlerId::new(extension_id, HandlerKind::Tool, &tool.name).into(),
            description: tool.description.clone(),
            input_schema: tool.parameters.clone(),
        });
    }
    for hook in &catalog.hooks {
        out.push(HandlerDescriptor {
            handler_id: HandlerId::new(extension_id, HandlerKind::Hook, &hook.on).into(),
            description: format!("hook {}", hook.on),
            input_schema: json!({"type":"object"}),
        });
    }
    for hook in &catalog.continuation_hooks {
        out.push(HandlerDescriptor {
            handler_id: HandlerId::new(extension_id, HandlerKind::Hook, hook).into(),
            description: format!("continuation hook {hook}"),
            input_schema: json!({"type":"object"}),
        });
    }
    for cmd in &catalog.commands {
        out.push(HandlerDescriptor {
            handler_id: HandlerId::new(extension_id, HandlerKind::Command, &cmd.name).into(),
            description: cmd.description.clone(),
            input_schema: json!({"type":"object"}),
        });
    }
    for route in &catalog.http_routes {
        out.push(HandlerDescriptor {
            handler_id: route.handler_id.clone(),
            description: route.route.description.clone(),
            input_schema: json!({"type":"object"}),
        });
    }
    for subscription in &catalog.custom_event_subscriptions {
        out.push(HandlerDescriptor {
            handler_id: HandlerId::new(extension_id, HandlerKind::Event, &subscription.id).into(),
            description: format!("custom event {}", subscription.event_type),
            input_schema: json!({"type":"object"}),
        });
    }
    out
}

pub fn tool_text(content: impl Into<String>, is_error: bool) -> HandlerResult {
    if is_error {
        HandlerResult::err(content.into())
    } else {
        HandlerResult::effect("ok", json!({ "content": content.into() }))
    }
}
