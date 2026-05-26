//! Worker 运行时：扩展子进程入口。

mod builder;
mod host;
mod manifest;
mod registry;

use std::sync::Arc;

pub use builder::{
    command_handler, handler_err, hook_handler, hook_handler_args, parse_hook_input,
    parse_tool_arguments, tool_handler, tool_handler_args,
};
pub use host::{HostApi, HostClient, inject_host_api};
pub use manifest::{CommandManifestEntry, HookManifestEntry, ManifestCatalog};
pub use registry::{CommandHandlerFn, HookHandlerFn, ToolHandlerFn, WorkerCallContext};
use serde_json::{Value, json};

use crate::{
    runtime::{CancelToken, InvokeHandler, InvokeReply, Peer, ProcessStdioTransport},
    s5r::{HandlerDescriptor, HandlerResult, PeerInfo, S5R_STACK},
    tool::ToolDefinition,
    worker::{
        host::{PeerHostApi, set_host_api},
        registry::{HandlerRegistry, registration_metadata},
    },
};

const MAX_CONTINUATION_DEPTH: u32 = 16;

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
        self.registry.catalog_mut().capabilities.push(cap.into());
        self
    }

    /// 声明可发射的扩展事件 schema。
    pub fn extension_event(mut self, event: Value) -> Self {
        self.registry.catalog_mut().extension_events.push(event);
        self
    }

    /// 注册 tool：manifest 定义与 handler 一次完成，避免两处手动对齐。
    pub fn tool(&mut self, def: ToolDefinition, handler: ToolHandlerFn) {
        self.registry.register_tool(def, handler);
    }

    /// 注册 hook（`on` 为事件名，`mode` 为 `blocking` / `non_blocking`）。
    pub fn hook(&mut self, on: impl Into<String>, mode: impl Into<String>, handler: HookHandlerFn) {
        self.registry.register_hook(on, mode, handler);
    }

    /// 注册 slash command。
    pub fn command(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        handler: CommandHandlerFn,
    ) {
        self.registry.register_command(name, description, handler);
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

        let metadata = registration_metadata(&self.extension_id, &self.version, registry.catalog());
        let handlers = build_handler_descriptors(registry.catalog(), &self.extension_id);
        peer.initialize(handlers, metadata)
            .await
            .map_err(|e| crate::s5r::ErrorPayload::new("initialize_failed", e.to_string()))?;

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    }
}

type ErrorPayload = crate::s5r::ErrorPayload;

async fn handle_worker_invoke(
    registry: Arc<HandlerRegistry>,
    invoke: crate::s5r::InvokeMsg,
    token: CancelToken,
) -> Result<InvokeReply, ErrorPayload> {
    let mut stack = vec![(
        invoke
            .input
            .get("handler_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        invoke.input.get("event").cloned().unwrap_or(Value::Null),
        0u32,
    )];
    let mut first: Option<HandlerResult> = None;
    while let Some((hid, ev, depth)) = stack.pop() {
        if depth > MAX_CONTINUATION_DEPTH {
            return Err(ErrorPayload::new(
                "continuation_depth_exceeded",
                format!("continuation depth exceeded (max {MAX_CONTINUATION_DEPTH})"),
            ));
        }
        let fake_invoke = crate::s5r::InvokeMsg {
            id: invoke.id.clone(),
            capability: crate::s5r::CAP_HANDLER_INVOKE.into(),
            input: json!({ "handler_id": hid, "event": ev }),
            stream: false,
            caller_extension_id: invoke.caller_extension_id.clone(),
        };
        let resp = registry.dispatch_invoke(fake_invoke, token.clone()).await?;
        if first.is_none() {
            first = Some(resp.clone());
        }
        registry.push_continuation_stack(&resp.continuations, &mut stack, depth);
    }
    let result =
        first.ok_or_else(|| ErrorPayload::new("empty_handler_chain", "empty handler chain"))?;
    Ok(InvokeReply::Value(
        serde_json::to_value(result).unwrap_or(Value::Null),
    ))
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
    for cmd in &catalog.commands {
        out.push(HandlerDescriptor {
            handler_id: registry.handler_id_for("command", &cmd.name),
            description: cmd.description.clone(),
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
