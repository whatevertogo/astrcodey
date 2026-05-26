//! Worker 侧 handler 注册表。

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

use serde_json::Value;

use crate::{
    runtime::CancelToken,
    s5r::{CAP_HANDLER_INVOKE, ErrorPayload, HandlerResult, InvokeMsg},
    worker::manifest::{CommandManifestEntry, HookManifestEntry, ManifestCatalog},
};

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

pub type ToolHandlerFn = Arc<
    dyn Fn(Value, WorkerCallContext) -> BoxFuture<Result<HandlerResult, ErrorPayload>>
        + Send
        + Sync,
>;

pub type HookHandlerFn = Arc<
    dyn Fn(Value, WorkerCallContext) -> BoxFuture<Result<HandlerResult, ErrorPayload>>
        + Send
        + Sync,
>;

pub type CommandHandlerFn = Arc<
    dyn Fn(Value, WorkerCallContext) -> BoxFuture<Result<HandlerResult, ErrorPayload>>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct WorkerCallContext {
    pub extension_id: String,
    pub cancel_token: CancelToken,
}

pub struct HandlerRegistry {
    pub extension_id: String,
    catalog: ManifestCatalog,
    tools: HashMap<String, ToolHandlerFn>,
    hooks: HashMap<String, HookHandlerFn>,
    commands: HashMap<String, CommandHandlerFn>,
}

impl HandlerRegistry {
    pub fn new(extension_id: impl Into<String>) -> Self {
        Self {
            extension_id: extension_id.into(),
            catalog: ManifestCatalog::default(),
            tools: HashMap::new(),
            hooks: HashMap::new(),
            commands: HashMap::new(),
        }
    }

    pub fn catalog(&self) -> &ManifestCatalog {
        &self.catalog
    }

    pub fn catalog_mut(&mut self) -> &mut ManifestCatalog {
        &mut self.catalog
    }

    pub fn register_tool(&mut self, def: crate::tool::ToolDefinition, handler: ToolHandlerFn) {
        let name = def.name.clone();
        if self.tools.contains_key(&name) {
            panic!("duplicate tool registration: {name}");
        }
        self.catalog.tools.push(def);
        self.tools.insert(name, handler);
    }

    pub fn register_hook(
        &mut self,
        on: impl Into<String>,
        mode: impl Into<String>,
        handler: HookHandlerFn,
    ) {
        let on = on.into();
        if self.hooks.contains_key(&on) {
            panic!("duplicate hook registration: {on}");
        }
        self.catalog.hooks.push(HookManifestEntry {
            on: on.clone(),
            mode: mode.into(),
        });
        self.hooks.insert(on, handler);
    }

    pub fn register_command(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        handler: CommandHandlerFn,
    ) {
        let name = name.into();
        if self.commands.contains_key(&name) {
            panic!("duplicate command registration: {name}");
        }
        self.catalog.commands.push(CommandManifestEntry {
            name: name.clone(),
            description: description.into(),
        });
        self.commands.insert(name, handler);
    }

    /// 兼容旧 API：仅注册 handler，须已在 catalog 中存在对应条目。
    #[allow(dead_code)]
    pub fn register_tool_handler(&mut self, name: impl Into<String>, handler: ToolHandlerFn) {
        let name = name.into();
        if !self.catalog.tools.iter().any(|t| t.name == name) {
            panic!(
                "register_tool_handler({name}): missing tool manifest entry — use Worker::tool() \
                 instead"
            );
        }
        self.tools.insert(name, handler);
    }

    #[allow(dead_code)]
    pub fn register_hook_handler(&mut self, on: impl Into<String>, handler: HookHandlerFn) {
        let on = on.into();
        if !self.catalog.hooks.iter().any(|h| h.on == on) {
            panic!(
                "register_hook_handler({on}): missing hook manifest entry — use Worker::hook() \
                 instead"
            );
        }
        self.hooks.insert(on, handler);
    }

    #[allow(dead_code)]
    pub fn register_command_handler(&mut self, name: impl Into<String>, handler: CommandHandlerFn) {
        let name = name.into();
        if !self.catalog.commands.iter().any(|c| c.name == name) {
            panic!(
                "register_command_handler({name}): missing command manifest entry — use \
                 Worker::command() instead"
            );
        }
        self.commands.insert(name, handler);
    }

    pub fn handler_id_for(&self, kind: &str, name: &str) -> String {
        format!("{}:{kind}:{name}", self.extension_id)
    }

    pub async fn dispatch_invoke(
        &self,
        invoke: InvokeMsg,
        token: CancelToken,
    ) -> Result<HandlerResult, ErrorPayload> {
        if invoke.capability != CAP_HANDLER_INVOKE {
            return Err(ErrorPayload::new(
                "unknown_capability",
                format!("worker does not handle capability {}", invoke.capability),
            ));
        }
        token
            .raise_if_cancelled()
            .map_err(|e| ErrorPayload::new("cancelled", e))?;
        let handler_id = invoke.input["handler_id"]
            .as_str()
            .ok_or_else(|| ErrorPayload::new("invalid_input", "handler_id required"))?;
        let event = invoke.input.get("event").cloned().unwrap_or(Value::Null);
        let ctx = WorkerCallContext {
            extension_id: self.extension_id.clone(),
            cancel_token: token,
        };
        self.dispatch_handler(handler_id, event, ctx).await
    }

    async fn dispatch_handler(
        &self,
        handler_id: &str,
        event: Value,
        ctx: WorkerCallContext,
    ) -> Result<HandlerResult, ErrorPayload> {
        let prefix = format!("{}:", self.extension_id);
        if !handler_id.starts_with(&prefix) {
            return Err(ErrorPayload::new(
                "unknown_handler",
                format!("unknown handler: {handler_id}"),
            ));
        }
        let rest = &handler_id[prefix.len()..];
        let mut parts = rest.splitn(2, ':');
        let kind = parts.next().unwrap_or("");
        let name = parts.next().unwrap_or("");
        let result = match kind {
            "tool" => {
                let handler = self.tools.get(name).ok_or_else(|| {
                    ErrorPayload::new("unknown_handler", format!("unknown tool: {name}"))
                })?;
                handler(event, ctx).await
            },
            "hook" => {
                let handler = self.hooks.get(name).ok_or_else(|| {
                    ErrorPayload::new("unknown_handler", format!("unknown hook: {name}"))
                })?;
                handler(event, ctx).await
            },
            "command" => {
                let handler = self.commands.get(name).ok_or_else(|| {
                    ErrorPayload::new("unknown_handler", format!("unknown command: {name}"))
                })?;
                handler(event, ctx).await
            },
            _ => {
                return Err(ErrorPayload::new(
                    "unknown_handler",
                    format!("unknown handler kind in {handler_id}"),
                ));
            },
        }?;
        Ok(result)
    }

    pub fn push_continuation_stack(
        &self,
        continuations: &[crate::s5r::CallContinuation],
        stack: &mut Vec<(String, Value, u32)>,
        depth: u32,
    ) {
        for cont in continuations.iter().rev() {
            let (hid, ev) = cont.handler_id_for_extension(&self.extension_id);
            stack.push((hid, ev, depth + 1));
        }
    }
}

pub fn registration_metadata(
    extension_id: &str,
    version: &str,
    catalog: &ManifestCatalog,
) -> Value {
    catalog.to_metadata_value(extension_id, version)
}
