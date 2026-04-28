//! Extension runner — dispatches lifecycle events to registered extensions.

use std::{sync::Arc, time::Duration};

use astrcode_core::{
    extension::*,
    tool::{Tool, ToolDefinition, ToolError, ToolExecutionContext, ToolResult},
};
use tokio::sync::RwLock;

use crate::runtime::{ExtensionRuntime, SessionSpawner, SpawnRequest};

/// Dispatches lifecycle events to all registered extensions.
///
/// Enforces HookMode semantics:
/// - Blocking: synchronous, can return Block or ModifiedInput/ModifiedResult
/// - NonBlocking: spawned as fire-and-forget task with snapshot context
/// - Advisory: result logged but not enforced
pub struct ExtensionRunner {
    extensions: RwLock<Vec<Arc<dyn Extension>>>,
    runtime: Arc<ExtensionRuntime>,
    timeout: Duration,
}

impl ExtensionRunner {
    pub fn new(timeout: Duration, runtime: Arc<ExtensionRuntime>) -> Self {
        Self {
            extensions: RwLock::new(Vec::new()),
            runtime,
            timeout,
        }
    }

    /// Register an extension.
    pub async fn register(&self, ext: Arc<dyn Extension>) {
        let mut exts = self.extensions.write().await;
        exts.push(ext);
    }

    /// Bind session spawn capability to the shared runtime.
    /// Called once after server boot, before any tool execution.
    pub fn bind(&self, spawner: Arc<dyn SessionSpawner>) {
        self.runtime.bind(spawner);
    }

    /// Dispatch an event to all subscribed extensions.
    ///
    /// Copies the extension list before iterating so the read lock is not held
    /// during hook execution.
    pub async fn dispatch(
        &self,
        event: ExtensionEvent,
        ctx: &dyn ExtensionContext,
    ) -> Result<(), ExtensionError> {
        let exts: Vec<Arc<dyn Extension>> = { self.extensions.read().await.clone() };

        for ext in &exts {
            let mode = match_ext_mode(ext.as_ref(), &event);
            let Some(mode) = mode else { continue };

            match mode {
                HookMode::Blocking => {
                    let result =
                        tokio::time::timeout(self.timeout, ext.on_event(event.clone(), ctx))
                            .await
                            .map_err(|_| {
                                ExtensionError::Timeout(self.timeout.as_millis() as u64)
                            })??;

                    if let HookEffect::Block { reason } = result {
                        return Err(ExtensionError::Internal(reason));
                    }
                    // HookEffect::Modified* / Allow variants pass through
                },
                HookMode::NonBlocking => {
                    let ext = Arc::clone(ext);
                    let evt = event.clone();
                    // Use a snapshot so we release the borrow before spawning
                    let snap_ctx = ctx.snapshot();
                    tokio::spawn(async move {
                        let _ = ext.on_event(evt, snap_ctx.as_ref()).await;
                    });
                },
                HookMode::Advisory => {
                    let _ = ext.on_event(event.clone(), ctx).await;
                },
            }
        }

        Ok(())
    }

    /// Dispatch a PreToolUse or PostToolUse event and collect the first
    /// Blocking result (ModifiedInput / ModifiedResult / Block).
    pub async fn dispatch_tool_hook(
        &self,
        event: ExtensionEvent,
        ctx: &dyn ExtensionContext,
    ) -> Result<ToolHookOutcome, ExtensionError> {
        let exts: Vec<Arc<dyn Extension>> = { self.extensions.read().await.clone() };

        let mut modified_input: Option<serde_json::Value> = None;
        let mut modified_result: Option<String> = None;

        for ext in &exts {
            let mode = match_ext_mode(ext.as_ref(), &event);
            let Some(mode) = mode else { continue };

            match mode {
                HookMode::Blocking => {
                    let result =
                        tokio::time::timeout(self.timeout, ext.on_event(event.clone(), ctx))
                            .await
                            .map_err(|_| {
                                ExtensionError::Timeout(self.timeout.as_millis() as u64)
                            })??;

                    match result {
                        HookEffect::Block { reason } => {
                            return Ok(ToolHookOutcome::Blocked { reason });
                        },
                        HookEffect::ModifiedInput { tool_input } => {
                            modified_input = Some(tool_input);
                        },
                        HookEffect::ModifiedResult { content } => {
                            modified_result = Some(content);
                        },
                        HookEffect::ModifiedMessages { .. }
                        | HookEffect::ModifiedOutput { .. }
                        | HookEffect::Allow => {},
                    }
                },
                HookMode::NonBlocking => {
                    let ext = Arc::clone(ext);
                    let evt = event.clone();
                    let snap_ctx = ctx.snapshot();
                    tokio::spawn(async move {
                        let _ = ext.on_event(evt, snap_ctx.as_ref()).await;
                    });
                },
                HookMode::Advisory => {
                    let _ = ext.on_event(event.clone(), ctx).await;
                },
            }
        }

        Ok(match (modified_input, modified_result) {
            (Some(input), _) => ToolHookOutcome::ModifiedInput { tool_input: input },
            (_, Some(content)) => ToolHookOutcome::ModifiedResult { content },
            _ => ToolHookOutcome::Allow,
        })
    }

    /// Dispatch provider-level hooks and collect message mutations.
    pub async fn dispatch_provider_hook(
        &self,
        event: ExtensionEvent,
        ctx: &dyn ExtensionContext,
    ) -> Result<ProviderHookOutcome, ExtensionError> {
        let exts: Vec<Arc<dyn Extension>> = { self.extensions.read().await.clone() };

        let mut modified_messages = None;

        for ext in &exts {
            let mode = match_ext_mode(ext.as_ref(), &event);
            let Some(mode) = mode else { continue };

            match mode {
                HookMode::Blocking => {
                    let result =
                        tokio::time::timeout(self.timeout, ext.on_event(event.clone(), ctx))
                            .await
                            .map_err(|_| {
                                ExtensionError::Timeout(self.timeout.as_millis() as u64)
                            })??;

                    match result {
                        HookEffect::Block { reason } => {
                            return Ok(ProviderHookOutcome::Blocked { reason });
                        },
                        HookEffect::ModifiedMessages { messages } => {
                            modified_messages = Some(messages);
                        },
                        HookEffect::Allow
                        | HookEffect::ModifiedInput { .. }
                        | HookEffect::ModifiedResult { .. }
                        | HookEffect::ModifiedOutput { .. } => {},
                    }
                },
                HookMode::NonBlocking => {
                    let ext = Arc::clone(ext);
                    let evt = event.clone();
                    let snap_ctx = ctx.snapshot();
                    tokio::spawn(async move {
                        let _ = ext.on_event(evt, snap_ctx.as_ref()).await;
                    });
                },
                HookMode::Advisory => {
                    let _ = ext.on_event(event.clone(), ctx).await;
                },
            }
        }

        Ok(match modified_messages {
            Some(messages) => ProviderHookOutcome::ModifiedMessages { messages },
            None => ProviderHookOutcome::Allow,
        })
    }

    /// Current number of registered extensions.
    pub async fn count(&self) -> usize {
        self.extensions.read().await.len()
    }

    /// Collect tool definitions from all registered extensions.
    pub async fn collect_tools(&self) -> Vec<astrcode_core::tool::ToolDefinition> {
        let exts = self.extensions.read().await;
        let mut tools = Vec::new();
        for ext in exts.iter() {
            tools.extend(ext.tools());
        }
        tools
    }

    /// Collect executable tool adapters from all registered extensions.
    pub async fn collect_tool_adapters(&self, working_dir: &str) -> Vec<Arc<dyn Tool>> {
        let exts = self.extensions.read().await;
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for ext in exts.iter() {
            for def in ext.tools() {
                tools.push(Arc::new(ExtensionTool {
                    extension: Arc::clone(ext),
                    definition: def,
                    working_dir: working_dir.to_string(),
                    runtime: Arc::clone(&self.runtime),
                }));
            }
        }
        tools
    }

    /// Collect slash commands from all registered extensions.
    pub async fn collect_commands(&self) -> Vec<astrcode_core::extension::SlashCommand> {
        let exts = self.extensions.read().await;
        let mut cmds = Vec::new();
        for ext in exts.iter() {
            cmds.extend(ext.slash_commands());
        }
        cmds
    }
}

struct ExtensionTool {
    extension: Arc<dyn Extension>,
    definition: ToolDefinition,
    working_dir: String,
    runtime: Arc<ExtensionRuntime>,
}

#[async_trait::async_trait]
impl Tool for ExtensionTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let mut result = self
            .extension
            .execute_tool(&self.definition.name, arguments, &self.working_dir, _ctx)
            .await
            .map_err(extension_error_to_tool_error)?;

        // Consume declarative outcome: RunSession → spawn child session
        if let Some(outcome_value) = result.metadata.remove("extension_tool_outcome") {
            if let Ok(ExtensionToolOutcome::RunSession {
                name,
                system_prompt,
                user_prompt,
                allowed_tools,
                model_preference,
            }) = serde_json::from_value(outcome_value)
            {
                let effective_tools = if allowed_tools.is_empty() {
                    _ctx.available_tools
                        .iter()
                        .map(|t| t.name.clone())
                        .collect()
                } else {
                    allowed_tools
                };

                let request = SpawnRequest {
                    name,
                    system_prompt,
                    user_prompt,
                    working_dir: _ctx.working_dir.clone(),
                    allowed_tools: effective_tools,
                    model_preference,
                };

                match self.runtime.spawn(&_ctx.session_id, request).await {
                    Ok(output) => {
                        result.content = output.content;
                        result
                            .metadata
                            .insert("child_session_id".into(), output.child_session_id.into());
                    },
                    Err(e) => {
                        result.content = format!("Failed to spawn child session: {e}");
                        result.is_error = true;
                        result.error = Some(e);
                    },
                }
            }
        }

        Ok(result)
    }
}

fn extension_error_to_tool_error(err: ExtensionError) -> ToolError {
    match err {
        ExtensionError::NotFound(name) => ToolError::NotFound(name),
        ExtensionError::Timeout(ms) => ToolError::Timeout(ms),
        ExtensionError::Internal(message) => ToolError::Execution(message),
    }
}

/// Outcome of a tool-level hook dispatch.
#[derive(Debug, Clone)]
pub enum ToolHookOutcome {
    Allow,
    Blocked { reason: String },
    ModifiedInput { tool_input: serde_json::Value },
    ModifiedResult { content: String },
}

/// Outcome of a provider-level hook dispatch.
#[derive(Debug, Clone)]
pub enum ProviderHookOutcome {
    Allow,
    Blocked {
        reason: String,
    },
    ModifiedMessages {
        messages: Vec<astrcode_core::llm::LlmMessage>,
    },
}

fn match_ext_mode(ext: &dyn Extension, event: &ExtensionEvent) -> Option<HookMode> {
    ext.subscriptions()
        .iter()
        .find(|(e, _)| e == event)
        .map(|(_, m)| *m)
}
