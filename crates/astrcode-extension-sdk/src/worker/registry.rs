//! Worker 侧 handler 注册表。

use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use astrcode_core::wire::WireErrorCode;
use serde_json::Value;

use crate::{
    extension::{
        CompactEvent, ContinueAfterStopOptions, CustomEventDeclaration, CustomEventSubscription,
        ExtensionCapability, ExtensionHttpRequest, ExtensionHttpResponse, ExtensionHttpRoute,
        HookMode, LifecycleEvent, canonical_registration_name,
        extension_http_route_patterns_conflict, fixed_hook_mode, has_duplicate_registration_name,
        hook_mode_is_supported,
    },
    runtime::CancelToken,
    s5r::{
        CAP_HANDLER_INVOKE, ErrorPayload, HandlerId, HandlerKind, HandlerResult, InvokeMsg,
        capability_to_wire, compact_event_to_name, event_to_name,
        manifest::{ManifestCommand, ManifestHook, ManifestHookOptions, ManifestHttpRoute},
        mode_to_name,
    },
    worker::manifest::ManifestCatalog,
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

pub type HttpHandlerFn = Arc<
    dyn Fn(
            ExtensionHttpRequest,
            WorkerCallContext,
        ) -> BoxFuture<Result<ExtensionHttpResponse, ErrorPayload>>
        + Send
        + Sync,
>;

/// Worker handler 的运行时调用事实与取消信号。
#[derive(Clone)]
pub struct WorkerCallContext {
    extension_id: String,
    cancel_token: CancelToken,
    session_id: Option<String>,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
    working_dir: Option<PathBuf>,
}

impl WorkerCallContext {
    pub(crate) fn from_event(
        extension_id: String,
        cancel_token: CancelToken,
        event: &Value,
    ) -> Self {
        let input = event.get("input").unwrap_or(event);
        // Tool events use `tool_call_id`; tool-use hooks currently use `call_id` for the same fact.
        let tool_call_id =
            optional_string(input, "tool_call_id").or_else(|| optional_string(input, "call_id"));

        Self {
            extension_id,
            cancel_token,
            session_id: optional_string(input, "session_id").map(str::to_owned),
            turn_id: optional_string(input, "turn_id").map(str::to_owned),
            tool_call_id: tool_call_id.map(str::to_owned),
            working_dir: optional_string(input, "working_dir").map(PathBuf::from),
        }
    }

    /// 当前 worker 的扩展标识。
    pub fn extension_id(&self) -> &str {
        &self.extension_id
    }

    /// 当前调用所属 session；仅在线缆事件携带该事实时存在。
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Returns the host-attributed session or a stable context error for session-only handlers.
    pub fn require_session_id(&self) -> Result<&str, ErrorPayload> {
        self.session_id().ok_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::ContextUnavailable,
                "worker call requires a session-scoped context",
            )
        })
    }

    /// 当前调用所属 turn；仅在线缆事件携带该事实时存在。
    pub fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    /// 当前工具调用标识；tool 与 tool-use hook 的线缆字段会统一到此 accessor。
    pub fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id.as_deref()
    }

    /// 当前调用的工作目录；仅在线缆事件携带该事实时存在。
    pub fn working_dir(&self) -> Option<&Path> {
        self.working_dir.as_deref()
    }

    /// Returns the validated workspace or a stable context error for workspace-only handlers.
    pub fn require_working_dir(&self) -> Result<&Path, ErrorPayload> {
        self.working_dir().ok_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::ContextUnavailable,
                "worker call requires a workspace-scoped context",
            )
        })
    }

    /// 当前 S5R 调用的取消令牌。
    pub fn cancel_token(&self) -> &CancelToken {
        &self.cancel_token
    }
}

fn optional_string<'a>(input: &'a Value, field: &str) -> Option<&'a str> {
    input.get(field).and_then(Value::as_str)
}

pub(crate) struct HandlerRegistry {
    pub extension_id: String,
    catalog: ManifestCatalog,
    tools: HashMap<String, ToolHandlerFn>,
    hooks: HashMap<String, HookHandlerFn>,
    custom_events: HashMap<String, HookHandlerFn>,
    commands: HashMap<String, CommandHandlerFn>,
    http_routes: HashMap<String, HttpHandlerFn>,
}

impl HandlerRegistry {
    pub fn new(extension_id: impl Into<String>) -> Self {
        Self {
            extension_id: extension_id.into(),
            catalog: ManifestCatalog::default(),
            tools: HashMap::new(),
            hooks: HashMap::new(),
            custom_events: HashMap::new(),
            commands: HashMap::new(),
            http_routes: HashMap::new(),
        }
    }

    pub(crate) fn catalog(&self) -> &ManifestCatalog {
        &self.catalog
    }

    pub(crate) fn declare_capability(&mut self, cap: ExtensionCapability) {
        let cap = capability_to_wire(cap);
        if !self
            .catalog
            .capabilities
            .iter()
            .any(|declared| declared == cap)
        {
            self.catalog.capabilities.push(cap.into());
        }
    }

    pub(crate) fn declare_custom_event(&mut self, mut event: CustomEventDeclaration) {
        canonical_registration_name(&mut event.event_type);
        self.catalog.custom_events.push(event);
    }

    pub(crate) fn register_custom_event(
        &mut self,
        mut subscription: CustomEventSubscription,
        handler: HookHandlerFn,
    ) -> Result<(), ErrorPayload> {
        subscription.normalize();
        if let Err(reason) = subscription.validate() {
            return Err(ErrorPayload::new(WireErrorCode::InvalidInput, reason));
        }
        if has_duplicate_registration_name(
            self.custom_events.keys().map(String::as_str),
            &subscription.id,
        ) {
            return Err(ErrorPayload::new(
                WireErrorCode::DuplicateRegistration,
                format!("duplicate custom event subscription: {}", subscription.id),
            ));
        }
        self.custom_events.insert(subscription.id.clone(), handler);
        self.catalog.custom_event_subscriptions.push(subscription);
        Ok(())
    }

    pub(crate) fn register_tool(
        &mut self,
        mut def: crate::tool::ToolDefinition,
        handler: ToolHandlerFn,
    ) -> Result<(), ErrorPayload> {
        canonical_registration_name(&mut def.name);
        let name = def.name.clone();
        if has_duplicate_registration_name(self.tools.keys().map(String::as_str), &name) {
            return Err(ErrorPayload::new(
                WireErrorCode::DuplicateRegistration,
                format!("duplicate tool registration: {name}"),
            ));
        }
        self.catalog.tools.push(def);
        self.tools.insert(name, handler);
        Ok(())
    }

    pub(crate) fn register_hook(
        &mut self,
        on: LifecycleEvent,
        mode: HookMode,
        handler: HookHandlerFn,
    ) -> Result<(), ErrorPayload> {
        if on == LifecycleEvent::UserMessageEnvelope {
            return Err(ErrorPayload::new(
                WireErrorCode::UnsupportedHook,
                "user_message_envelope is not supported by S5R workers",
            ));
        }
        if let Some(required) = fixed_hook_mode(&on) {
            return Err(ErrorPayload::new(
                WireErrorCode::TypedHookRequired,
                format!(
                    "{} has fixed {} mode; {}",
                    event_to_name(&on),
                    mode_to_name(required),
                    fixed_worker_hook_hint(&on)
                ),
            ));
        }
        if !hook_mode_is_supported(&on, mode) {
            return Err(ErrorPayload::new(
                WireErrorCode::InvalidHookMode,
                format!(
                    "{} does not support {} mode",
                    event_to_name(&on),
                    mode_to_name(mode)
                ),
            ));
        }
        self.register_manifest_hook(on, mode, ManifestHookOptions::default(), handler)
    }

    pub(crate) fn register_fixed_hook(
        &mut self,
        on: LifecycleEvent,
        handler: HookHandlerFn,
    ) -> Result<(), ErrorPayload> {
        self.register_fixed_hook_with_options(on, ManifestHookOptions::default(), handler)
    }

    pub(crate) fn register_compact_hook(
        &mut self,
        on: CompactEvent,
        handler: HookHandlerFn,
    ) -> Result<(), ErrorPayload> {
        self.register_manifest_hook_name(
            compact_event_to_name(on),
            HookMode::Blocking,
            ManifestHookOptions::default(),
            handler,
        )
    }

    fn register_fixed_hook_with_options(
        &mut self,
        on: LifecycleEvent,
        options: ManifestHookOptions,
        handler: HookHandlerFn,
    ) -> Result<(), ErrorPayload> {
        if on == LifecycleEvent::UserMessageEnvelope {
            return Err(ErrorPayload::new(
                WireErrorCode::UnsupportedHook,
                "user_message_envelope is not supported by S5R workers",
            ));
        }
        let mode = fixed_hook_mode(&on).ok_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::InvalidHookRegistration,
                format!("{} is not a fixed-mode hook", event_to_name(&on)),
            )
        })?;
        self.register_manifest_hook(on, mode, options, handler)
    }

    fn register_manifest_hook(
        &mut self,
        on: LifecycleEvent,
        mode: HookMode,
        options: ManifestHookOptions,
        handler: HookHandlerFn,
    ) -> Result<(), ErrorPayload> {
        self.register_manifest_hook_name(event_to_name(&on), mode, options, handler)
    }

    fn register_manifest_hook_name(
        &mut self,
        on: &str,
        mode: HookMode,
        options: ManifestHookOptions,
        handler: HookHandlerFn,
    ) -> Result<(), ErrorPayload> {
        self.insert_hook_handler(on.to_owned(), handler)?;
        self.catalog.hooks.push(ManifestHook {
            on: on.to_owned(),
            mode: mode_to_name(mode).into(),
            options,
        });
        Ok(())
    }

    pub(crate) fn register_continuation_hook_handler(
        &mut self,
        on: impl Into<String>,
        handler: HookHandlerFn,
    ) -> Result<(), ErrorPayload> {
        let on = on.into();
        self.insert_hook_handler(on.clone(), handler)?;
        self.catalog.continuation_hooks.push(on);
        Ok(())
    }

    pub(crate) fn register_continue_after_stop_hook(
        &mut self,
        options: ContinueAfterStopOptions,
        handler: HookHandlerFn,
    ) -> Result<(), ErrorPayload> {
        self.register_fixed_hook_with_options(
            LifecycleEvent::ContinueAfterStop,
            ManifestHookOptions {
                max_per_turn: Some(options.max_per_turn),
            },
            handler,
        )
    }

    fn insert_hook_handler(
        &mut self,
        on: String,
        handler: HookHandlerFn,
    ) -> Result<(), ErrorPayload> {
        if has_duplicate_registration_name(self.hooks.keys().map(String::as_str), &on) {
            return Err(ErrorPayload::new(
                WireErrorCode::DuplicateRegistration,
                format!("duplicate hook registration: {on}"),
            ));
        }
        self.hooks.insert(on, handler);
        Ok(())
    }

    pub(crate) fn register_command(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        handler: CommandHandlerFn,
    ) -> Result<(), ErrorPayload> {
        let mut name = name.into();
        canonical_registration_name(&mut name);
        if has_duplicate_registration_name(self.commands.keys().map(String::as_str), &name) {
            return Err(ErrorPayload::new(
                WireErrorCode::DuplicateRegistration,
                format!("duplicate command registration: {name}"),
            ));
        }
        self.catalog.commands.push(ManifestCommand {
            name: name.clone(),
            description: description.into(),
        });
        self.commands.insert(name, handler);
        Ok(())
    }

    pub(crate) fn register_http_route(
        &mut self,
        route: ExtensionHttpRoute,
        handler: HttpHandlerFn,
    ) -> Result<(), ErrorPayload> {
        route
            .validate()
            .map_err(|error| ErrorPayload::new(WireErrorCode::InvalidHttpRoute, error))?;
        if self.catalog.http_routes.iter().any(|entry| {
            entry.route.access == route.access
                && entry.route.method == route.method
                && extension_http_route_patterns_conflict(&entry.route.path, &route.path)
        }) {
            return Err(ErrorPayload::new(
                WireErrorCode::DuplicateRegistration,
                format!("conflicting HTTP route registration: {}", route.path),
            ));
        }
        let handler_name = format!("route_{}", self.catalog.http_routes.len());
        let handler_id =
            HandlerId::new(&self.extension_id, HandlerKind::Http, &handler_name).into();
        self.catalog
            .http_routes
            .push(ManifestHttpRoute { route, handler_id });
        self.http_routes.insert(handler_name, handler);
        Ok(())
    }

    pub async fn dispatch_invoke(
        &self,
        invoke: InvokeMsg,
        token: CancelToken,
    ) -> Result<HandlerResult, ErrorPayload> {
        if invoke.capability != CAP_HANDLER_INVOKE {
            return Err(ErrorPayload::new(
                WireErrorCode::UnknownCapability,
                format!("worker does not handle capability {}", invoke.capability),
            ));
        }
        token
            .raise_if_cancelled()
            .map_err(|e| ErrorPayload::new(WireErrorCode::Cancelled, e))?;
        let handler_id = invoke.input["handler_id"]
            .as_str()
            .ok_or_else(|| ErrorPayload::new(WireErrorCode::InvalidInput, "handler_id required"))?;
        let event = invoke.input.get("event").cloned().unwrap_or(Value::Null);
        let ctx = WorkerCallContext::from_event(self.extension_id.clone(), token, &event);
        self.dispatch_handler(handler_id, event, ctx).await
    }

    async fn dispatch_handler(
        &self,
        handler_id: &str,
        event: Value,
        ctx: WorkerCallContext,
    ) -> Result<HandlerResult, ErrorPayload> {
        let prefix = format!("{}:", self.extension_id);
        let Some(handler_name) = handler_id.strip_prefix(&prefix) else {
            return Err(ErrorPayload::new(
                WireErrorCode::UnknownHandler,
                format!("unknown handler: {handler_id}"),
            ));
        };
        let (kind, name) = handler_name.split_once(':').unwrap_or((handler_name, ""));
        match kind {
            "tool" => {
                let handler = self.tools.get(name).ok_or_else(|| {
                    ErrorPayload::new(
                        WireErrorCode::UnknownHandler,
                        format!("unknown tool: {name}"),
                    )
                })?;
                handler(event, ctx).await
            },
            "hook" => {
                let handler = self.hooks.get(name).ok_or_else(|| {
                    ErrorPayload::new(
                        WireErrorCode::UnknownHandler,
                        format!("unknown hook: {name}"),
                    )
                })?;
                handler(event, ctx).await
            },
            "command" => {
                let handler = self.commands.get(name).ok_or_else(|| {
                    ErrorPayload::new(
                        WireErrorCode::UnknownHandler,
                        format!("unknown command: {name}"),
                    )
                })?;
                handler(event, ctx).await
            },
            "http" => {
                let handler = self.http_routes.get(name).ok_or_else(|| {
                    ErrorPayload::new(
                        WireErrorCode::UnknownHandler,
                        format!("unknown HTTP route: {name}"),
                    )
                })?;
                let request = serde_json::from_value(event).map_err(|error| {
                    ErrorPayload::new(
                        WireErrorCode::UnknownHandler,
                        format!("invalid HTTP request payload: {error}"),
                    )
                })?;
                let response = handler(request, ctx).await?;
                let data = serde_json::to_value(response).map_err(|error| {
                    ErrorPayload::new(
                        WireErrorCode::UnknownHandler,
                        format!("serialize HTTP response: {error}"),
                    )
                })?;
                Ok(HandlerResult::effect("http_response", data))
            },
            "event" => {
                let handler = self.custom_events.get(name).ok_or_else(|| {
                    ErrorPayload::new(
                        WireErrorCode::UnknownHandler,
                        format!("unknown custom event subscription: {name}"),
                    )
                })?;
                handler(event, ctx).await
            },
            _ => Err(ErrorPayload::new(
                WireErrorCode::UnknownHandler,
                format!("unknown handler kind in {handler_id}"),
            )),
        }
    }
}

fn fixed_worker_hook_hint(event: &LifecycleEvent) -> &'static str {
    match event {
        LifecycleEvent::AfterProviderResponse => {
            "use Worker::on_after_provider_response(...) instead"
        },
        LifecycleEvent::ContinueAfterStop => "use Worker::on_continue_after_stop(...) instead",
        LifecycleEvent::PromptBuild => "use Worker::on_prompt_build(...) instead",
        _ => "use the dedicated fixed-mode Worker registration method instead",
    }
}

pub(crate) fn registration_metadata(
    extension_id: &str,
    version: &str,
    catalog: &ManifestCatalog,
) -> Result<Value, String> {
    catalog.to_metadata_value(extension_id, version)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::worker_prelude::{
        ExtensionHttpMethod, ExtensionHttpRequest, ExtensionHttpResponse, ExtensionHttpRoute,
    };

    #[tokio::test]
    async fn continuation_hook_handler_dispatches_without_manifest_subscription() {
        let mut registry = HandlerRegistry::new("test-extension");
        registry
            .register_continuation_hook_handler(
                "pipeline_step",
                crate::worker::hook_handler(|_| async {
                    Ok(HandlerResult::effect("ok", json!({"step": 1})))
                }),
            )
            .unwrap();

        let metadata =
            registration_metadata("test-extension", "0.1.0", registry.catalog()).unwrap();
        assert_eq!(metadata["hooks"], json!([]));

        let result = registry
            .dispatch_invoke(
                InvokeMsg {
                    id: "invoke-1".into(),
                    capability: CAP_HANDLER_INVOKE.into(),
                    input: json!({
                        "handler_id": "test-extension:hook:pipeline_step",
                        "event": {}
                    }),
                    stream: false,
                    parent_invoke_id: None,
                },
                CancelToken::default(),
            )
            .await
            .unwrap();

        assert!(result.ok);
        assert_eq!(result.data_value("step"), Some(&json!(1)));
    }

    #[test]
    fn http_route_conflicts_are_scoped_by_access_and_method() {
        let mut registry = HandlerRegistry::new("test-extension");
        let handler = crate::worker::http_handler(|_: ExtensionHttpRequest, _| async {
            Ok(ExtensionHttpResponse::json(200, json!({})))
        });

        for route in [
            ExtensionHttpRoute::public(ExtensionHttpMethod::Get, "/jobs/{id}"),
            ExtensionHttpRoute::authenticated(ExtensionHttpMethod::Get, "/jobs/{id}"),
        ] {
            registry
                .register_http_route(route, Arc::clone(&handler))
                .unwrap();
        }

        let duplicate = ExtensionHttpRoute::authenticated(
            ExtensionHttpMethod::Get,
            "/jobs/{different_parameter_name}",
        );
        let error = registry
            .register_http_route(duplicate, handler)
            .expect_err("same-access conflicting route must be rejected");

        assert_eq!(registry.catalog.http_routes.len(), 2);
        assert_eq!(
            error.code_enum(),
            Some(WireErrorCode::DuplicateRegistration)
        );
    }

    #[test]
    fn worker_registration_names_use_the_same_canonical_form_as_the_host() {
        let mut registry = HandlerRegistry::new("test-extension");
        let tool_handler =
            crate::worker::tool_handler(|_| async { Ok(HandlerResult::effect("ok", json!({}))) });
        registry
            .register_tool(
                crate::builder::worker_tool("  review  ").build().into(),
                Arc::clone(&tool_handler),
            )
            .unwrap();
        registry
            .register_command(
                "  inspect  ",
                "Inspect state",
                crate::worker::command_handler(|_| async {
                    Ok(HandlerResult::effect("ok", json!({})))
                }),
            )
            .unwrap();
        registry.declare_custom_event(CustomEventDeclaration {
            event_type: "  review.completed  ".into(),
            schema_version: 1,
            durable: true,
            max_payload_bytes: 1024,
        });

        assert_eq!(registry.catalog.tools[0].name, "review");
        assert_eq!(registry.catalog.commands[0].name, "inspect");
        assert_eq!(
            registry.catalog.custom_events[0].event_type,
            "review.completed"
        );
        assert_eq!(
            registry
                .register_tool(
                    crate::builder::worker_tool("review").build().into(),
                    tool_handler,
                )
                .expect_err("canonical duplicate")
                .code_enum(),
            Some(WireErrorCode::DuplicateRegistration)
        );
    }

    #[test]
    fn compact_hooks_keep_their_wire_names_outside_lifecycle_events() {
        let mut registry = HandlerRegistry::new("test-extension");
        let handler =
            crate::worker::hook_handler(|_| async { Ok(HandlerResult::effect("ok", json!({}))) });

        registry
            .register_compact_hook(CompactEvent::PreCompact, Arc::clone(&handler))
            .unwrap();
        registry
            .register_compact_hook(CompactEvent::PostCompact, handler)
            .unwrap();

        let metadata =
            registration_metadata("test-extension", "0.1.0", registry.catalog()).unwrap();
        assert_eq!(
            metadata["hooks"],
            json!([
                { "on": "pre_compact", "mode": "blocking" },
                { "on": "post_compact", "mode": "blocking" }
            ])
        );
    }

    #[test]
    fn worker_call_context_requires_scoped_facts_with_stable_errors() {
        let scoped = WorkerCallContext::from_event(
            "test-extension".into(),
            CancelToken::default(),
            &json!({
                "input": {
                    "session_id": "session-1",
                    "working_dir": "/workspace"
                }
            }),
        );
        assert_eq!(scoped.require_session_id().unwrap(), "session-1");
        assert_eq!(
            scoped.require_working_dir().unwrap(),
            Path::new("/workspace")
        );

        let unscoped = WorkerCallContext::from_event(
            "test-extension".into(),
            CancelToken::default(),
            &Value::Null,
        );
        assert_eq!(
            unscoped.require_session_id().unwrap_err().code_enum(),
            Some(WireErrorCode::ContextUnavailable)
        );
        assert_eq!(
            unscoped.require_working_dir().unwrap_err().code_enum(),
            Some(WireErrorCode::ContextUnavailable)
        );
    }
}
