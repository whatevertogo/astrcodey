//! 磁盘 s5r 子进程扩展：stdio 长度前缀帧 + WireMessage。

mod protocol;
mod session;

use std::{path::Path, sync::Arc};

use astrcode_core::extension::{
    CommandContext, CommandHandler, CompactContext, CompactEvent, CompactHandler, CompactResult,
    Extension, ExtensionCapability, ExtensionCommandResult, ExtensionError, ExtensionEvent,
    ExtensionEventDecl, HookMode, HookResult, LifecycleContext, LifecycleHandler,
    PostToolUseContext, PostToolUseHandler, PostToolUseResult, PreToolUseContext,
    PreToolUseHandler, PreToolUseResult, PromptBuildContext, PromptBuildHandler,
    PromptContributions, ProviderContext, ProviderEvent, ProviderHandler, ProviderResult,
    Registrar, SlashCommand, StopReason, ToolHandler,
};
use astrcode_extension_sdk::{
    s5r::event_to_name,
    tool::{ToolDefinition, ToolResult},
};
pub use protocol::S5R_PROTOCOL_VERSION;
use serde_json::{Value, json};

use crate::{
    extension_manifest::ExtensionRegistration,
    host_router::{HostRouter, InvokeContext},
    remote_manifest::{
        build_commands, build_subscriptions, build_tools, handler_id, parse_command_result,
        parse_compact_result, parse_lifecycle_result, parse_post_tool_use_result,
        parse_pre_tool_use_result, parse_prompt_build_result, parse_provider_result,
        parse_tool_result, validate_registration,
    },
    s5r_ext::session::S5rSession,
};

pub struct S5rExtension {
    id: String,
    capabilities: Vec<ExtensionCapability>,
    session: Arc<S5rSession>,
    event_decls: Vec<ExtensionEventDecl>,
    tools: Vec<ToolDefinition>,
    commands: Vec<SlashCommand>,
    subscriptions: Vec<(ExtensionEvent, HookMode)>,
}

impl S5rExtension {
    pub async fn load(
        ext_dir: &Path,
        manifest: &Value,
        host_router: Arc<HostRouter>,
        working_dir: Option<&str>,
    ) -> Result<Arc<Self>, String> {
        let (program, args) = parse_command(manifest, ext_dir)?;
        let env = parse_env(manifest);
        let session =
            S5rSession::spawn(&program, &args, ext_dir, &env, host_router, working_dir).await?;
        let reg = session
            .registration()
            .ok_or("s5r extension did not complete initialize handshake")?;
        validate_registration(&reg)?;
        Ok(build_extension(session, reg))
    }
}

fn build_extension(session: Arc<S5rSession>, reg: ExtensionRegistration) -> Arc<S5rExtension> {
    let tools = build_tools(&reg);
    let commands = build_commands(&reg);
    let subscriptions = build_subscriptions(&reg);
    let ExtensionRegistration {
        extension_id,
        capabilities,
        extension_events,
        ..
    } = reg;
    Arc::new(S5rExtension {
        id: extension_id,
        capabilities,
        session,
        event_decls: extension_events,
        tools,
        commands,
        subscriptions,
    })
}

fn parse_command(manifest: &Value, ext_dir: &Path) -> Result<(String, Vec<String>), String> {
    let cmd = manifest
        .get("command")
        .ok_or("extension.json missing 'command' array for s5r extension")?;
    let arr = cmd
        .as_array()
        .ok_or("'command' must be a JSON array of strings")?;
    if arr.is_empty() {
        return Err("'command' must contain at least the executable path".into());
    }
    let program = arr[0]
        .as_str()
        .ok_or("command[0] must be a string")?
        .to_string();
    let program_path = Path::new(&program);
    let program = if program_path.is_absolute() {
        program
    } else if program.contains('/') || program.contains('\\') {
        ext_dir.join(program_path).to_string_lossy().into_owned()
    } else {
        program
    };
    let args: Vec<String> = arr[1..]
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or("command elements must be strings")
                .map(str::to_string)
        })
        .collect::<Result<_, _>>()?;
    Ok((program, args))
}

fn parse_env(manifest: &Value) -> Vec<(String, String)> {
    manifest
        .get("env")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

#[async_trait::async_trait]
impl Extension for S5rExtension {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> &[ExtensionCapability] {
        &self.capabilities
    }

    fn register(&self, reg: &mut Registrar) {
        for decl in &self.event_decls {
            reg.extension_event(&decl.event_type)
                .schema_version(decl.schema_version)
                .durable(decl.durable)
                .max_payload_bytes(decl.max_payload_bytes)
                .register();
        }
        for tool_def in &self.tools {
            reg.tool(
                tool_def.clone(),
                Arc::new(S5rToolHandler {
                    session: Arc::clone(&self.session),
                    extension_id: self.id.clone(),
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
        for (event, mode) in &self.subscriptions {
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
                    reg.on_provider(
                        ProviderEvent::BeforeRequest,
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
                    reg.on_provider(
                        ProviderEvent::AfterResponse,
                        *mode,
                        0,
                        Arc::new(S5rProviderHandler {
                            session,
                            ext_id,
                            on: "after_provider_response".into(),
                        }),
                    );
                },
                ExtensionEvent::PromptBuild => {
                    reg.on_prompt_build(0, Arc::new(S5rPromptBuildHandler { session, ext_id }));
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
                    reg.on_event(
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

fn hook_invoke_ctx(
    session: &Arc<S5rSession>,
    ext_id: &str,
    session_id: Option<String>,
    working_dir: Option<String>,
    session_store_dir: Option<std::path::PathBuf>,
    event_tx: Option<tokio::sync::mpsc::UnboundedSender<astrcode_core::event::EventPayload>>,
    session_ops: Option<Arc<dyn astrcode_core::tool::SessionOperations>>,
) -> InvokeContext {
    InvokeContext {
        extension_id: ext_id.to_string(),
        session_id,
        session_store_dir,
        session_ops,
        event_tx,
        working_dir,
        cancel_token: None,
        event_declarations: session.event_decls(),
        declared_capabilities: session.declared_capabilities(),
        on_peer_io_thread: false,
    }
}

struct S5rToolHandler {
    session: Arc<S5rSession>,
    extension_id: String,
}

#[async_trait::async_trait]
impl ToolHandler for S5rToolHandler {
    async fn execute(
        &self,
        tool_name: &str,
        arguments: Value,
        working_dir: &str,
        ctx: &astrcode_extension_sdk::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        let invoke_ctx = InvokeContext {
            extension_id: self.extension_id.clone(),
            session_id: Some(ctx.session_id.to_string()),
            session_store_dir: ctx.capabilities.session_store_dir.clone(),
            session_ops: ctx.capabilities.session_ops.clone(),
            event_tx: ctx.event_tx.clone(),
            working_dir: Some(working_dir.to_string()),
            cancel_token: None,
            event_declarations: self.session.event_decls(),
            declared_capabilities: self.session.declared_capabilities(),
            on_peer_io_thread: false,
        };
        let event = json!({
            "on": "tool",
            "name": tool_name,
            "input": {
                "tool_name": tool_name,
                "arguments": arguments,
                "working_dir": working_dir,
                "session_id": ctx.session_id,
                "tool_call_id": ctx.tool_call_id,
            }
        });
        let hid = handler_id(&self.extension_id, "tool", tool_name);
        let resp = self
            .session
            .invoke_handler_with_continuations(&hid, event, &invoke_ctx)
            .await?;
        parse_tool_result(&resp)
    }
}

struct S5rCommandHandler {
    session: Arc<S5rSession>,
    extension_id: String,
}

#[async_trait::async_trait]
impl CommandHandler for S5rCommandHandler {
    async fn execute(
        &self,
        command_name: &str,
        arguments: &str,
        working_dir: &str,
        ctx: &CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        let invoke_ctx = hook_invoke_ctx(
            &self.session,
            &self.extension_id,
            Some(ctx.session_id.to_string()),
            Some(working_dir.to_string()),
            None,
            None,
            None,
        );
        let event = json!({
            "on": "command",
            "name": command_name,
            "input": {
                "command_name": command_name,
                "arguments": arguments,
                "working_dir": working_dir,
                "session_id": ctx.session_id,
                "model": ctx.model,
            }
        });
        let hid = handler_id(&self.extension_id, "command", command_name);
        let resp = self
            .session
            .invoke_handler_with_continuations(&hid, event, &invoke_ctx)
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
        let invoke_ctx = hook_invoke_ctx(
            &self.session,
            &self.ext_id,
            Some(ctx.session_id.clone()),
            Some(ctx.working_dir.clone()),
            ctx.session_store_dir,
            ctx.event_tx,
            None,
        );
        let input = json!({
            "session_id": ctx.session_id,
            "working_dir": ctx.working_dir,
            "model": ctx.model,
            "tool_name": ctx.tool_name,
            "tool_input": ctx.tool_input,
            "available_tools": ctx.available_tools,
        });
        let hid = handler_id(&self.ext_id, "hook", "pre_tool_use");
        let resp = self
            .session
            .invoke_handler_with_continuations(
                &hid,
                json!({ "on": "pre_tool_use", "input": input }),
                &invoke_ctx,
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
        let invoke_ctx = hook_invoke_ctx(
            &self.session,
            &self.ext_id,
            Some(ctx.session_id.clone()),
            Some(ctx.working_dir.clone()),
            ctx.session_store_dir,
            ctx.event_tx,
            None,
        );
        let input = json!({
            "session_id": ctx.session_id,
            "working_dir": ctx.working_dir,
            "model": ctx.model,
            "tool_name": ctx.tool_name,
            "tool_input": ctx.tool_input,
            "tool_result": ctx.tool_result,
            "is_error": ctx.is_error,
        });
        let hid = handler_id(&self.ext_id, "hook", "post_tool_use");
        let resp = self
            .session
            .invoke_handler_with_continuations(
                &hid,
                json!({ "on": "post_tool_use", "input": input }),
                &invoke_ctx,
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
        let invoke_ctx = hook_invoke_ctx(
            &self.session,
            &self.ext_id,
            Some(ctx.session_id.clone()),
            Some(ctx.working_dir.clone()),
            ctx.session_store_dir,
            None,
            None,
        );
        let input = json!({
            "session_id": ctx.session_id,
            "working_dir": ctx.working_dir,
            "model": ctx.model,
            "messages": ctx.messages,
        });
        let hid = handler_id(&self.ext_id, "hook", &self.on);
        let resp = self
            .session
            .invoke_handler_with_continuations(
                &hid,
                json!({ "on": self.on, "input": input }),
                &invoke_ctx,
            )
            .await?;
        parse_provider_result(&resp)
    }
}

struct S5rPromptBuildHandler {
    session: Arc<S5rSession>,
    ext_id: String,
}

#[async_trait::async_trait]
impl PromptBuildHandler for S5rPromptBuildHandler {
    async fn handle(&self, ctx: PromptBuildContext) -> Result<PromptContributions, ExtensionError> {
        let invoke_ctx = hook_invoke_ctx(
            &self.session,
            &self.ext_id,
            Some(ctx.session_id.clone()),
            Some(ctx.working_dir.clone()),
            None,
            None,
            None,
        );
        let input = json!({
            "session_id": ctx.session_id,
            "working_dir": ctx.working_dir,
            "model": ctx.model,
        });
        let hid = handler_id(&self.ext_id, "hook", "prompt_build");
        let resp = self
            .session
            .invoke_handler_with_continuations(
                &hid,
                json!({ "on": "prompt_build", "input": input }),
                &invoke_ctx,
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
        let invoke_ctx = hook_invoke_ctx(
            &self.session,
            &self.ext_id,
            Some(ctx.session_id.clone()),
            Some(ctx.working_dir.clone()),
            None,
            None,
            None,
        );
        let input = json!({
            "session_id": ctx.session_id,
            "working_dir": ctx.working_dir,
            "model": ctx.model,
            "trigger": ctx.trigger,
            "message_count": ctx.message_count,
            "pre_tokens": ctx.pre_tokens,
            "post_tokens": ctx.post_tokens,
            "summary": ctx.summary,
        });
        let hid = handler_id(&self.ext_id, "hook", &self.on);
        let resp = self
            .session
            .invoke_handler_with_continuations(
                &hid,
                json!({ "on": self.on, "input": input }),
                &invoke_ctx,
            )
            .await?;
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
        let invoke_ctx = hook_invoke_ctx(
            &self.session,
            &self.ext_id,
            Some(ctx.session_id.clone()),
            Some(ctx.working_dir.clone()),
            None,
            ctx.event_tx,
            None,
        );
        let input = json!({
            "session_id": ctx.session_id,
            "working_dir": ctx.working_dir,
            "model": ctx.model,
        });
        let hid = handler_id(&self.ext_id, "hook", &self.on);
        let resp = self
            .session
            .invoke_handler_with_continuations(
                &hid,
                json!({ "on": self.on, "input": input }),
                &invoke_ctx,
            )
            .await?;
        parse_lifecycle_result(&resp)
    }
}
