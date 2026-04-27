//! Extension context — restricted session/services view for extensions.

use std::{collections::HashMap, sync::Arc};

use astrcode_core::{
    config::ModelSelection,
    event::EventPayload,
    extension::{ExtensionContext, PostToolUseInput, PreToolUseInput},
    tool::ToolDefinition,
};
use tokio::sync::mpsc;

/// Concrete implementation of ExtensionContext.
///
/// Provides extensions a controlled, read-oriented view of the session
/// and server services. Extensions cannot modify session state directly;
/// they must go through hooks and event emission.
pub struct ServerExtensionContext {
    session_id: String,
    working_dir: String,
    model_selection: ModelSelection,
    config_values: HashMap<String, String>,
    tool_defs: HashMap<String, ToolDefinition>,
    custom_event_tx: Option<mpsc::UnboundedSender<EventPayload>>,
    pre_tool_use_input: Option<PreToolUseInput>,
    post_tool_use_input: Option<PostToolUseInput>,
}

impl ServerExtensionContext {
    pub fn new(session_id: String, working_dir: String, model_selection: ModelSelection) -> Self {
        Self {
            session_id,
            working_dir,
            model_selection,
            config_values: HashMap::new(),
            tool_defs: HashMap::new(),
            custom_event_tx: None,
            pre_tool_use_input: None,
            post_tool_use_input: None,
        }
    }

    /// Populate config values available to extensions.
    pub fn set_config(&mut self, values: HashMap<String, String>) {
        self.config_values = values;
    }

    /// Populate tool definitions visible to extensions.
    pub fn set_tools(&mut self, tools: HashMap<String, ToolDefinition>) {
        self.tool_defs = tools;
    }

    /// Set the channel for emitting custom events to the session log.
    pub fn set_custom_event_sender(&mut self, tx: mpsc::UnboundedSender<EventPayload>) {
        self.custom_event_tx = Some(tx);
    }

    /// Attach the current tool-call input for PreToolUse hooks.
    pub fn set_pre_tool_use_input(&mut self, input: PreToolUseInput) {
        self.pre_tool_use_input = Some(input);
        self.post_tool_use_input = None;
    }

    /// Attach the current tool-call result for PostToolUse hooks.
    pub fn set_post_tool_use_input(&mut self, input: PostToolUseInput) {
        self.pre_tool_use_input = None;
        self.post_tool_use_input = Some(input);
    }

    /// Build a lightweight snapshot of this context, shareable across threads.
    pub fn snapshot(&self) -> Arc<ServerExtensionContextSnapshot> {
        Arc::new(ServerExtensionContextSnapshot {
            session_id: self.session_id.clone(),
            working_dir: self.working_dir.clone(),
            model_selection: self.model_selection.clone(),
            pre_tool_use_input: self.pre_tool_use_input.clone(),
            post_tool_use_input: self.post_tool_use_input.clone(),
        })
    }
}

/// Lightweight snapshot for use in NonBlocking fire-and-forget hooks.
pub struct ServerExtensionContextSnapshot {
    session_id: String,
    working_dir: String,
    model_selection: ModelSelection,
    pre_tool_use_input: Option<PreToolUseInput>,
    post_tool_use_input: Option<PostToolUseInput>,
}

#[async_trait::async_trait]
impl ExtensionContext for ServerExtensionContextSnapshot {
    fn session_id(&self) -> &str {
        &self.session_id
    }
    fn working_dir(&self) -> &str {
        &self.working_dir
    }
    fn model_selection(&self) -> ModelSelection {
        self.model_selection.clone()
    }
    fn config_value(&self, _key: &str) -> Option<String> {
        None
    }
    async fn emit_custom_event(&self, _name: &str, _data: serde_json::Value) {}
    fn find_tool(&self, _name: &str) -> Option<ToolDefinition> {
        None
    }
    fn pre_tool_use_input(&self) -> Option<PreToolUseInput> {
        self.pre_tool_use_input.clone()
    }
    fn post_tool_use_input(&self) -> Option<PostToolUseInput> {
        self.post_tool_use_input.clone()
    }
    fn log_warn(&self, msg: &str) {
        tracing::warn!("[extension] {msg}");
    }
    fn snapshot(&self) -> Arc<dyn ExtensionContext> {
        Arc::new(ServerExtensionContextSnapshot {
            session_id: self.session_id.clone(),
            working_dir: self.working_dir.clone(),
            model_selection: self.model_selection.clone(),
            pre_tool_use_input: self.pre_tool_use_input.clone(),
            post_tool_use_input: self.post_tool_use_input.clone(),
        })
    }
}

#[async_trait::async_trait]
impl ExtensionContext for ServerExtensionContext {
    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn working_dir(&self) -> &str {
        &self.working_dir
    }

    fn model_selection(&self) -> ModelSelection {
        self.model_selection.clone()
    }

    fn config_value(&self, key: &str) -> Option<String> {
        self.config_values.get(key).cloned()
    }

    async fn emit_custom_event(&self, name: &str, data: serde_json::Value) {
        if let Some(tx) = &self.custom_event_tx {
            let _ = tx.send(EventPayload::Custom {
                name: name.into(),
                data,
            });
        }
    }

    fn find_tool(&self, name: &str) -> Option<ToolDefinition> {
        self.tool_defs.get(name).cloned()
    }

    fn pre_tool_use_input(&self) -> Option<PreToolUseInput> {
        self.pre_tool_use_input.clone()
    }

    fn post_tool_use_input(&self) -> Option<PostToolUseInput> {
        self.post_tool_use_input.clone()
    }

    fn log_warn(&self, msg: &str) {
        tracing::warn!("[extension] {msg}");
    }

    fn snapshot(&self) -> Arc<dyn ExtensionContext> {
        Arc::new(ServerExtensionContextSnapshot {
            session_id: self.session_id.clone(),
            working_dir: self.working_dir.clone(),
            model_selection: self.model_selection.clone(),
            pre_tool_use_input: self.pre_tool_use_input.clone(),
            post_tool_use_input: self.post_tool_use_input.clone(),
        })
    }
}
