//! Worker 侧 handler 注册表。

use std::{
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};

use astrcode_extension_sdk::{
    config::ModelSelection,
    wire::manifest::{
        InitializeManifest, ManifestCommand, ManifestHook, ManifestHookEvent, ManifestHookOptions,
        ManifestHttpRoute, ManifestTool, ManifestToolMode,
    },
};
use serde::Deserialize;
use serde_json::Value;

use super::CancelToken;
use crate::{
    WireErrorCode,
    extension::{
        CompactEvent, ContinueAfterStopOptions, CustomEventDeclaration, CustomEventSubscription,
        ExtensionCapability, ExtensionHttpRequest, ExtensionHttpResponse, ExtensionHttpRoute,
        HookMode, LifecycleEvent,
        internal::{
            canonical_registration_name, extension_http_route_patterns_conflict, fixed_hook_mode,
            has_duplicate_registration_name, hook_mode_is_supported,
            normalize_custom_event_subscription, validate_custom_event_subscription,
            validate_extension_http_route,
        },
    },
    s5r::{
        ErrorPayload, HandlerEffect, HandlerId, HandlerInvokeRequest, HandlerKind, HandlerResult,
    },
};

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

pub type ToolHandlerFn = Arc<
    dyn Fn(Value, WorkerInvocationContext) -> BoxFuture<Result<HandlerResult, ErrorPayload>>
        + Send
        + Sync,
>;

pub type ToolPlannerFn = Arc<
    dyn Fn(Value, WorkerToolPlanContext) -> BoxFuture<Result<crate::tool::ToolPlan, ErrorPayload>>
        + Send
        + Sync,
>;

pub type HookHandlerFn = Arc<
    dyn Fn(Value, WorkerInvocationContext) -> BoxFuture<Result<HandlerResult, ErrorPayload>>
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
    dyn Fn(WorkerCommandContext) -> BoxFuture<Result<HandlerResult, ErrorPayload>> + Send + Sync,
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

/// Host-attributed facts guaranteed for a session/workspace worker invocation.
#[derive(Clone)]
pub struct WorkerInvocationContext {
    scoped: WorkerSessionWorkspaceContext,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
}

/// Side-effect-free facts exposed to a worker tool planner.
#[derive(Clone)]
pub struct WorkerToolPlanContext {
    extension_id: String,
    session_id: String,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
    working_dir: PathBuf,
    cancel_token: CancelToken,
}

impl WorkerToolPlanContext {
    pub fn extension_id(&self) -> &str {
        &self.extension_id
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    pub fn tool_call_id(&self) -> Option<&str> {
        self.tool_call_id.as_deref()
    }

    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    pub fn cancel_token(&self) -> &CancelToken {
        &self.cancel_token
    }
}

impl WorkerInvocationContext {
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
    command_name: String,
    argument: String,
    model: ModelSelection,
    invocation: WorkerCommandInvocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerCommandInvocation {
    Execute,
    Complete { cursor: usize },
}

#[derive(Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
enum WorkerCommandEvent {
    Command { input: WorkerCommandInput },
    CommandComplete { input: WorkerCommandCompletionInput },
}

impl WorkerCommandEvent {
    fn into_parts(self) -> (WorkerCommandInput, WorkerCommandInvocation) {
        match self {
            Self::Command { input } => (input, WorkerCommandInvocation::Execute),
            Self::CommandComplete { input } => (
                WorkerCommandInput {
                    command_name: input.command_name,
                    argument: input.argument,
                    model: input.model,
                },
                WorkerCommandInvocation::Complete {
                    cursor: input.cursor,
                },
            ),
        }
    }
}

#[derive(Deserialize)]
struct WorkerCommandInput {
    command_name: String,
    argument: String,
    model: ModelSelection,
}

#[derive(Deserialize)]
struct WorkerCommandCompletionInput {
    command_name: String,
    argument: String,
    cursor: usize,
    model: ModelSelection,
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

    pub fn command_name(&self) -> &str {
        &self.command_name
    }

    pub fn argument(&self) -> &str {
        &self.argument
    }

    pub fn model(&self) -> &ModelSelection {
        &self.model
    }

    pub const fn invocation(&self) -> WorkerCommandInvocation {
        self.invocation
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
        let input = event
            .get("input")
            .or_else(|| event.get("scope"))
            .unwrap_or(event);
        Ok(Self {
            call: WorkerCallContext {
                extension_id,
                cancel_token,
            },
            session_id: optional_string(input, "session_id")?.map(str::to_owned),
            turn_id: optional_string(input, "turn_id")?.map(str::to_owned),
            tool_call_id: optional_string(input, "tool_call_id")?.map(str::to_owned),
            working_dir: optional_string(input, "working_dir")?.map(PathBuf::from),
        })
    }

    pub(crate) fn into_invocation(
        self,
        scope: &'static str,
    ) -> Result<WorkerInvocationContext, ErrorPayload> {
        let (scoped, turn_id, tool_call_id) = self.into_scoped(scope)?;
        Ok(WorkerInvocationContext {
            scoped,
            turn_id,
            tool_call_id,
        })
    }

    fn into_tool_plan(self) -> Result<WorkerToolPlanContext, ErrorPayload> {
        let (scoped, turn_id, tool_call_id) = self.into_scoped("tool plan")?;
        Ok(WorkerToolPlanContext {
            extension_id: scoped.call.extension_id,
            session_id: scoped.session_id,
            turn_id,
            tool_call_id,
            working_dir: scoped.working_dir,
            cancel_token: scoped.call.cancel_token,
        })
    }

    fn into_command(
        self,
        event: Value,
        registered_name: &str,
    ) -> Result<WorkerCommandContext, ErrorPayload> {
        let (scoped, _, _) = self.into_scoped("command")?;
        let event = serde_json::from_value::<WorkerCommandEvent>(event).map_err(|error| {
            ErrorPayload::new(
                WireErrorCode::InvalidInput,
                format!("invalid command invocation: {error}"),
            )
        })?;
        let (input, invocation) = event.into_parts();
        if input.command_name != registered_name {
            return Err(ErrorPayload::new(
                WireErrorCode::InvalidInput,
                format!(
                    "command invocation name {} does not match handler {registered_name}",
                    input.command_name
                ),
            ));
        }
        Ok(WorkerCommandContext {
            scoped,
            command_name: input.command_name,
            argument: input.argument,
            model: input.model,
            invocation,
        })
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
    extension_id: String,
    manifest: InitializeManifest,
    tools: HashMap<String, RegisteredTool>,
    hooks: HashMap<String, HookHandlerFn>,
    continuation_hooks: HashMap<String, ContinuationHandlerFn>,
    custom_events: HashMap<String, CustomEventHandlerFn>,
    commands: HashMap<String, CommandHandlerFn>,
    http_routes: HashMap<String, HttpHandlerFn>,
}

struct RegisteredTool {
    planner: ToolPlannerFn,
    handler: ToolHandlerFn,
}

impl HandlerRegistry {
    pub fn new(extension_id: impl Into<String>) -> Self {
        Self {
            extension_id: extension_id.into(),
            manifest: InitializeManifest::default(),
            tools: HashMap::new(),
            hooks: HashMap::new(),
            continuation_hooks: HashMap::new(),
            custom_events: HashMap::new(),
            commands: HashMap::new(),
            http_routes: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn manifest(&self) -> &InitializeManifest {
        &self.manifest
    }

    pub(crate) fn take_manifest(&mut self) -> InitializeManifest {
        std::mem::take(&mut self.manifest)
    }

    pub(crate) fn extension_id(&self) -> &str {
        &self.extension_id
    }

    pub(crate) fn declare_capability(&mut self, cap: ExtensionCapability) {
        if !self.manifest.capabilities.contains(&cap) {
            self.manifest.capabilities.push(cap);
        }
    }

    pub(crate) fn declare_custom_event(&mut self, mut event: CustomEventDeclaration) {
        canonical_registration_name(&mut event.event_type);
        self.manifest.custom_events.push(event);
    }

    pub(crate) fn register_custom_event(
        &mut self,
        mut subscription: CustomEventSubscription,
        handler: CustomEventHandlerFn,
    ) -> Result<(), ErrorPayload> {
        normalize_custom_event_subscription(&mut subscription);
        if let Err(reason) = validate_custom_event_subscription(&subscription) {
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
        self.manifest.custom_event_subscriptions.push(subscription);
        Ok(())
    }

    pub(crate) fn register_tool(
        &mut self,
        mut def: crate::tool::ToolDefinition,
        planner: ToolPlannerFn,
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
        self.manifest.tools.push(ManifestTool {
            name: def.name,
            description: def.description,
            parameters: def.parameters,
            strict: def.strict,
            mode: match def.execution_mode {
                crate::tool::ExecutionMode::Parallel => ManifestToolMode::Parallel,
                crate::tool::ExecutionMode::Sequential => ManifestToolMode::Sequential,
            },
        });
        self.tools.insert(name, RegisteredTool { planner, handler });
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
                    on.as_str(),
                    required.as_str(),
                    fixed_worker_hook_hint(&on)
                ),
            ));
        }
        if !hook_mode_is_supported(&on, mode) {
            return Err(ErrorPayload::new(
                WireErrorCode::InvalidHookMode,
                format!("{} does not support {} mode", on.as_str(), mode.as_str()),
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
        self.register_manifest_hook_event(
            on.into(),
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
                format!("{} is not a fixed-mode hook", on.as_str()),
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
        self.register_manifest_hook_event(on.into(), mode, options, handler)
    }

    fn register_manifest_hook_event(
        &mut self,
        on: ManifestHookEvent,
        mode: HookMode,
        options: ManifestHookOptions,
        handler: HookHandlerFn,
    ) -> Result<(), ErrorPayload> {
        self.insert_hook_handler(on.as_str().to_owned(), handler)?;
        self.manifest.hooks.push(ManifestHook { on, mode, options });
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
        self.continuation_hooks.insert(on, handler);
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
        mut command: crate::extension::SlashCommand,
        handler: CommandHandlerFn,
    ) -> Result<(), ErrorPayload> {
        canonical_registration_name(&mut command.name);
        if has_duplicate_registration_name(self.commands.keys().map(String::as_str), &command.name)
        {
            return Err(ErrorPayload::new(
                WireErrorCode::DuplicateRegistration,
                format!("duplicate command registration: {}", command.name),
            ));
        }
        let name = command.name.clone();
        self.manifest.commands.push(ManifestCommand {
            name: command.name,
            description: command.description,
            args_schema: command.args_schema,
            requires_idle: command.requires_idle,
            argument_completions: command.argument_completions,
            priority: command.priority,
            availability: command.availability,
            execution: command.execution,
        });
        self.commands.insert(name, handler);
        Ok(())
    }

    pub(crate) fn register_http_route(
        &mut self,
        route: ExtensionHttpRoute,
        handler: HttpHandlerFn,
    ) -> Result<(), ErrorPayload> {
        validate_extension_http_route(&route)
            .map_err(|error| ErrorPayload::new(WireErrorCode::InvalidHttpRoute, error))?;
        if self.manifest.http_routes.iter().any(|entry| {
            entry.route.access == route.access
                && entry.route.method == route.method
                && extension_http_route_patterns_conflict(&entry.route.path, &route.path)
        }) {
            return Err(ErrorPayload::new(
                WireErrorCode::DuplicateRegistration,
                format!("conflicting HTTP route registration: {}", route.path),
            ));
        }
        let handler_name = format!("route_{}", self.manifest.http_routes.len());
        let handler_id = HandlerId::new(&self.extension_id, HandlerKind::Http, &handler_name)
            .map_err(|error| ErrorPayload::new(WireErrorCode::InvalidHookRegistration, error))?;
        self.manifest
            .http_routes
            .push(ManifestHttpRoute { route, handler_id });
        self.http_routes.insert(handler_name, handler);
        Ok(())
    }

    pub async fn dispatch_invoke(
        &self,
        input: Value,
        token: CancelToken,
    ) -> Result<HandlerResult, ErrorPayload> {
        if token.is_cancelled() {
            return Err(ErrorPayload::new(
                WireErrorCode::Cancelled,
                "handler invocation cancelled",
            ));
        }
        let request: HandlerInvokeRequest = serde_json::from_value(input).map_err(|error| {
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
        handler_id: &astrcode_extension_sdk::wire::HandlerId,
        event: Value,
        facts: WorkerCallFacts,
    ) -> Result<HandlerResult, ErrorPayload> {
        let (owner, kind, name) = handler_id.parts().ok_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::InvalidInput,
                format!("invalid handler id: {handler_id}"),
            )
        })?;
        if owner != self.extension_id {
            return Err(ErrorPayload::new(
                WireErrorCode::UnknownHandler,
                format!("unknown handler: {handler_id}"),
            ));
        }
        match kind {
            astrcode_extension_sdk::wire::HandlerKind::Tool => {
                let tool = self.tools.get(name).ok_or_else(|| {
                    ErrorPayload::new(
                        WireErrorCode::UnknownHandler,
                        format!("unknown tool: {name}"),
                    )
                })?;
                let invocation = serde_json::from_value::<crate::s5r::ToolInvocationRequest>(event)
                    .map_err(|error| {
                        ErrorPayload::new(
                            WireErrorCode::InvalidInput,
                            format!("invalid tool invocation: {error}"),
                        )
                    })?;
                match invocation.phase {
                    crate::s5r::ToolInvocationPhase::Plan => {
                        let plan =
                            (tool.planner)(invocation.arguments, facts.into_tool_plan()?).await?;
                        let data = serde_json::to_value(crate::s5r::ToolPlanDto::from(&plan))
                            .map_err(|error| {
                                ErrorPayload::new(
                                    WireErrorCode::SerializationFailed,
                                    format!("serialize tool plan: {error}"),
                                )
                            })?;
                        Ok(HandlerResult::effect(HandlerEffect::ToolPlan, data))
                    },
                    crate::s5r::ToolInvocationPhase::Execute => {
                        (tool.handler)(invocation.arguments, facts.into_invocation("tool")?).await
                    },
                }
            },
            astrcode_extension_sdk::wire::HandlerKind::Hook => {
                if let Some(handler) = self.hooks.get(name) {
                    handler(event, facts.into_invocation("hook")?).await
                } else if let Some(handler) = self.continuation_hooks.get(name) {
                    handler(event, facts.call).await
                } else {
                    Err(ErrorPayload::new(
                        WireErrorCode::UnknownHandler,
                        format!("unknown hook: {name}"),
                    ))
                }
            },
            astrcode_extension_sdk::wire::HandlerKind::Command => {
                let handler = self.commands.get(name).ok_or_else(|| {
                    ErrorPayload::new(
                        WireErrorCode::UnknownHandler,
                        format!("unknown command: {name}"),
                    )
                })?;
                handler(facts.into_command(event, name)?).await
            },
            astrcode_extension_sdk::wire::HandlerKind::Http => {
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
                Ok(HandlerResult::effect(HandlerEffect::HttpResponse, data))
            },
            astrcode_extension_sdk::wire::HandlerKind::Event => {
                let handler = self.custom_events.get(name).ok_or_else(|| {
                    ErrorPayload::new(
                        WireErrorCode::UnknownHandler,
                        format!("unknown custom event subscription: {name}"),
                    )
                })?;
                handler(event, facts.into_custom_event()?).await
            },
        }
    }
}

fn fixed_worker_hook_hint(event: &LifecycleEvent) -> &'static str {
    match event {
        LifecycleEvent::AfterProviderResponse => {
            "use Worker::on_after_provider_response(...) instead"
        },
        LifecycleEvent::ProviderContribution => "use Worker::on_provider_contribution(...) instead",
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
                    Ok(HandlerResult::effect(HandlerEffect::Ok, json!({"step": 1})))
                }),
            )
            .unwrap();

        assert!(registry.manifest().hooks.is_empty());

        let result = registry
            .dispatch_invoke(
                json!({
                    "handler_id": "test-extension:hook:pipeline_step",
                    "event": {}
                }),
                CancelToken::default(),
            )
            .await
            .unwrap();

        assert_eq!(result.effect, HandlerEffect::Ok);
        assert_eq!(result.data.get("step"), Some(&json!(1)));

        let error = registry
            .dispatch_invoke(
                json!({
                    "handler_id": "test-extension:hook:pipeline_step",
                    "event": {},
                    "unknown": true
                }),
                CancelToken::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code_enum(), Some(WireErrorCode::InvalidInput));
    }

    #[test]
    fn lifecycle_hook_modes_are_validated_and_preserved() {
        let mut registry = HandlerRegistry::new("test-extension");
        let handler = crate::worker::hook_handler(|_| async {
            Ok(HandlerResult::effect(HandlerEffect::Ok, json!({})))
        });

        registry
            .register_hook(
                LifecycleEvent::TurnEnd,
                HookMode::NonBlocking,
                Arc::clone(&handler),
            )
            .unwrap();
        assert_eq!(registry.manifest.hooks[0].mode, HookMode::NonBlocking);

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

        assert_eq!(registry.manifest.http_routes.len(), 2);
        assert_eq!(
            error.code_enum(),
            Some(WireErrorCode::DuplicateRegistration)
        );
    }

    #[test]
    fn worker_registration_names_use_the_same_canonical_form_as_the_host() {
        let mut registry = HandlerRegistry::new("test-extension");
        let tool_handler = crate::worker::tool_handler(|_| async {
            Ok(HandlerResult::effect(HandlerEffect::Ok, json!({})))
        });
        let tool_planner =
            crate::worker::tool_planner(|_| async { Ok(crate::tool::ToolPlan::default()) });
        registry
            .register_tool(
                crate::builder::worker_tool("  review  ")
                    .strict()
                    .execution_mode(crate::tool::ExecutionMode::Sequential)
                    .build()
                    .into(),
                Arc::clone(&tool_planner),
                Arc::clone(&tool_handler),
            )
            .unwrap();
        registry
            .register_command(
                crate::builder::command("  inspect  ")
                    .description("Inspect state")
                    .arguments(json!({ "type": "string" }))
                    .requires_idle(true)
                    .argument_completions(true)
                    .priority(17)
                    .build(),
                crate::worker::command_handler(|_| async {
                    Ok(HandlerResult::effect(HandlerEffect::Ok, json!({})))
                }),
            )
            .unwrap();
        registry.declare_custom_event(CustomEventDeclaration {
            event_type: "  review.completed  ".into(),
            schema_version: 1,
            durable: true,
            max_payload_bytes: 1024,
        });

        assert_eq!(registry.manifest.tools[0].name, "review");
        assert!(registry.manifest.tools[0].strict);
        assert_eq!(
            registry.manifest.tools[0].mode,
            ManifestToolMode::Sequential
        );
        assert_eq!(registry.manifest.commands[0].name, "inspect");
        assert_eq!(
            registry.manifest.commands[0].args_schema,
            Some(json!({ "type": "string" }))
        );
        assert!(registry.manifest.commands[0].requires_idle);
        assert!(registry.manifest.commands[0].argument_completions);
        assert_eq!(registry.manifest.commands[0].priority, 17);
        assert_eq!(
            registry.manifest.commands[0].availability,
            crate::extension::CommandAvailability::AllTransports
        );
        assert_eq!(
            registry.manifest.commands[0].execution,
            crate::extension::CommandExecution::Extension
        );
        assert_eq!(
            registry.manifest.custom_events[0].event_type,
            "review.completed"
        );
        assert_eq!(
            registry
                .register_tool(
                    crate::builder::worker_tool("review").build().into(),
                    tool_planner,
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
        let handler = crate::worker::hook_handler(|_| async {
            Ok(HandlerResult::effect(HandlerEffect::Ok, json!({})))
        });

        registry
            .register_compact_hook(CompactEvent::PreCompact, Arc::clone(&handler))
            .unwrap();
        registry
            .register_compact_hook(CompactEvent::PostCompact, handler)
            .unwrap();

        let manifest = registry.manifest();
        assert_eq!(
            serde_json::to_value(&manifest.hooks).unwrap(),
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
        .into_invocation("tool")
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
            unscoped.into_invocation("tool").err().unwrap().code_enum(),
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
