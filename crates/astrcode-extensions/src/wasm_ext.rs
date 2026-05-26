//! WASM 扩展适配器（s5r 对称 peer 协议）。

use std::sync::Arc;

use astrcode_extension_sdk::{
    extension::{
        CommandContext, CommandHandler, CompactContext, CompactContributions, CompactEvent,
        CompactHandler, CompactResult, EXTENSION_TOOL_OUTCOME_KEY, Extension, ExtensionCapability,
        ExtensionCommandResult, ExtensionError, ExtensionEvent, ExtensionEventDecl,
        ExtensionToolOutcome, HookMode, HookResult, LifecycleContext, LifecycleHandler,
        PostToolUseContext, PostToolUseHandler, PostToolUseResult, PreToolUseContext,
        PreToolUseHandler, PreToolUseResult, PromptBuildContext, PromptBuildHandler,
        PromptContributions, ProviderContext, ProviderEvent, ProviderHandler, ProviderResult,
        Registrar, SlashCommand, ToolHandler,
    },
    s5r::{effects::HandlerResult, event_from_name, event_to_name, mode_from_name},
    tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult, tool_metadata},
};
use parking_lot::Mutex;
use serde_json::json;

use crate::{
    extension_peer::{ExtensionPeer, PeerRegistration, manifest_types::ManifestHook},
    host_router::{HostRouter, InvokeContext},
    wasm_api::{self, HostState},
    wasm_peer_transport::{WasmGuestRuntime, WasmPeerTransport},
};

pub struct WasmExtension {
    id: String,
    capabilities: Vec<ExtensionCapability>,
    peer: Arc<ExtensionPeer>,
    event_decls: Vec<ExtensionEventDecl>,
    tools: Vec<ToolDefinition>,
    commands: Vec<SlashCommand>,
    subscriptions: Vec<(ExtensionEvent, HookMode)>,
}

impl WasmExtension {
    pub fn load(
        path: &std::path::Path,
        fuel: u64,
        memory_bytes: usize,
        host_router: Arc<HostRouter>,
    ) -> Result<Arc<Self>, String> {
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine =
            wasmtime::Engine::new(&config).map_err(|e| format!("create wasm engine: {e}"))?;
        let module = wasmtime::Module::from_file(&engine, path)
            .map_err(|e| format!("compile wasm module: {e}"))?;
        let linker = wasm_api::create_linker(&engine)?;

        let mut store =
            wasmtime::Store::new(&engine, HostState::new().with_limits(fuel, memory_bytes));
        store.limiter(|s: &mut HostState| -> &mut dyn wasmtime::ResourceLimiter { s });

        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| format!("instantiate wasm: {e}"))?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or("wasm module must export 'memory'")?;
        let alloc_fn = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .map_err(|e| format!("must export 'alloc': {e}"))?;
        let dealloc_fn = instance
            .get_typed_func::<(i32, i32), ()>(&mut store, "dealloc")
            .map_err(|e| format!("must export 'dealloc': {e}"))?;
        let exchange_fn = instance
            .get_typed_func::<(i32, i32), i64>(&mut store, "peer_exchange")
            .map_err(|e| format!("must export 'peer_exchange': {e}"))?;
        let init_fn = instance
            .get_typed_func::<(), i32>(&mut store, "extension_init")
            .map_err(|e| format!("must export 'extension_init': {e}"))?;

        let runtime = Arc::new(Mutex::new(WasmGuestRuntime {
            store,
            memory,
            alloc_fn,
            dealloc_fn,
            exchange_fn,
        }));

        let job_tx = WasmPeerTransport::spawn(Arc::clone(&runtime), "loading")?;
        let peer = Arc::new(ExtensionPeer::new(job_tx, host_router));

        {
            let mut guard = runtime.lock();
            guard.store.data_mut().set_peer(Arc::clone(&peer));
            guard
                .store
                .data_mut()
                .set_invoke_context(&InvokeContext::default());
            guard
                .store
                .set_fuel(fuel)
                .map_err(|e| format!("set_fuel: {e}"))?;
            init_fn
                .call(&mut guard.store, ())
                .map_err(|e| format!("extension_init trap: {e}"))?;
            guard.store.data_mut().clear_invoke_context();
        }

        let reg = peer
            .registration()
            .ok_or("extension_init did not complete s5r handshake")?;

        validate_registration(&reg)?;

        let tools = build_tools(&reg);
        let commands = build_commands(&reg);
        let subscriptions = build_subscriptions(&reg);
        let event_decls = reg.extension_events.clone();

        Ok(Arc::new(Self {
            id: reg.extension_id,
            capabilities: reg.capabilities,
            peer,
            event_decls,
            tools,
            commands,
            subscriptions,
        }))
    }
}

fn validate_registration(reg: &PeerRegistration) -> Result<(), String> {
    if reg.extension_id.trim().is_empty() {
        return Err("s5r initialize: extension id is empty".into());
    }
    Ok(())
}

fn build_tools(reg: &PeerRegistration) -> Vec<ToolDefinition> {
    reg.tools
        .iter()
        .map(|t| ToolDefinition {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: t.parameters.clone(),
            origin: ToolOrigin::Extension,
            execution_mode: if t.mode == "parallel" {
                ExecutionMode::Parallel
            } else {
                ExecutionMode::Sequential
            },
        })
        .collect()
}

fn build_commands(reg: &PeerRegistration) -> Vec<SlashCommand> {
    reg.commands
        .iter()
        .map(|c| SlashCommand {
            name: c.name.clone(),
            description: c.description.clone(),
            args_schema: None,
        })
        .collect()
}

fn build_subscriptions(reg: &PeerRegistration) -> Vec<(ExtensionEvent, HookMode)> {
    reg.hooks
        .iter()
        .filter_map(|h: &ManifestHook| {
            let event = event_from_name(&h.on)?;
            let mode = mode_from_name(&h.mode)?;
            Some((event, mode))
        })
        .collect()
}

#[async_trait::async_trait]
impl Extension for WasmExtension {
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
                Arc::new(WasmToolHandler {
                    peer: Arc::clone(&self.peer),
                    extension_id: self.id.clone(),
                }),
            );
        }
        for cmd in &self.commands {
            reg.command(
                cmd.clone(),
                Arc::new(WasmCommandHandler {
                    peer: Arc::clone(&self.peer),
                    extension_id: self.id.clone(),
                }),
            );
        }
        for (event, mode) in &self.subscriptions {
            let peer = Arc::clone(&self.peer);
            let ext_id = self.id.clone();
            match event {
                ExtensionEvent::PreToolUse => {
                    reg.on_pre_tool_use(*mode, 0, Arc::new(WasmPreToolUseHandler { peer, ext_id }));
                },
                ExtensionEvent::PostToolUse => {
                    reg.on_post_tool_use(
                        *mode,
                        0,
                        Arc::new(WasmPostToolUseHandler { peer, ext_id }),
                    );
                },
                ExtensionEvent::BeforeProviderRequest => {
                    reg.on_provider(
                        ProviderEvent::BeforeRequest,
                        *mode,
                        0,
                        Arc::new(WasmProviderHandler {
                            peer,
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
                        Arc::new(WasmProviderHandler {
                            peer,
                            ext_id,
                            on: "after_provider_response".into(),
                        }),
                    );
                },
                ExtensionEvent::PromptBuild => {
                    reg.on_prompt_build(0, Arc::new(WasmPromptBuildHandler { peer, ext_id }));
                },
                ExtensionEvent::PreCompact => {
                    reg.on_compact(
                        CompactEvent::PreCompact,
                        0,
                        Arc::new(WasmCompactHandler {
                            peer,
                            ext_id,
                            on: "pre_compact".into(),
                        }),
                    );
                },
                ExtensionEvent::PostCompact => {
                    reg.on_compact(
                        CompactEvent::PostCompact,
                        0,
                        Arc::new(WasmCompactHandler {
                            peer,
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
                        Arc::new(WasmLifecycleHandler { peer, ext_id, on }),
                    );
                },
            }
        }
    }
}

// ─── result parsers (HandlerResult) ──────────────────────────────────────

fn parse_tool_result(resp: &HandlerResult) -> Result<ToolResult, ExtensionError> {
    if !resp.ok {
        let msg = resp.error.clone().unwrap_or_default();
        return Ok(ToolResult::text(msg, true, Default::default()));
    }
    match resp.effect_name() {
        "tool_outcome" => {
            let raw = resp
                .data_value("outcome")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let outcome: ExtensionToolOutcome = serde_json::from_value(raw)
                .map_err(|e| ExtensionError::Internal(format!("parse tool_outcome: {e}")))?;
            let outcome_json = serde_json::to_value(&outcome)
                .map_err(|e| ExtensionError::Internal(format!("serialize outcome: {e}")))?;
            Ok(ToolResult::text(
                String::new(),
                false,
                tool_metadata([(EXTENSION_TOOL_OUTCOME_KEY, outcome_json)]),
            ))
        },
        _ => {
            let content = resp
                .data_value("content")
                .and_then(|v| v.as_str())
                .map(ToString::to_string)
                .unwrap_or_default();
            Ok(ToolResult::text(content, false, Default::default()))
        },
    }
}

fn parse_command_result(resp: &HandlerResult) -> Result<ExtensionCommandResult, ExtensionError> {
    if !resp.ok {
        return Err(ExtensionError::Internal(
            resp.error.clone().unwrap_or_default(),
        ));
    }
    let data = resp.data.clone().unwrap_or(json!({}));
    serde_json::from_value(data)
        .map_err(|e| ExtensionError::Internal(format!("parse command result: {e}")))
}

fn parse_pre_tool_use_result(resp: &HandlerResult) -> Result<PreToolUseResult, ExtensionError> {
    if !resp.ok {
        return Ok(PreToolUseResult::Allow);
    }
    match resp.effect_name() {
        "block" => Ok(PreToolUseResult::Block {
            reason: resp.data_str("reason").to_string(),
        }),
        "modified_input" => {
            let tool_input = resp.data_value("tool_input").cloned().ok_or_else(|| {
                ExtensionError::Internal("effect=modified_input but data.tool_input missing".into())
            })?;
            Ok(PreToolUseResult::ModifyInput { tool_input })
        },
        _ => Ok(PreToolUseResult::Allow),
    }
}

fn parse_post_tool_use_result(resp: &HandlerResult) -> Result<PostToolUseResult, ExtensionError> {
    if !resp.ok {
        return Ok(PostToolUseResult::Allow);
    }
    match resp.effect_name() {
        "block" => Ok(PostToolUseResult::Block {
            reason: resp.data_str("reason").to_string(),
        }),
        "tool_outcome" => Ok(PostToolUseResult::ModifyResult {
            content: resp.data_str("content").to_string(),
        }),
        _ => Ok(PostToolUseResult::Allow),
    }
}

fn parse_provider_result(resp: &HandlerResult) -> Result<ProviderResult, ExtensionError> {
    if !resp.ok {
        return Ok(ProviderResult::Allow);
    }
    match resp.effect_name() {
        "block" => Ok(ProviderResult::Block {
            reason: resp.data_str("reason").to_string(),
        }),
        "replace_messages" => {
            let messages_val = resp.data_value("messages").cloned().ok_or_else(|| {
                ExtensionError::Internal("effect=replace_messages but data.messages missing".into())
            })?;
            Ok(ProviderResult::ReplaceMessages {
                messages: serde_json::from_value(messages_val)
                    .map_err(|e| ExtensionError::Internal(format!("parse messages: {e}")))?,
            })
        },
        "append_messages" => {
            let messages_val = resp.data_value("messages").cloned().ok_or_else(|| {
                ExtensionError::Internal("effect=append_messages but data.messages missing".into())
            })?;
            Ok(ProviderResult::AppendMessages {
                messages: serde_json::from_value(messages_val)
                    .map_err(|e| ExtensionError::Internal(format!("parse messages: {e}")))?,
            })
        },
        _ => Ok(ProviderResult::Allow),
    }
}

fn parse_prompt_build_result(resp: &HandlerResult) -> Result<PromptContributions, ExtensionError> {
    if !resp.ok || resp.effect_name() != "prompt_contributions" {
        return Ok(PromptContributions::default());
    }
    serde_json::from_value(resp.data.clone().unwrap_or_default())
        .map_err(|e| ExtensionError::Internal(format!("parse PromptContributions: {e}")))
}

fn parse_compact_result(resp: &HandlerResult) -> Result<CompactResult, ExtensionError> {
    if !resp.ok || resp.effect_name() != "compact_contributions" {
        return Ok(CompactResult::Allow);
    }
    let contributions: CompactContributions =
        serde_json::from_value(resp.data.clone().unwrap_or_default())
            .map_err(|e| ExtensionError::Internal(format!("parse CompactContributions: {e}")))?;
    Ok(CompactResult::Contributions(contributions))
}

fn parse_lifecycle_result(resp: &HandlerResult) -> Result<HookResult, ExtensionError> {
    if resp.ok {
        Ok(HookResult::Allow)
    } else {
        Ok(HookResult::Block {
            reason: resp.error.clone().unwrap_or_default(),
        })
    }
}

fn handler_id(extension_id: &str, kind: &str, name: &str) -> String {
    format!("{extension_id}:{kind}:{name}")
}

// ─── handlers ────────────────────────────────────────────────────────────

struct WasmToolHandler {
    peer: Arc<ExtensionPeer>,
    extension_id: String,
}

#[async_trait::async_trait]
impl ToolHandler for WasmToolHandler {
    async fn execute(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
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
            event_declarations: self.peer.event_decls(),
            declared_capabilities: self.peer.declared_capabilities(),
            on_wasm_peer_thread: false,
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
            .peer
            .invoke_handler_with_continuations(&hid, event, &invoke_ctx)
            .await?;
        parse_tool_result(&resp)
    }
}

struct WasmCommandHandler {
    peer: Arc<ExtensionPeer>,
    extension_id: String,
}

#[async_trait::async_trait]
impl CommandHandler for WasmCommandHandler {
    async fn execute(
        &self,
        command_name: &str,
        arguments: &str,
        working_dir: &str,
        ctx: &CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        let invoke_ctx = InvokeContext {
            extension_id: self.extension_id.clone(),
            session_id: Some(ctx.session_id.to_string()),
            session_store_dir: None,
            session_ops: None,
            event_tx: None,
            working_dir: Some(working_dir.to_string()),
            cancel_token: None,
            event_declarations: self.peer.event_decls(),
            declared_capabilities: self.peer.declared_capabilities(),
            on_wasm_peer_thread: false,
        };
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
            .peer
            .invoke_handler_with_continuations(&hid, event, &invoke_ctx)
            .await?;
        parse_command_result(&resp)
    }
}

struct WasmPreToolUseHandler {
    peer: Arc<ExtensionPeer>,
    ext_id: String,
}

#[async_trait::async_trait]
impl PreToolUseHandler for WasmPreToolUseHandler {
    async fn handle(&self, ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError> {
        let invoke_ctx = hook_invoke_ctx(
            &self.peer,
            &self.ext_id,
            Some(ctx.session_id.clone()),
            Some(ctx.working_dir.clone()),
            ctx.session_store_dir.clone(),
            ctx.event_tx.clone(),
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
            .peer
            .invoke_handler_with_continuations(
                &hid,
                json!({ "on": "pre_tool_use", "input": input }),
                &invoke_ctx,
            )
            .await?;
        parse_pre_tool_use_result(&resp)
    }
}

// Similar handlers - I'll add simplified versions for other hooks

struct WasmPostToolUseHandler {
    peer: Arc<ExtensionPeer>,
    ext_id: String,
}

#[async_trait::async_trait]
impl PostToolUseHandler for WasmPostToolUseHandler {
    async fn handle(&self, ctx: PostToolUseContext) -> Result<PostToolUseResult, ExtensionError> {
        let invoke_ctx = hook_invoke_ctx(
            &self.peer,
            &self.ext_id,
            Some(ctx.session_id.clone()),
            Some(ctx.working_dir.clone()),
            ctx.session_store_dir.clone(),
            ctx.event_tx.clone(),
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
            .peer
            .invoke_handler_with_continuations(
                &hid,
                json!({ "on": "post_tool_use", "input": input }),
                &invoke_ctx,
            )
            .await?;
        parse_post_tool_use_result(&resp)
    }
}

fn hook_invoke_ctx(
    peer: &Arc<ExtensionPeer>,
    ext_id: &str,
    session_id: Option<String>,
    working_dir: Option<String>,
    session_store_dir: Option<std::path::PathBuf>,
    event_tx: Option<tokio::sync::mpsc::UnboundedSender<astrcode_core::event::EventPayload>>,
) -> InvokeContext {
    InvokeContext {
        extension_id: ext_id.to_string(),
        session_id,
        session_store_dir,
        session_ops: None,
        event_tx,
        working_dir,
        cancel_token: None,
        event_declarations: peer.event_decls(),
        declared_capabilities: peer.declared_capabilities(),
        on_wasm_peer_thread: false,
    }
}

struct WasmProviderHandler {
    peer: Arc<ExtensionPeer>,
    ext_id: String,
    on: String,
}

#[async_trait::async_trait]
impl ProviderHandler for WasmProviderHandler {
    async fn handle(&self, ctx: ProviderContext) -> Result<ProviderResult, ExtensionError> {
        let invoke_ctx = hook_invoke_ctx(
            &self.peer,
            &self.ext_id,
            Some(ctx.session_id.clone()),
            Some(ctx.working_dir.clone()),
            ctx.session_store_dir.clone(),
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
            .peer
            .invoke_handler_with_continuations(
                &hid,
                json!({ "on": self.on, "input": input }),
                &invoke_ctx,
            )
            .await?;
        parse_provider_result(&resp)
    }
}

struct WasmPromptBuildHandler {
    peer: Arc<ExtensionPeer>,
    ext_id: String,
}

#[async_trait::async_trait]
impl PromptBuildHandler for WasmPromptBuildHandler {
    async fn handle(&self, ctx: PromptBuildContext) -> Result<PromptContributions, ExtensionError> {
        let invoke_ctx = hook_invoke_ctx(
            &self.peer,
            &self.ext_id,
            Some(ctx.session_id.clone()),
            Some(ctx.working_dir.clone()),
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
            .peer
            .invoke_handler_with_continuations(
                &hid,
                json!({ "on": "prompt_build", "input": input }),
                &invoke_ctx,
            )
            .await?;
        parse_prompt_build_result(&resp)
    }
}

struct WasmCompactHandler {
    peer: Arc<ExtensionPeer>,
    ext_id: String,
    on: String,
}

#[async_trait::async_trait]
impl CompactHandler for WasmCompactHandler {
    async fn handle(&self, ctx: CompactContext) -> Result<CompactResult, ExtensionError> {
        let invoke_ctx = hook_invoke_ctx(
            &self.peer,
            &self.ext_id,
            Some(ctx.session_id.clone()),
            Some(ctx.working_dir.clone()),
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
            .peer
            .invoke_handler_with_continuations(
                &hid,
                json!({ "on": self.on, "input": input }),
                &invoke_ctx,
            )
            .await?;
        parse_compact_result(&resp)
    }
}

struct WasmLifecycleHandler {
    peer: Arc<ExtensionPeer>,
    ext_id: String,
    on: String,
}

#[async_trait::async_trait]
impl LifecycleHandler for WasmLifecycleHandler {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        let invoke_ctx = hook_invoke_ctx(
            &self.peer,
            &self.ext_id,
            Some(ctx.session_id.clone()),
            Some(ctx.working_dir.clone()),
            None,
            ctx.event_tx.clone(),
        );
        let input = json!({
            "session_id": ctx.session_id,
            "working_dir": ctx.working_dir,
            "model": ctx.model,
        });
        let hid = handler_id(&self.ext_id, "hook", &self.on);
        let resp = self
            .peer
            .invoke_handler_with_continuations(
                &hid,
                json!({ "on": self.on, "input": input }),
                &invoke_ctx,
            )
            .await?;
        parse_lifecycle_result(&resp)
    }
}
