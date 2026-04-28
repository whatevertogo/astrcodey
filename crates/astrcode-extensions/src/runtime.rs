//! Shared extension runtime — lazy binding pattern borrowed from pi-mono.
//!
//! Extensions are loaded before the server fully boots. Their registrations
//! (tools, commands) are queued into this runtime. Once the server is ready,
//! `bind()` injects live session capabilities.

use std::sync::{Arc, Mutex, RwLock};

use astrcode_core::tool::ToolDefinition;

/// Generic session creation primitive. Server implements this, runner holds it,
/// extensions never see it.
#[async_trait::async_trait]
pub trait SessionSpawner: Send + Sync {
    /// Create a child session and run one turn.
    async fn spawn(
        &self,
        parent_session_id: &str,
        request: SpawnRequest,
    ) -> Result<SpawnResult, String>;
}

/// Request to spawn a child session turn.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub name: String,
    pub system_prompt: String,
    pub user_prompt: String,
    pub working_dir: String,
    pub allowed_tools: Vec<String>,
    pub model_preference: Option<String>,
}

/// Result of a spawned child session turn.
pub struct SpawnResult {
    pub content: String,
    pub child_session_id: String,
}

/// Shared state for all loaded extensions.
///
/// Created by the loader, then `bind()` is called after the server is ready
/// to inject live session capabilities.
pub struct ExtensionRuntime {
    /// Tools registered by extensions during loading.
    pending_tools: Mutex<Vec<ToolDefinition>>,
    /// Injected session spawner. None until `bind()` is called.
    /// Arc enables clone-then-drop-guard-before-await.
    spawner: RwLock<Option<Arc<dyn SessionSpawner>>>,
}

impl Default for ExtensionRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl ExtensionRuntime {
    pub fn new() -> Self {
        Self {
            pending_tools: Mutex::new(Vec::new()),
            spawner: RwLock::new(None),
        }
    }

    /// Bind the live session spawner. Called once after server boot.
    pub fn bind(&self, spawner: Arc<dyn SessionSpawner>) {
        *self.spawner.write().unwrap() = Some(spawner);
    }

    /// Queue a tool registration. Called from NativeExtension during factory().
    pub fn register_tool(&self, def: ToolDefinition) {
        self.pending_tools.lock().unwrap().push(def);
    }

    /// Take all pending tool registrations (consumes them).
    pub fn take_pending_tools(&self) -> Vec<ToolDefinition> {
        std::mem::take(&mut *self.pending_tools.lock().unwrap())
    }

    /// Run a child session turn. Errors if `bind()` hasn't been called yet.
    pub async fn spawn(
        &self,
        parent_session_id: &str,
        request: SpawnRequest,
    ) -> Result<SpawnResult, String> {
        let spawner = {
            let guard = self.spawner.read().unwrap();
            match &*guard {
                Some(s) => Arc::clone(s),
                None => {
                    return Err("ExtensionRuntime not bound — bind() must be called before \
                                spawn()"
                        .into());
                },
            }
        };
        // Guard is dropped here, before the await
        spawner.spawn(parent_session_id, request).await
    }
}
