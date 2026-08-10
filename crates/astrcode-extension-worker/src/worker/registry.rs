//! Worker 侧 handler 注册表。

use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use astrcode_extension_contract::effects::EFFECT_HTTP_RESPONSE;
use serde_json::Value;

use super::CancelToken;
use crate::{
    WireErrorCode,
    extension::{
        CompactEvent, ContinueAfterStopOptions, CustomEventDeclaration, CustomEventSubscription,
        ExtensionCapability, ExtensionHttpRequest, ExtensionHttpResponse, ExtensionHttpRoute,
        HookMode, LifecycleEvent, canonical_registration_name,
        extension_http_route_patterns_conflict, fixed_hook_mode, has_duplicate_registration_name,
        hook_mode_is_supported,
    },
    s5r::{
        CAP_HANDLER_INVOKE, ErrorPayload, HandlerId, HandlerInvokeRequest, HandlerKind,
        HandlerResult, capability_to_wire, compact_event_to_name, event_to_name,
        manifest::{ManifestCommand, ManifestHook, ManifestHookOptions, ManifestHttpRoute},
        mode_to_name,
    },
    worker::manifest::ManifestCatalog,
};

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

pub(crate) struct HandlerInvoke {
    pub(crate) capability: String,
    pub(crate) input: Value,
}

pub type ToolHandlerFn = Arc<
    dyn Fn(Value, WorkerToolContext) -> BoxFuture<Result<HandlerResult, ErrorPayload>>
        + Send
        + Sync,
>;

pub type HookHandlerFn = Arc<
    dyn Fn(Value, WorkerHookContext) -> BoxFuture<Result<HandlerResult, ErrorPayload>>
        + Send
        + Sync,
>;

pub type ContinuationHandlerFn = Arc<
    dyn Fn(Value, WorkerCallContext) -> BoxFuture<Result<HandlerResult, ErrorPayload>>
        + Send
        + Sync,
>;

pub type CustomEventHandlerFn = Arc<
    dyn Fn(Value, WorkerCustomEventContext) -> BoxFuture<Result<HandlerResult, ErrorPayload>>
        + Send
        + Sync,
>;

pub type CommandHandlerFn = Arc<
    dyn Fn(Value, WorkerCommandContext) -> BoxFuture<Result<HandlerResult, ErrorPayload>>
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

/// Facts shared by worker calls that do not have session or workspace scope.
#[derive(Clone)]
pub struct WorkerCallContext {
    extension_id: String,
    cancel_token: CancelToken,
}

impl WorkerCallContext {
    /// 当前 worker 的扩展标识。
    pub fn extension_id(&self) -> &str {
        &self.extension_id
    }

    /// 当前 S5R 调用的取消令牌。
    pub fn cancel_token(&self) -> &CancelToken {
        &self.cancel_token
    }
}

/// Host-attributed facts guaranteed for a worker tool invocation.
#[derive(Clone)]
pub struct WorkerToolContext {
    scoped: WorkerSessionWorkspaceContext,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
}

impl WorkerToolContext {
    pub fn extension_id(&self) -> &str {
        self.scoped.call.extension_id()
    }

    pub fn session_id(&self) -> &str {
        &self.scoped.session_id
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    pub fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id.as_deref()
    }

    pub fn working_dir(&self) -> &Path {
        &self.scoped.working_dir
    }

    pub fn cancel_token(&self) -> &CancelToken {
        self.scoped.call.cancel_token()
    }
}

/// Host-attributed facts guaranteed for a worker hook invocation.
#[derive(Clone)]
pub struct WorkerHookContext {
    scoped: WorkerSessionWorkspaceContext,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
}

impl WorkerHookContext {
    pub fn extension_id(&self) -> &str {
        self.scoped.call.extension_id()
    }

    pub fn session_id(&self) -> &str {
        &self.scoped.session_id
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    pub fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id.as_deref()
    }

    pub fn working_dir(&self) -> &Path {
        &self.scoped.working_dir
    }

    pub fn cancel_token(&self) -> &CancelToken {
        self.scoped.call.cancel_token()
    }
}

/// Host-attributed facts guaranteed for a worker command invocation.
#[derive(Clone)]
pub struct WorkerCommandContext {
    scoped: WorkerSessionWorkspaceContext,
}

impl WorkerCommandContext {
    pub fn extension_id(&self) -> &str {
        self.scoped.call.extension_id()
    }

    pub fn session_id(&self) -> &str {
        &self.scoped.session_id
    }

    pub fn working_dir(&self) -> &Path {
        &self.scoped.working_dir
    }

    pub fn cancel_token(&self) -> &CancelToken {
        self.scoped.call.cancel_token()
    }
}

/// Host-attributed facts guaranteed for a worker custom-event delivery.
#[derive(Clone)]
pub struct WorkerCustomEventContext {
    call: WorkerCallContext,
    session_id: String,
    turn_id: Option<String>,
}

impl WorkerCustomEventContext {
    pub fn extension_id(&self) -> &str {
        self.call.extension_id()
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    pub fn cancel_token(&self) -> &CancelToken {
        self.call.cancel_token()
    }
}

pub(crate) struct WorkerCallFacts {
    call: WorkerCallContext,
    session_id: Option<String>,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
    working_dir: Option<PathBuf>,
}

#[derive(Clone)]
struct WorkerSessionWorkspaceContext {
    call: WorkerCallContext,
    session_id: String,
    working_dir: PathBuf,
}

impl WorkerCallFacts {
    pub(crate) fn from_event(
        extension_id: String,
        cancel_token: CancelToken,
        event: &Value,
    ) -> Result<Self, ErrorPayload> {
        let input = event.get("input").unwrap_or(event);
        // Tool events use `tool_call_id`; tool-use hooks currently use `call_id` for the same fact.
        let tool_call_id = match optional_string(input, "tool_call_id")? {
            Some(tool_call_id) => Some(tool_call_id),
            None => optional_string(input, "call_id")?,
        };

        Ok(Self {
            call: WorkerCallContext {
                extension_id,
                cancel_token,
            },
            session_id: optional_string(input, "session_id")?.map(str::to_owned),
            turn_id: optional_string(input, "turn_id")?.map(str::to_owned),
            tool_call_id: tool_call_id.map(str::to_owned),
            working_dir: optional_string(input, "working_dir")?.map(PathBuf::from),
        })
    }

    pub(crate) fn into_tool(self) -> Result<WorkerToolContext, ErrorPayload> {
        let (scoped, turn_id, tool_call_id) = self.into_scoped("tool")?;
        Ok(WorkerToolContext {
            scoped,
            turn_id,
            tool_call_id,
        })
    }

    pub(crate) fn into_hook(self) -> Result<WorkerHookContext, ErrorPayload> {
        let (scoped, turn_id, tool_call_id) = self.into_scoped("hook")?;
        Ok(WorkerHookContext {
            scoped,
            turn_id,
            tool_call_id,
        })
    }

    fn into_command(self) -> Result<WorkerCommandContext, ErrorPayload> {
        let (scoped, _, _) = self.into_scoped("command")?;
        Ok(WorkerCommandContext { scoped })
    }

    fn into_custom_event(self) -> Result<WorkerCustomEventContext, ErrorPayload> {
        let session_id = required_context_fact(self.session_id, "custom event", "session_id")?;
        Ok(WorkerCustomEventContext {
            call: self.call,
            session_id,
            turn_id: self.turn_id,
        })
    }

    fn into_scoped(
        self,
        kind: &'static str,
    ) -> Result<
        (
            WorkerSessionWorkspaceContext,
            Option<String>,
            Option<String>,
        ),
        ErrorPayload,
    > {
        let session_id = required_context_fact(self.session_id, kind, "session_id")?;
        let working_dir = required_context_fact(self.working_dir, kind, "working_dir")?;
        Ok((
            WorkerSessionWorkspaceContext {
                call: self.call,
                session_id,
                working_dir,
            },
            self.turn_id,
            self.tool_call_id,
        ))
    }
}

fn required_context_fact<T>(
    value: Option<T>,
    handler_kind: &str,
    field: &str,
) -> Result<T, ErrorPayload> {
    value.ok_or_else(|| {
        ErrorPayload::new(
            WireErrorCode::ContextUnavailable,
            format!("worker {handler_kind} call requires {field}"),
        )
    })
}

fn optional_string<'a>(input: &'a Value, field: &str) -> Result<Option<&'a str>, ErrorPayload> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(ErrorPayload::new(
            WireErrorCode::InvalidInput,
            format!("{field} must be a string when present"),
        )),
    }
}

pub(crate) struct HandlerRegistry {
    pub extension_id: String,
    catalog: ManifestCatalog,
    tools: HashMap<String, ToolHandlerFn>,
    hooks: HashMap<String, HookHandlerFn>,
    continuation_hooks: HashMap<String, ContinuationHandlerFn>,
    custom_events: HashMap<String, CustomEventHandlerFn>,
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
            continuation_hooks: HashMap::new(),
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
        handler: CustomEventHandlerFn,
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
        handler: ContinuationHandlerFn,
    ) -> Result<(), ErrorPayload> {
        let on = on.into();
        if has_duplicate_registration_name(
            self.hooks
                .keys()
                .chain(self.continuation_hooks.keys())
                .map(String::as_str),
            &on,
        ) {
            return Err(ErrorPayload::new(
                WireErrorCode::DuplicateRegistration,
                format!("duplicate hook registration: {on}"),
            ));
        }
        self.continuation_hooks.insert(on.clone(), handler);
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
        if has_duplicate_registration_name(
            self.hooks
                .keys()
                .chain(self.continuation_hooks.keys())
                .map(String::as_str),
            &on,
        ) {
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
        invoke: HandlerInvoke,
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
        let request: HandlerInvokeRequest =
            serde_json::from_value(invoke.input).map_err(|error| {
                ErrorPayload::new(
                    WireErrorCode::InvalidInput,
                    format!("invalid handler invocation: {error}"),
                )
            })?;
        let facts = WorkerCallFacts::from_event(self.extension_id.clone(), token, &request.event)?;
        self.dispatch_handler(&request.handler_id, request.event, facts)
            .await
    }

    async fn dispatch_handler(
        &self,
        handler_id: &str,
        event: Value,
        facts: WorkerCallFacts,
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
                handler(event, facts.into_tool()?).await
            },
            "hook" => {
                if let Some(handler) = self.hooks.get(name) {
                    handler(event, facts.into_hook()?).await
                } else if let Some(handler) = self.continuation_hooks.get(name) {
                    handler(event, facts.call).await
                } else {
                    Err(ErrorPayload::new(
                        WireErrorCode::UnknownHandler,
                        format!("unknown hook: {name}"),
                    ))
                }
            },
            "command" => {
                let handler = self.commands.get(name).ok_or_else(|| {
                    ErrorPayload::new(
                        WireErrorCode::UnknownHandler,
                        format!("unknown command: {name}"),
                    )
                })?;
                handler(event, facts.into_command()?).await
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
                        WireErrorCode::InvalidInput,
                        format!("invalid HTTP request payload: {error}"),
                    )
                })?;
                let response = handler(request, facts.call).await?;
                let data = serde_json::to_value(response).map_err(|error| {
                    ErrorPayload::new(
                        WireErrorCode::SerializationFailed,
                        format!("serialize HTTP response: {error}"),
                    )
                })?;
                Ok(HandlerResult::effect(EFFECT_HTTP_RESPONSE, data))
            },
            "event" => {
                let handler = self.custom_events.get(name).ok_or_else(|| {
                    ErrorPayload::new(
                        WireErrorCode::UnknownHandler,
                        format!("unknown custom event subscription: {name}"),
                    )
                })?;
                handler(event, facts.into_custom_event()?).await
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
                crate::worker::continuation_handler(|_| async {
                    Ok(HandlerResult::effect("ok", json!({"step": 1})))
                }),
            )
            .unwrap();

        let metadata = registry
            .catalog()
            .to_metadata_value("test-extension", "0.1.0")
            .unwrap();
        assert_eq!(metadata["hooks"], json!([]));

        let result = registry
            .dispatch_invoke(
                HandlerInvoke {
                    capability: CAP_HANDLER_INVOKE.into(),
                    input: json!({
                        "handler_id": "test-extension:hook:pipeline_step",
                        "event": {}
                    }),
                },
                CancelToken::default(),
            )
            .await
            .unwrap();

        assert!(result.ok);
        assert_eq!(result.data_value("step"), Some(&json!(1)));

        let error = registry
            .dispatch_invoke(
                HandlerInvoke {
                    capability: CAP_HANDLER_INVOKE.into(),
                    input: json!({
                        "handler_id": "test-extension:hook:pipeline_step",
                        "event": {},
                        "unknown": true
                    }),
                },
                CancelToken::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code_enum(), Some(WireErrorCode::InvalidInput));
    }

    #[test]
    fn lifecycle_hook_modes_are_validated_and_preserved() {
        let mut registry = HandlerRegistry::new("test-extension");
        let handler =
            crate::worker::hook_handler(|_| async { Ok(HandlerResult::effect("ok", json!({}))) });

        registry
            .register_hook(
                LifecycleEvent::TurnEnd,
                HookMode::NonBlocking,
                Arc::clone(&handler),
            )
            .unwrap();
        assert_eq!(registry.catalog.hooks[0].mode, "non_blocking");

        let invalid = registry
            .register_hook(
                LifecycleEvent::SessionStart,
                HookMode::Blocking,
                Arc::clone(&handler),
            )
            .expect_err("observe-only lifecycle event must reject blocking mode");
        assert_eq!(invalid.code_enum(), Some(WireErrorCode::InvalidHookMode));

        let fixed = registry
            .register_hook(
                LifecycleEvent::AfterProviderResponse,
                HookMode::Advisory,
                handler,
            )
            .expect_err("fixed-mode lifecycle event must use its typed registration API");
        assert_eq!(fixed.code_enum(), Some(WireErrorCode::TypedHookRequired));
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

        let metadata = registry
            .catalog()
            .to_metadata_value("test-extension", "0.1.0")
            .unwrap();
        assert_eq!(
            metadata["hooks"],
            json!([
                { "on": "pre_compact", "mode": "blocking" },
                { "on": "post_compact", "mode": "blocking" }
            ])
        );
    }

    #[test]
    fn worker_context_validates_scoped_facts_before_author_code() {
        let scoped = WorkerCallFacts::from_event(
            "test-extension".into(),
            CancelToken::default(),
            &json!({
                "input": {
                    "session_id": "session-1",
                    "working_dir": "/workspace"
                }
            }),
        )
        .unwrap()
        .into_tool()
        .unwrap();
        assert_eq!(scoped.session_id(), "session-1");
        assert_eq!(scoped.working_dir(), Path::new("/workspace"));

        let unscoped = WorkerCallFacts::from_event(
            "test-extension".into(),
            CancelToken::default(),
            &Value::Null,
        )
        .unwrap();
        assert_eq!(
            unscoped.into_tool().err().unwrap().code_enum(),
            Some(WireErrorCode::ContextUnavailable)
        );

        let invalid = WorkerCallFacts::from_event(
            "test-extension".into(),
            CancelToken::default(),
            &json!({ "input": { "session_id": 7 } }),
        )
        .err()
        .unwrap();
        assert_eq!(invalid.code_enum(), Some(WireErrorCode::InvalidInput));
    }
}
