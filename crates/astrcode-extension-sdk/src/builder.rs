//! Convenience adapters for writing extension handlers and tool definitions.

use std::{future::Future, sync::Arc};

use crate::{
    extension::{ExtensionError, ToolHandler},
    tool::{ExecutionMode, ToolDefinition, ToolExecutionContext, ToolOrigin, ToolResult},
};

// ─── handler_fn ──────────────────────────────────────────────────────────

/// Wraps an async closure into `Arc<dyn ToolHandler>`.
///
/// Avoids the boilerplate of defining a struct and implementing `ToolHandler` by hand.
///
/// ```ignore
/// use astrcode_extension_sdk::builder::handler_fn;
///
/// reg.tool(
///     my_tool_def(),
///     handler_fn(|tool_name, arguments, working_dir, ctx| async move {
///         Ok(ToolResult::text("done", false, Default::default()))
///     }),
/// );
/// ```
pub fn handler_fn<F, Fut>(f: F) -> Arc<dyn ToolHandler>
where
    F: Fn(&str, serde_json::Value, &str, &ToolExecutionContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ToolResult, ExtensionError>> + Send + 'static,
{
    Arc::new(FnToolHandler { f })
}

struct FnToolHandler<F> {
    f: F,
}

#[async_trait::async_trait]
impl<F, Fut> ToolHandler for FnToolHandler<F>
where
    F: Fn(&str, serde_json::Value, &str, &ToolExecutionContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ToolResult, ExtensionError>> + Send + 'static,
{
    async fn execute(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        working_dir: &str,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        (self.f)(tool_name, arguments, working_dir, ctx).await
    }
}

// ─── ToolDefinition builder ──────────────────────────────────────────────

/// Builder for [`ToolDefinition`] with sensible defaults.
///
/// ```ignore
/// use astrcode_extension_sdk::builder::tool;
///
/// let def = tool("hello")
///     .description("Say hello to someone")
///     .parameters(json!({
///         "type": "object",
///         "properties": { "name": { "type": "string" } }
///     }))
///     .build();
/// ```
pub fn tool(name: impl Into<String>) -> ToolDefinitionBuilder {
    ToolDefinitionBuilder {
        name: name.into(),
        description: String::new(),
        parameters: serde_json::json!({"type": "object"}),
        execution_mode: ExecutionMode::Sequential,
    }
}

pub struct ToolDefinitionBuilder {
    name: String,
    description: String,
    parameters: serde_json::Value,
    execution_mode: ExecutionMode,
}

impl ToolDefinitionBuilder {
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = desc.into();
        self
    }

    pub fn parameters(mut self, schema: serde_json::Value) -> Self {
        self.parameters = schema;
        self
    }

    pub fn execution_mode(mut self, mode: ExecutionMode) -> Self {
        self.execution_mode = mode;
        self
    }

    pub fn build(self) -> ToolDefinition {
        ToolDefinition {
            name: self.name,
            description: self.description,
            parameters: self.parameters,
            origin: ToolOrigin::Extension,
            execution_mode: self.execution_mode,
        }
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::types::SessionId;

    use super::*;
    use crate::tool::ToolCapabilities;

    #[test]
    fn tool_builder_sets_defaults() {
        let def = tool("test").description("A test tool").build();
        assert_eq!(def.name, "test");
        assert_eq!(def.description, "A test tool");
        assert_eq!(def.origin, ToolOrigin::Extension);
        assert_eq!(def.execution_mode, ExecutionMode::Sequential);
    }

    #[tokio::test]
    async fn handler_fn_dispatches_to_closure() {
        let handler = handler_fn(|_name, _args, _dir, _ctx| async move {
            Ok(ToolResult::text(
                "ok".to_string(),
                false,
                Default::default(),
            ))
        });
        let ctx = ToolExecutionContext {
            session_id: SessionId::new("test"),
            working_dir: String::new(),
            tool_call_id: None,
            event_tx: None,
            capabilities: ToolCapabilities::default(),
        };
        let result = handler
            .execute("test", serde_json::json!({}), "", &ctx)
            .await
            .unwrap();
        assert_eq!(result.content, "ok");
        assert!(!result.is_error);
    }
}
