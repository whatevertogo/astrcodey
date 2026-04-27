//! Extension runner — dispatches lifecycle events to registered extensions.

use std::{sync::Arc, time::Duration};

use astrcode_core::extension::*;
use tokio::sync::RwLock;

/// Dispatches lifecycle events to all registered extensions.
///
/// Enforces HookMode semantics:
/// - Blocking: synchronous, can return Block or ModifiedInput/ModifiedResult
/// - NonBlocking: spawned as fire-and-forget task with snapshot context
/// - Advisory: result logged but not enforced
pub struct ExtensionRunner {
    extensions: RwLock<Vec<Arc<dyn Extension>>>,
    timeout: Duration,
}

impl ExtensionRunner {
    pub fn new(timeout: Duration) -> Self {
        Self {
            extensions: RwLock::new(Vec::new()),
            timeout,
        }
    }

    /// Register an extension.
    pub async fn register(&self, ext: Arc<dyn Extension>) {
        let mut exts = self.extensions.write().await;
        exts.push(ext);
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
                        HookEffect::Allow => {},
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

    /// Current number of registered extensions.
    pub async fn count(&self) -> usize {
        self.extensions.read().await.len()
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

fn match_ext_mode(ext: &dyn Extension, event: &ExtensionEvent) -> Option<HookMode> {
    ext.subscriptions()
        .iter()
        .find(|(e, _)| e == event)
        .map(|(_, m)| *m)
}
