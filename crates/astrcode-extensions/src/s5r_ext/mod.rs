//! 磁盘 s5r 子进程扩展：stdio 长度前缀帧 + WireMessage。

mod session;

use std::{path::Path, sync::Arc};

use astrcode_extension_sdk::{
    builder::manifest,
    extension::{
        CommandContext, CommandHandler, CompactContext, CompactEvent, CompactHandler,
        CompactResult, ContinueAfterStopContext, ContinueAfterStopHandler,
        ContinueAfterStopOptions, ContinueAfterStopResult, Extension, ExtensionCallContext,
        ExtensionCapability, ExtensionCommandResult, ExtensionError, ExtensionEvent,
        ExtensionEventDecl, ExtensionHttpHandler, ExtensionHttpResponse, ExtensionPackageManifest,
        ExtensionStartContext, HookMode, HookResult, HttpContext, LifecycleContext,
        LifecycleHandler, PostToolUseContext, PostToolUseHandler, PostToolUseResult,
        PreToolUseContext, PreToolUseHandler, PreToolUseResult, PromptBuildContext,
        PromptBuildHandler, PromptContributions, ProviderContext, ProviderHandler, ProviderResult,
        Registrar, SlashCommand, StopReason, ToolContext, ToolHandler,
    },
    s5r::{effects::HandlerResult, event_to_name},
    tool::{ExecutionMode, ToolDefinition},
};
use serde_json::json;

use crate::{
    extension_manifest::{ExtensionRegistration, RegisteredHttpRoute},
    host_router::{HostRouter, InvokeContext},
    remote_manifest::{
        handler_id, parse_command_result, parse_compact_result, parse_continue_after_stop_result,
        parse_http_response, parse_lifecycle_result, parse_post_tool_use_result,
        parse_pre_tool_use_result, parse_prompt_build_result, parse_provider_result,
        parse_tool_result,
    },
    s5r_ext::session::S5rSession,
};

pub struct S5rExtension {
    id: String,
    version: String,
    capabilities: Vec<ExtensionCapability>,
    session: Arc<S5rSession>,
    event_decls: Vec<ExtensionEventDecl>,
    tools: Vec<ToolDefinition>,
    commands: Vec<SlashCommand>,
    subscriptions: Vec<(ExtensionEvent, HookMode, ContinueAfterStopOptions)>,
    http_routes: Vec<RegisteredHttpRoute>,
}

impl S5rExtension {
    pub async fn load(
        ext_dir: &Path,
        manifest: &ExtensionPackageManifest,
        host_router: Arc<HostRouter>,
    ) -> Result<Arc<Self>, String> {
        let (program, args) = parse_command(manifest, ext_dir)?;
        let env = parse_env(manifest);
        let session = S5rSession::spawn(&program, &args, ext_dir, &env, host_router).await?;
        let registration = session
            .registration()
            .ok_or("s5r extension did not complete initialize handshake")?;
        if registration.extension_id() != manifest.extension_id {
            let actual_id = registration.extension_id().to_owned();
            session.shutdown().await;
            return Err(format!(
                "extension id mismatch: extension.json declares {:?}, initialize declares \
                 {actual_id:?}",
                manifest.extension_id
            ));
        }
        Ok(build_extension(session, registration))
    }
}

fn build_extension(
    session: Arc<S5rSession>,
    registration: ExtensionRegistration,
) -> Arc<S5rExtension> {
    Arc::new(S5rExtension {
        id: registration.extension_id().to_owned(),
        version: registration.version().to_owned(),
        capabilities: registration.capabilities().to_vec(),
        session,
        event_decls: registration.extension_events().to_vec(),
        tools: registration.tools().to_vec(),
        commands: registration.commands().to_vec(),
        subscriptions: registration.subscriptions().to_vec(),
        http_routes: registration.http_routes().to_vec(),
    })
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
        self.capabilities
            .iter()
            .copied()
            .fold(
                manifest(self.id.clone())
                    .version(self.version.clone())
                    .description("External S5R extension"),
                |builder, capability| builder.capability(capability),
            )
            .build()
    }

    fn register(&self, reg: &mut Registrar) {
        for decl in &self.event_decls {
            reg.declare_event(decl.clone());
        }
        for tool_def in &self.tools {
            reg.tool(
                tool_def.clone(),
                Arc::new(S5rToolHandler {
                    session: Arc::clone(&self.session),
                    extension_id: self.id.clone(),
                    execution_mode: tool_def.execution_mode,
                }),
            );
        }
        for cmd in &self.commands {
            reg.command(
                cmd.clone(),
                Arc::new(S5rCommandHandler {
                    session: Arc::clone(&self.session),
                    extension_id: self.id.clone(),
                }),
            );
        }
        for entry in &self.http_routes {
            reg.http_route(
                entry.route.clone(),
                Arc::new(S5rHttpHandler {
                    session: Arc::clone(&self.session),
                    handler_id: entry.handler_id.clone(),
                }),
            );
        }
        for (event, mode, options) in &self.subscriptions {
            let session = Arc::clone(&self.session);
            let ext_id = self.id.clone();
            match event {
                ExtensionEvent::PreToolUse => {
                    reg.on_pre_tool_use(
                        *mode,
                        0,
                        Arc::new(S5rPreToolUseHandler { session, ext_id }),
                    );
                },
                ExtensionEvent::PostToolUse => {
                    reg.on_post_tool_use(
                        *mode,
                        0,
                        Arc::new(S5rPostToolUseHandler { session, ext_id }),
                    );
                },
                ExtensionEvent::BeforeProviderRequest => {
                    reg.on_before_provider_request(
                        *mode,
                        0,
                        Arc::new(S5rProviderHandler {
                            session,
                            ext_id,
                            on: "before_provider_request".into(),
                        }),
                    );
                },
                ExtensionEvent::AfterProviderResponse => {
                    reg.on_after_provider_response(
                        0,
                        Arc::new(S5rProviderHandler {
                            session,
                            ext_id,
                            on: "after_provider_response".into(),
                        }),
                    );
                },
                ExtensionEvent::ContinueAfterStop => {
                    reg.on_continue_after_stop(
                        0,
                        *options,
                        Arc::new(S5rContinueAfterStopHandler { session, ext_id }),
                    );
                },
                ExtensionEvent::PromptBuild => {
                    reg.on_prompt_build(0, Arc::new(S5rPromptBuildHandler { session, ext_id }));
                },
                ExtensionEvent::UserMessageEnvelope => {
                    tracing::warn!(
                        extension_id = %ext_id,
                        hook = event_to_name(event),
                        "s5r manifest requested an internal typed decision hook; ignoring"
                    );
                },
                ExtensionEvent::PreCompact => {
                    reg.on_compact(
                        CompactEvent::PreCompact,
                        0,
                        Arc::new(S5rCompactHandler {
                            session,
                            ext_id,
                            on: "pre_compact".into(),
                        }),
                    );
                },
                ExtensionEvent::PostCompact => {
                    reg.on_compact(
                        CompactEvent::PostCompact,
                        0,
                        Arc::new(S5rCompactHandler {
                            session,
                            ext_id,
                            on: "post_compact".into(),
                        }),
                    );
                },
                other => {
                    let on = event_to_name(other).to_string();
                    reg.on_lifecycle(
                        other.clone(),
                        *mode,
                        0,
                        Arc::new(S5rLifecycleHandler {
                            session,
                            ext_id,
                            on,
                        }),
                    );
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
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) -> Result<(), ExtensionError> {
        self.session.shutdown().await;
        Ok(())
    }

    async fn health(&self) -> Result<(), ExtensionError> {
        self.session
            .ping()
            .await
            .map_err(|e| ExtensionError::Internal(e.to_string()))
    }
}

struct S5rHttpHandler {
    session: Arc<S5rSession>,
    handler_id: String,
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
    let handler = handler_id(extension_id, "hook", hook_name);
    session
        .invoke_handler_with_continuations(
            &handler,
            json!({ "on": hook_name, "input": input }),
            invoke_context,
            ExecutionMode::Sequential,
        )
        .await
}

struct S5rToolHandler {
    session: Arc<S5rSession>,
    extension_id: String,
    execution_mode: ExecutionMode,
}

#[async_trait::async_trait]
impl ToolHandler for S5rToolHandler {
    async fn execute(
        &self,
        ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        let tool_name = ctx.tool_name().to_owned();
        let arguments = ctx.raw_arguments().clone();
        let session_id = ctx
            .session_id()
            .ok_or_else(|| ExtensionError::Internal("s5r tool requires a session id".into()))?
            .to_string();
        let working_dir = ctx
            .working_dir()
            .ok_or_else(|| {
                ExtensionError::Internal("s5r tool requires a working directory".into())
            })?
            .to_string_lossy()
            .into_owned();
        let tool_call_id = ctx.call_id().map(str::to_owned);
        let turn_id = ctx.turn_id().map(str::to_owned);
        let invoke_ctx = require_transport_invoke_ctx(ctx.call())?;
        let event = json!({
            "on": "tool",
            "name": &tool_name,
            "input": {
                "tool_name": &tool_name,
                "arguments": arguments,
                "working_dir": working_dir,
                "session_id": session_id,
                "turn_id": turn_id,
                "tool_call_id": tool_call_id,
            }
        });
        let hid = handler_id(&self.extension_id, "tool", &tool_name);
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
                "working_dir": ctx.working_dir().map(|path| path.display().to_string()),
                "session_id": ctx.session_id().map(ToString::to_string),
                "model": ctx.model(),
            }
        });
        let hid = handler_id(&self.extension_id, "command", ctx.command_name());
        let resp = self
            .session
            .invoke_handler_with_continuations(&hid, event, &invoke_ctx, ExecutionMode::Sequential)
            .await?;
        parse_command_result(&resp)
    }
}

struct S5rPreToolUseHandler {
    session: Arc<S5rSession>,
    ext_id: String,
}

#[async_trait::async_trait]
impl PreToolUseHandler for S5rPreToolUseHandler {
    async fn handle(&self, ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError> {
        let invoke_ctx = require_transport_invoke_ctx(ctx.call())?;
        let input = json!({
            "session_id": ctx.session_id().map(ToString::to_string),
            "working_dir": ctx.working_dir().map(|path| path.display().to_string()),
            "model": ctx.model(),
            "call_id": ctx.call_id(),
            "tool_name": ctx.tool_name(),
            "tool_input": ctx.tool_input(),
            "available_tools": ctx.available_tools(),
        });
        let resp = invoke_hook(
            &self.session,
            &self.ext_id,
            "pre_tool_use",
            &invoke_ctx,
            input,
        )
        .await?;
        parse_pre_tool_use_result(&resp)
    }
}

struct S5rPostToolUseHandler {
    session: Arc<S5rSession>,
    ext_id: String,
}

#[async_trait::async_trait]
impl PostToolUseHandler for S5rPostToolUseHandler {
    async fn handle(&self, ctx: PostToolUseContext) -> Result<PostToolUseResult, ExtensionError> {
        let is_error = ctx.tool_result().is_error;
        let invoke_ctx = require_transport_invoke_ctx(ctx.call())?;
        let input = json!({
            "session_id": ctx.session_id().map(ToString::to_string),
            "working_dir": ctx.working_dir().map(|path| path.display().to_string()),
            "model": ctx.model(),
            "call_id": ctx.call_id(),
            "tool_name": ctx.tool_name(),
            "tool_input": ctx.tool_input(),
            "tool_result": ctx.tool_result(),
            "is_error": is_error,
        });
        let resp = invoke_hook(
            &self.session,
            &self.ext_id,
            "post_tool_use",
            &invoke_ctx,
            input,
        )
        .await?;
        parse_post_tool_use_result(&resp)
    }
}

struct S5rProviderHandler {
    session: Arc<S5rSession>,
    ext_id: String,
    on: String,
}

#[async_trait::async_trait]
impl ProviderHandler for S5rProviderHandler {
    async fn handle(&self, ctx: ProviderContext) -> Result<ProviderResult, ExtensionError> {
        let invoke_ctx = require_transport_invoke_ctx(ctx.call())?;
        let input = json!({
            "session_id": ctx.session_id().map(ToString::to_string),
            "working_dir": ctx.working_dir().map(|path| path.display().to_string()),
            "model": ctx.model(),
            "messages": ctx.messages(),
        });
        let resp = invoke_hook(&self.session, &self.ext_id, &self.on, &invoke_ctx, input).await?;
        parse_provider_result(&resp)
    }
}

struct S5rContinueAfterStopHandler {
    session: Arc<S5rSession>,
    ext_id: String,
}

#[async_trait::async_trait]
impl ContinueAfterStopHandler for S5rContinueAfterStopHandler {
    async fn handle(
        &self,
        ctx: ContinueAfterStopContext,
    ) -> Result<ContinueAfterStopResult, ExtensionError> {
        let invoke_ctx = require_transport_invoke_ctx(ctx.call())?;
        let input = json!({
            "session_id": ctx.session_id().map(ToString::to_string),
            "working_dir": ctx.working_dir().map(|path| path.display().to_string()),
            "model": ctx.model(),
            "assistant_text": ctx.assistant_text(),
            "finish_reason": ctx.finish_reason(),
            "continuations_this_turn": ctx.continuations_this_turn(),
        });
        let resp = invoke_hook(
            &self.session,
            &self.ext_id,
            "continue_after_stop",
            &invoke_ctx,
            input,
        )
        .await?;
        parse_continue_after_stop_result(&resp)
    }
}

struct S5rPromptBuildHandler {
    session: Arc<S5rSession>,
    ext_id: String,
}

#[async_trait::async_trait]
impl PromptBuildHandler for S5rPromptBuildHandler {
    async fn handle(&self, ctx: PromptBuildContext) -> Result<PromptContributions, ExtensionError> {
        let invoke_ctx = require_transport_invoke_ctx(ctx.call())?;
        let input = json!({
            "session_id": ctx.session_id().map(ToString::to_string),
            "working_dir": ctx.working_dir().map(|path| path.display().to_string()),
            "model": ctx.model(),
        });
        let resp = invoke_hook(
            &self.session,
            &self.ext_id,
            "prompt_build",
            &invoke_ctx,
            input,
        )
        .await?;
        parse_prompt_build_result(&resp)
    }
}

struct S5rCompactHandler {
    session: Arc<S5rSession>,
    ext_id: String,
    on: String,
}

#[async_trait::async_trait]
impl CompactHandler for S5rCompactHandler {
    async fn handle(&self, ctx: CompactContext) -> Result<CompactResult, ExtensionError> {
        let invoke_ctx = require_transport_invoke_ctx(ctx.call())?;
        let input = json!({
            "session_id": ctx.session_id().map(ToString::to_string),
            "working_dir": ctx.working_dir().map(|path| path.display().to_string()),
            "model": ctx.model(),
            "trigger": ctx.trigger(),
            "message_count": ctx.message_count(),
            "pre_tokens": ctx.pre_tokens(),
            "post_tokens": ctx.post_tokens(),
            "summary": ctx.summary(),
        });
        let resp = invoke_hook(&self.session, &self.ext_id, &self.on, &invoke_ctx, input).await?;
        parse_compact_result(&resp)
    }
}

struct S5rLifecycleHandler {
    session: Arc<S5rSession>,
    ext_id: String,
    on: String,
}

#[async_trait::async_trait]
impl LifecycleHandler for S5rLifecycleHandler {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        let invoke_ctx = require_transport_invoke_ctx(ctx.call())?;
        let input = json!({
            "session_id": ctx.session_id().map(ToString::to_string),
            "working_dir": ctx.working_dir().map(|path| path.display().to_string()),
            "model": ctx.model(),
            "mid_turn_user_messages_synced": ctx.mid_turn_user_messages_synced(),
        });
        let resp = invoke_hook(&self.session, &self.ext_id, &self.on, &invoke_ctx, input).await?;
        parse_lifecycle_result(&resp)
    }
}
