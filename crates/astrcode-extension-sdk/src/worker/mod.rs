//! Worker 运行时：扩展子进程入口。

mod builder;
mod host;
mod manifest;
mod registry;

use std::sync::Arc;

pub use builder::{
    command_handler, handler_err, hook_handler, hook_handler_args, http_handler, parse_hook_input,
    parse_tool_arguments, tool_handler, tool_handler_args,
};
pub use host::{
    HostApi, HostClient, HostConfigureSessionToolsOutput, HostConfigureSessionToolsRequest,
    HostNetworkRequest, HostNetworkResponse, HostProcessOutput, HostProcessRequest,
    HostSessionDeliveryOutput, HostSessionExecutionView, HostSessionInputRequest,
    HostSessionTargetRequest, HostWorkspaceEditOutput, HostWorkspaceEditRequest,
    HostWorkspaceGlobOutput, HostWorkspaceGlobRequest, HostWorkspaceGrepMatch,
    HostWorkspaceGrepOutput, HostWorkspaceGrepRequest, HostWorkspaceListEntry,
    HostWorkspaceListOutput, HostWorkspaceListRequest, HostWorkspaceWriteOutput,
    HostWorkspaceWriteRequest, inject_host_api,
};
pub use registry::{
    CommandHandlerFn, HookHandlerFn, HttpHandlerFn, ToolHandlerFn, WorkerCallContext,
};
use serde_json::{Value, json};

pub use crate::session::{
    HostCreateSessionOutput, HostCreateSessionRequest, HostSubmitTurnOutput, HostSubmitTurnRequest,
};
use crate::{
    extension::ContinueAfterStopOptions,
    runtime::{CancelToken, InvokeHandler, InvokeReply, Peer, ProcessStdioTransport},
    s5r::{HandlerDescriptor, HandlerResult, PeerInfo, S5R_STACK},
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

    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// 声明 manifest 能力（wire 名，如 `small_model`）。
    pub fn capability(mut self, cap: impl Into<String>) -> Self {
        self.registry.declare_capability(cap);
        self
    }

    /// 声明可发射的扩展事件 schema（兼容旧版 JSON authoring API）。
    pub fn extension_event(mut self, event: Value) -> Self {
        self.registry.declare_legacy_extension_event(event);
        self
    }

    /// 使用强类型契约声明可发射的扩展事件 schema。
    pub fn extension_event_decl(mut self, event: crate::extension::ExtensionEventDecl) -> Self {
        self.registry.declare_extension_event(event);
        self
    }

    /// 注册 tool：manifest 定义与 handler 一次完成，避免两处手动对齐。
    pub fn tool(
        &mut self,
        def: ToolDefinition,
        handler: ToolHandlerFn,
    ) -> Result<&mut Self, ErrorPayload> {
        self.registry.register_tool(def, handler)?;
        Ok(self)
    }

    /// 注册 hook（`on` 为事件名，`mode` 为 `blocking` / `non_blocking`）。
    pub fn hook(
        &mut self,
        on: impl Into<String>,
        mode: impl Into<String>,
        handler: HookHandlerFn,
    ) -> Result<&mut Self, ErrorPayload> {
        self.registry.register_hook(on, mode, handler)?;
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
        peer.start()
            .await
            .map_err(|e| crate::s5r::ErrorPayload::new("peer_start_failed", e.to_string()))?;
        set_host_api(Arc::new(PeerHostApi::new(
            Arc::clone(&peer),
            self.extension_id.clone(),
        )))
        .map_err(|_| {
            crate::s5r::ErrorPayload::new("host_api_already_set", "host API already initialized")
        })?;

        let registry = Arc::new(self.registry);
        let invoke_handler: InvokeHandler = {
            let registry = Arc::clone(&registry);
            Arc::new(move |invoke, token| {
                let registry = Arc::clone(&registry);
                Box::pin(async move { handle_worker_invoke(registry, invoke, token).await })
            })
        };
        peer.set_invoke_handler(invoke_handler);

        let metadata = registration_metadata(&self.extension_id, &self.version, registry.catalog())
            .map_err(|error| {
                ErrorPayload::new(
                    "manifest_serialize_failed",
                    format!("failed to serialize initialize manifest: {error}"),
                )
            })?;
        let handlers = build_handler_descriptors(registry.catalog(), &self.extension_id);
        peer.initialize(handlers, metadata)
            .await
            .map_err(|e| crate::s5r::ErrorPayload::new("initialize_failed", e.to_string()))?;

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
            "serialization_failed",
            format!("serialize handler result: {error}"),
        )
    })?;
    Ok(InvokeReply::Value(output))
}

fn build_handler_descriptors(
    catalog: &ManifestCatalog,
    extension_id: &str,
) -> Vec<HandlerDescriptor> {
    let registry = HandlerRegistry::new(extension_id);
    let mut out = Vec::new();
    for tool in &catalog.tools {
        out.push(HandlerDescriptor {
            handler_id: registry.handler_id_for("tool", &tool.name),
            description: tool.description.clone(),
            input_schema: tool.parameters.clone(),
        });
    }
    for hook in &catalog.hooks {
        out.push(HandlerDescriptor {
            handler_id: registry.handler_id_for("hook", &hook.on),
            description: format!("hook {}", hook.on),
            input_schema: json!({"type":"object"}),
        });
    }
    for hook in &catalog.continuation_hooks {
        out.push(HandlerDescriptor {
            handler_id: registry.handler_id_for("hook", hook),
            description: format!("continuation hook {hook}"),
            input_schema: json!({"type":"object"}),
        });
    }
    for cmd in &catalog.commands {
        out.push(HandlerDescriptor {
            handler_id: registry.handler_id_for("command", &cmd.name),
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
    out
}

pub fn tool_text(content: impl Into<String>, is_error: bool) -> HandlerResult {
    if is_error {
        HandlerResult::err(content.into())
    } else {
        HandlerResult::effect("ok", json!({ "content": content.into() }))
    }
}
