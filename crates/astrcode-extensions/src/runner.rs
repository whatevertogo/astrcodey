//! Extension runner — dispatches lifecycle events to registered extensions.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use astrcode_core::extension::*;

/// Dispatches lifecycle events to all registered extensions.
///
/// Enforces HookMode semantics:
/// - Blocking: synchronous, can return Block
/// - NonBlocking: spawned as fire-and-forget task
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

    /// Dispatch an event to all registered extensions that subscribe to it.
    ///
    /// Returns `Err` if any Blocking hook returns `Block`.
    pub async fn dispatch(
        &self,
        event: ExtensionEvent,
        ctx: &dyn ExtensionContext,
    ) -> Result<(), ExtensionError> {
        let exts = self.extensions.read().await;

        for ext in exts.iter() {
            let subs = ext.subscriptions();
            let mode = subs.iter().find(|(e, _)| *e == event).map(|(_, m)| *m);

            let Some(mode) = mode else {
                continue;
            };

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
                }
                HookMode::NonBlocking => {
                    let ext = Arc::clone(ext);
                    let evt = event.clone();
                    // Note: ctx is shared, but we need to manage lifetimes
                    tokio::spawn(async move {
                        let _ = ext.on_event(evt, &NoopContext).await;
                    });
                }
                HookMode::Advisory => {
                    let _ = ext.on_event(event.clone(), ctx).await;
                }
            }
        }

        Ok(())
    }
}

/// No-op context for fire-and-forget NonBlocking hooks.
struct NoopContext;

#[async_trait::async_trait]
impl ExtensionContext for NoopContext {
    fn session_id(&self) -> &str {
        ""
    }
    fn working_dir(&self) -> &str {
        ""
    }
    fn model_selection(&self) -> astrcode_core::config::ModelSelection {
        astrcode_core::config::ModelSelection {
            profile_name: String::new(),
            model: String::new(),
            provider_kind: String::new(),
        }
    }
    fn config_value(&self, _key: &str) -> Option<String> {
        None
    }
    async fn emit_custom_event(&self, _name: &str, _data: serde_json::Value) {}
    fn find_tool(&self, _name: &str) -> Option<astrcode_core::tool::ToolDefinition> {
        None
    }
}
