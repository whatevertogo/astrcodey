//! 磁盘 S5R 3.0 子进程扩展。

mod session_support;
mod v3_session;

use std::{path::Path, sync::Arc};

use astrcode_extension_sdk::{
    builder::manifest,
    extension::{
        CommandContext, CommandHandler, CompactContext, CompactHandler, CompactResult,
        ContinueAfterStopContext, ContinueAfterStopHandler, ContinueAfterStopResult,
        CustomEventContext, CustomEventDisposition, CustomEventHandler, Extension, ExtensionCall,
        ExtensionCallContext, ExtensionCommandResult, ExtensionError, ExtensionHttpHandler,
        ExtensionHttpResponse, ExtensionPackageManifest, ExtensionStartContext,
        ExtensionStopContext, HookResult, HttpContext, LifecycleContext, LifecycleEvent,
        LifecycleHandler, PostToolUseContext, PostToolUseHandler, PostToolUseResult,
        PreToolUseContext, PreToolUseHandler, PreToolUseResult, PromptBuildContext,
        PromptBuildHandler, PromptContributions, ProviderContext, ProviderHandler, ProviderResult,
        Registrar, ToolContext, ToolHandler, ToolPlanContext,
    },
    s5r::{ToolInvocationPhase, ToolInvocationRequest, ToolInvocationScope, ToolPlanDto},
    tool::{ExecutionMode, ToolPlan},
    wire::{HandlerEffect, HandlerId, HandlerKind, HandlerResult},
};
use serde_json::{Value, json};

use crate::{
    extension_manifest::HookSubscription,
    host_router::{HostRouter, InvokeContext},
    s5r_ext::v3_session::S5rV3Session as S5rSession,
    s5r_handler::{
        handler_id, parse_command_result, parse_compact_result, parse_continue_after_stop_result,
        parse_http_response, parse_lifecycle_result, parse_post_tool_use_result,
        parse_pre_tool_use_result, parse_prompt_build_result, parse_provider_result,
        parse_tool_result,
    },
};

pub struct S5rExtension {
    session: Arc<S5rSession>,
}

impl S5rExtension {
    pub async fn load(
        ext_dir: &Path,
        manifest: &ExtensionPackageManifest,
        host_router: Arc<HostRouter>,
    ) -> Result<Arc<Self>, String> {
        let (program, args) = parse_command(manifest, ext_dir)?;
        let env = parse_env(manifest);
        let session = S5rSession::spawn(
            &program,
            &args,
            ext_dir,
            &env,
            &manifest.extension_id,
            host_router,
        )
        .await?;
        Ok(Arc::new(Self { session }))
    }
}

pub(crate) fn parse_command(
    manifest: &ExtensionPackageManifest,
    ext_dir: &Path,
) -> Result<(String, Vec<String>), String> {
    if manifest.command.is_empty() {
        return Err("'command' must contain at least the executable path".into());
    }
    let program = manifest.command[0].clone();
    let program_path = Path::new(&program);
    let program = if program_path.is_absolute() {
        program
    } else if program.contains('/') || program.contains('\\') {
        ext_dir.join(program_path).to_string_lossy().into_owned()
    } else {
        program
    };
    let args = manifest.command[1..].to_vec();
    Ok((program, args))
}

fn parse_env(manifest: &ExtensionPackageManifest) -> Vec<(String, String)> {
    manifest
        .env
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

#[async_trait::async_trait]
impl Extension for S5rExtension {
    fn manifest(&self) -> astrcode_extension_sdk::extension::ExtensionManifest {
        let registration = self.session.registration();
        registration
            .capabilities
            .iter()
            .copied()
            .fold(
                manifest(&registration.extension_id)
                    .version(&registration.version)
                    .description("External S5R extension"),
                |builder, capability| builder.capability(capability),
            )
            .build()
    }

    fn register(&self, reg: &mut Registrar) {
        let registration = self.session.registration();
        for decl in &registration.custom_events {
            reg.declare_custom_event(decl.clone());
        }
        for subscription in &registration.custom_event_subscriptions {
            reg.on_custom_event(
                subscription.clone(),
                0,
                Arc::new(S5rCustomEventHandler {
                    session: Arc::clone(&self.session),
                    ext_id: registration.extension_id.clone(),
                    subscription_id: subscription.id.clone(),
                }),
            );
        }
        for tool_def in &registration.tools {
            reg.tool(
                tool_def.clone(),
                Arc::new(S5rToolHandler {
                    session: Arc::clone(&self.session),
                    extension_id: registration.extension_id.clone(),
                    execution_mode: tool_def.execution_mode,
                }),
            );
        }
        for cmd in &registration.commands {
            reg.command(
                cmd.clone(),
                Arc::new(S5rCommandHandler {
                    session: Arc::clone(&self.session),
                    extension_id: registration.extension_id.clone(),
                }),
            );
        }
        for entry in &registration.http_routes {
            reg.http_route(
                entry.route.clone(),
                Arc::new(S5rHttpHandler {
                    session: Arc::clone(&self.session),
                    handler_id: entry.handler_id.clone(),
                }),
            );
        }
        for subscription in &registration.subscriptions {
            let event_name = subscription.event_name();
            let session = Arc::clone(&self.session);
            let ext_id = registration.extension_id.clone();
            match subscription {
                HookSubscription::Compact(event) => {
                    reg.on_compact(
                        *event,
                        0,
                        Arc::new(S5rCompactHandler {
                            session,
                            ext_id,
                            on: event_name.into(),
                        }),
                    );
                },
                HookSubscription::Lifecycle {
                    event,
                    mode,
                    options,
                } => match event {
                    LifecycleEvent::PreToolUse => {
                        reg.on_pre_tool_use(
                            *mode,
                            0,
                            Arc::new(S5rPreToolUseHandler {
                                session,
                                ext_id,
                                on: event_name.into(),
                            }),
                        );
                    },
                    LifecycleEvent::PostToolUse => {
                        reg.on_post_tool_use(
                            *mode,
                            0,
                            Arc::new(S5rPostToolUseHandler {
                                session,
                                ext_id,
                                on: event_name.into(),
                            }),
                        );
                    },
                    LifecycleEvent::BeforeProviderRequest => {
                        reg.on_before_provider_request(
                            *mode,
                            0,
                            Arc::new(S5rProviderHandler {
                                session,
                                ext_id,
                                on: event_name.into(),
                            }),
                        );
                    },
                    LifecycleEvent::AfterProviderResponse => {
                        reg.on_after_provider_response(
                            0,
                            Arc::new(S5rProviderHandler {
                                session,
                                ext_id,
                                on: event_name.into(),
                            }),
                        );
                    },
                    LifecycleEvent::ContinueAfterStop => {
                        reg.on_continue_after_stop(
                            0,
                            *options,
                            Arc::new(S5rContinueAfterStopHandler {
                                session,
                                ext_id,
                                on: event_name.into(),
                            }),
                        );
                    },
                    LifecycleEvent::PromptBuild => {
                        reg.on_prompt_build(
                            0,
                            Arc::new(S5rPromptBuildHandler {
                                session,
                                ext_id,
                                on: event_name.into(),
                            }),
                        );
                    },
                    LifecycleEvent::UserMessageEnvelope => {
                        tracing::warn!(
                            extension_id = %ext_id,
                            hook = event_name,
                            "s5r manifest requested an internal typed decision hook; ignoring"
                        );
                    },
                    other => {
                        reg.on_lifecycle(
                            other.clone(),
                            *mode,
                            0,
                            Arc::new(S5rLifecycleHandler {
                                session,
                                ext_id,
                                on: event_name.into(),
                            }),
                        );
                    },
                },
            }
        }
    }

    async fn start(&self, ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        let invoke_context =
            crate::runner::transport_invoke_context(ctx.host()).ok_or_else(|| {
                ExtensionError::Internal(
                    "s5r startup requires the runner-provided transport context".to_owned(),
                )
            })?;
        self.session.set_detached_invoke_context(invoke_context);
        self.session.activate().await
    }

    async fn stop(&self, _ctx: ExtensionStopContext) -> Result<(), ExtensionError> {
        self.session.shutdown().await;
        Ok(())
    }

    async fn health(&self) -> Result<(), ExtensionError> {
        self.session.ping().await
    }
}

struct S5rHttpHandler {
    session: Arc<S5rSession>,
    handler_id: HandlerId,
}

#[async_trait::async_trait]
impl ExtensionHttpHandler for S5rHttpHandler {
    async fn handle(&self, ctx: HttpContext) -> Result<ExtensionHttpResponse, ExtensionError> {
        let invoke_ctx = require_transport_invoke_ctx(ctx.call())?;
        let event = serde_json::to_value(ctx.request()).map_err(|error| {
            ExtensionError::Internal(format!("serialize HTTP request: {error}"))
        })?;
        let response = self
            .session
            .invoke_handler(&self.handler_id, event, &invoke_ctx)
            .await?;
        parse_http_response(&response)
    }
}

fn require_transport_invoke_ctx(
    call: &ExtensionCallContext,
) -> Result<InvokeContext, ExtensionError> {
    crate::runner::transport_invoke_context(call.host()).ok_or_else(|| {
        ExtensionError::Internal(
            "s5r invocation requires the runner-provided transport context".to_owned(),
        )
    })
}

async fn invoke_hook(
    session: &S5rSession,
    extension_id: &str,
    hook_name: &str,
    invoke_context: &InvokeContext,
    input: serde_json::Value,
) -> Result<HandlerResult, ExtensionError> {
    let handler = handler_id(extension_id, HandlerKind::Hook, hook_name)?;
    session
        .invoke_handler_with_continuations(
            &handler,
            json!({ "on": hook_name, "input": input }),
            invoke_context,
            ExecutionMode::Sequential,
        )
        .await
}

/// Generates one S5R hook adapter: a handler struct that serializes the typed hook context
/// to the wire input, invokes the subprocess hook named by `on`, and parses its result.
macro_rules! s5r_hook_handler {
    ($handler:ident, $trait:ident, $ctx:ty, $output:ty, | $c:ident | $input:expr, $parse:ident) => {
        struct $handler {
            session: Arc<S5rSession>,
            ext_id: String,
            on: String,
        }

        #[async_trait::async_trait]
        impl $trait for $handler {
            async fn handle(&self, $c: $ctx) -> Result<$output, ExtensionError> {
                let invoke_ctx = require_transport_invoke_ctx($c.call())?;
                let input = $input;
                let resp =
                    invoke_hook(&self.session, &self.ext_id, &self.on, &invoke_ctx, input).await?;
                $parse(&resp)
            }
        }
    };
}

struct S5rToolHandler {
    session: Arc<S5rSession>,
    extension_id: String,
    execution_mode: ExecutionMode,
}

fn serialize_tool_invocation(request: ToolInvocationRequest) -> Result<Value, ExtensionError> {
    serde_json::to_value(request)
        .map_err(|error| ExtensionError::Internal(format!("serialize tool invocation: {error}")))
}

#[async_trait::async_trait]
impl ToolHandler for S5rToolHandler {
    async fn plan(&self, ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        let tool_name = ctx.tool_name().to_owned();
        let event = serialize_tool_invocation(ToolInvocationRequest {
            phase: ToolInvocationPhase::Plan,
            arguments: ctx.raw_arguments().clone(),
            scope: ToolInvocationScope {
                session_id: ctx.session_id().to_string(),
                working_dir: ctx.working_dir().to_string_lossy().into_owned(),
                turn_id: ctx.turn_id().map(str::to_owned),
                tool_call_id: ctx.call_id().map(str::to_owned),
            },
        })?;
        let invoke_context = InvokeContext {
            extension_id: self.extension_id.clone(),
            session_id: Some(ctx.session_id().to_string()),
            tool_call_id: ctx.call_id().map(str::to_owned),
            working_dir: Some(ctx.working_dir().to_string_lossy().into_owned()),
            cancel_token: Some(ctx.cancellation().clone()),
            planning: true,
            ..InvokeContext::default()
        };
        let handler = handler_id(&self.extension_id, HandlerKind::Tool, &tool_name)?;
        let response = self
            .session
            .invoke_handler_once(&handler, event, &invoke_context, self.execution_mode)
            .await?;
        if response.effect != HandlerEffect::ToolPlan {
            return Err(ExtensionError::Internal(format!(
                "tool planner returned {:?}, expected tool_plan",
                response.effect
            )));
        }
        serde_json::from_value::<ToolPlanDto>(response.data)
            .map(ToolPlan::from)
            .map_err(|error| ExtensionError::Internal(format!("parse tool plan: {error}")))
    }

    async fn execute(
        &self,
        ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        let tool_name = ctx.tool_name().to_owned();
        let arguments = ctx.raw_arguments().clone();
        let session_id = ctx.session_id().to_string();
        let working_dir = ctx.working_dir().to_string_lossy().into_owned();
        let tool_call_id = ctx.call_id().map(str::to_owned);
        let turn_id = ctx.turn_id().map(str::to_owned);
        let invoke_ctx = require_transport_invoke_ctx(ctx.call())?;
        let event = serialize_tool_invocation(ToolInvocationRequest {
            phase: ToolInvocationPhase::Execute,
            arguments,
            scope: ToolInvocationScope {
                session_id,
                working_dir,
                turn_id,
                tool_call_id,
            },
        })?;
        let hid = handler_id(&self.extension_id, HandlerKind::Tool, &tool_name)?;
        let resp = self
            .session
            .invoke_handler_with_continuations(&hid, event, &invoke_ctx, self.execution_mode)
            .await?;
        parse_tool_result(&resp).map(Into::into)
    }
}

struct S5rCommandHandler {
    session: Arc<S5rSession>,
    extension_id: String,
}

#[async_trait::async_trait]
impl CommandHandler for S5rCommandHandler {
    async fn execute(&self, ctx: CommandContext) -> Result<ExtensionCommandResult, ExtensionError> {
        let invoke_ctx = require_transport_invoke_ctx(ctx.call())?;
        let event = json!({
            "on": "command",
            "name": ctx.command_name(),
            "input": {
                "command_name": ctx.command_name(),
                "arguments": ctx.argument(),
                "working_dir": ctx.working_dir().display().to_string(),
                "session_id": ctx.session_id().to_string(),
                "model": ctx.model(),
            }
        });
        let hid = handler_id(&self.extension_id, HandlerKind::Command, ctx.command_name())?;
        let resp = self
            .session
            .invoke_handler_with_continuations(&hid, event, &invoke_ctx, ExecutionMode::Sequential)
            .await?;
        parse_command_result(&resp)
    }
}

s5r_hook_handler!(
    S5rPreToolUseHandler,
    PreToolUseHandler,
    PreToolUseContext,
    PreToolUseResult,
    |ctx| json!({
        "session_id": ctx.session_id().to_string(),
        "working_dir": ctx.working_dir().display().to_string(),
        "model": ctx.model(),
        "tool_call_id": ctx.call_id(),
        "tool_name": ctx.tool_name(),
        "tool_input": ctx.tool_input(),
        "available_tools": ctx.available_tools(),
    }),
    parse_pre_tool_use_result
);

s5r_hook_handler!(
    S5rPostToolUseHandler,
    PostToolUseHandler,
    PostToolUseContext,
    PostToolUseResult,
    |ctx| json!({
        "session_id": ctx.session_id().to_string(),
        "working_dir": ctx.working_dir().display().to_string(),
        "model": ctx.model(),
        "tool_call_id": ctx.call_id(),
        "tool_name": ctx.tool_name(),
        "tool_input": ctx.tool_input(),
        "tool_result": ctx.tool_result(),
        "is_error": ctx.tool_result().is_error,
    }),
    parse_post_tool_use_result
);

s5r_hook_handler!(
    S5rProviderHandler,
    ProviderHandler,
    ProviderContext,
    ProviderResult,
    |ctx| json!({
        "session_id": ctx.session_id().to_string(),
        "working_dir": ctx.working_dir().display().to_string(),
        "model": ctx.model(),
        "messages": ctx.messages(),
    }),
    parse_provider_result
);

s5r_hook_handler!(
    S5rContinueAfterStopHandler,
    ContinueAfterStopHandler,
    ContinueAfterStopContext,
    ContinueAfterStopResult,
    |ctx| json!({
        "session_id": ctx.session_id().to_string(),
        "working_dir": ctx.working_dir().display().to_string(),
        "model": ctx.model(),
        "assistant_text": ctx.assistant_text(),
        "finish_reason": ctx.finish_reason(),
        "continuations_this_turn": ctx.continuations_this_turn(),
    }),
    parse_continue_after_stop_result
);

s5r_hook_handler!(
    S5rPromptBuildHandler,
    PromptBuildHandler,
    PromptBuildContext,
    PromptContributions,
    |ctx| json!({
        "session_id": ctx.session_id().to_string(),
        "working_dir": ctx.working_dir().display().to_string(),
        "model": ctx.model(),
    }),
    parse_prompt_build_result
);

s5r_hook_handler!(
    S5rCompactHandler,
    CompactHandler,
    CompactContext,
    CompactResult,
    |ctx| json!({
        "session_id": ctx.session_id().to_string(),
        "working_dir": ctx.working_dir().display().to_string(),
        "model": ctx.model(),
        "trigger": ctx.trigger(),
        "message_count": ctx.message_count(),
        "pre_tokens": ctx.pre_tokens(),
        "post_tokens": ctx.post_tokens(),
        "summary": ctx.summary(),
    }),
    parse_compact_result
);

s5r_hook_handler!(
    S5rLifecycleHandler,
    LifecycleHandler,
    LifecycleContext,
    HookResult,
    |ctx| json!({
        "session_id": ctx.session_id().to_string(),
        "working_dir": ctx.working_dir().display().to_string(),
        "model": ctx.model(),
        "mid_turn_user_messages_synced": ctx.mid_turn_user_messages_synced(),
    }),
    parse_lifecycle_result
);

struct S5rCustomEventHandler {
    session: Arc<S5rSession>,
    ext_id: String,
    subscription_id: String,
}

#[async_trait::async_trait]
impl CustomEventHandler for S5rCustomEventHandler {
    async fn handle(
        &self,
        ctx: CustomEventContext,
    ) -> Result<CustomEventDisposition, ExtensionError> {
        let invoke_ctx = require_transport_invoke_ctx(ctx.call())?;
        let handler = handler_id(&self.ext_id, HandlerKind::Event, &self.subscription_id)?;
        let result = self
            .session
            .invoke_handler_with_continuations(
                &handler,
                json!({
                    "input": {
                        "event_id": ctx.event_id(),
                        "session_id": ctx.session_id(),
                        "turn_id": ctx.turn_id(),
                        "seq": ctx.seq(),
                        "source_extension_id": ctx.source_extension_id(),
                        "event_type": ctx.event_type(),
                        "schema_version": ctx.schema_version(),
                        "causation_id": ctx.causation_id(),
                        "cascade_depth": ctx.cascade_depth(),
                        "durable": ctx.is_durable(),
                        "payload": ctx.payload(),
                    }
                }),
                &invoke_ctx,
                ExecutionMode::Sequential,
            )
            .await?;
        let reason = || {
            result
                .data
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("custom event handler requested redelivery")
                .to_owned()
        };
        match result.effect {
            HandlerEffect::CustomEventAck => Ok(CustomEventDisposition::Ack),
            HandlerEffect::CustomEventRetry => Ok(CustomEventDisposition::retry(reason())),
            HandlerEffect::CustomEventDeadLetter => {
                Ok(CustomEventDisposition::dead_letter(reason()))
            },
            effect => Err(ExtensionError::Internal(format!(
                "unexpected {effect:?} effect from custom event handler"
            ))),
        }
    }
}
