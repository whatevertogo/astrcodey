//! Extension context — restricted session/services view for extensions.

use std::collections::HashMap;

use astrcode_core::config::ModelSelection;
use astrcode_core::extension::ExtensionContext;
use astrcode_core::tool::ToolDefinition;

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
}

impl ServerExtensionContext {
    pub fn new(session_id: String, working_dir: String, model_selection: ModelSelection) -> Self {
        Self {
            session_id,
            working_dir,
            model_selection,
            config_values: HashMap::new(),
            tool_defs: HashMap::new(),
        }
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

    async fn emit_custom_event(&self, _name: &str, _data: serde_json::Value) {
        // TODO: Route custom event to session event log
    }

    fn find_tool(&self, name: &str) -> Option<ToolDefinition> {
        self.tool_defs.get(name).cloned()
    }
}
