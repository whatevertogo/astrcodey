use std::{path::Path, sync::Arc};

use astrcode_core::tool_access::ResourceAccess;
use astrcode_extension_sdk::{
    extension::*,
    tool::{ExecutionMode, Tool, ToolDefinition, ToolError, ToolExecutionContext, ToolResult},
};

use super::{ExtensionRunner, bind_extension_event_sink};

impl ExtensionRunner {
    /// 从 HandlerIndex 缓存收集工具适配器。
    pub async fn collect_tool_adapters_typed(&self, working_dir: &str) -> Vec<Arc<dyn Tool>> {
        let index = self.load_index();
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for (def, handler, ext_id, capabilities) in &index.static_tools {
            let prompt_metadata = index.tool_metadata.get(&def.name).cloned();
            tools.push(Arc::new(HandlerTool {
                definition: def.clone(),
                handler: Arc::clone(handler),
                prompt_metadata,
                working_dir: working_dir.to_string(),
                extension_id: ext_id.clone(),
                capabilities: capabilities.clone(),
                event_declarations: index
                    .extension_event_decls
                    .get(ext_id)
                    .cloned()
                    .unwrap_or_default(),
            }));
        }
        for (ext_id, discovery, capabilities) in &index.tool_discoveries {
            match tokio::time::timeout(self.timeout, discovery.discover(working_dir)).await {
                Ok(discovered) => {
                    for discovered_tool in discovered {
                        tools.push(Arc::new(HandlerTool {
                            definition: discovered_tool.definition,
                            handler: discovered_tool.handler,
                            prompt_metadata: discovered_tool.prompt_metadata,
                            working_dir: working_dir.to_string(),
                            extension_id: ext_id.clone(),
                            capabilities: capabilities.clone(),
                            event_declarations: index
                                .extension_event_decls
                                .get(ext_id)
                                .cloned()
                                .unwrap_or_default(),
                        }));
                    }
                },
                Err(_) => {
                    tracing::warn!("tool discovery timed out");
                },
            }
        }
        tools
    }
}

/// 类型化工具适配器，将 `ToolHandler` 包装为 `Tool` trait 实现。
struct HandlerTool {
    definition: ToolDefinition,
    handler: Arc<dyn ToolHandler>,
    prompt_metadata: Option<astrcode_extension_sdk::tool::ToolPromptMetadata>,
    working_dir: String,
    extension_id: String,
    capabilities: Vec<ExtensionCapability>,
    event_declarations: Vec<ExtensionEventDecl>,
}

// Providers occasionally stringify booleans despite the declared tool schema.
// Normalize only schema-declared boolean fields at the plugin boundary so HTTP,
// configuration and persistence DTOs remain strict.
pub(super) fn normalize_stringified_booleans(
    arguments: &mut serde_json::Value,
    schema: &serde_json::Value,
) -> usize {
    match arguments {
        serde_json::Value::String(raw) if schema["type"] == "boolean" => {
            let normalized = match raw.trim().to_ascii_lowercase().as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };
            if let Some(normalized) = normalized {
                *arguments = serde_json::Value::Bool(normalized);
                1
            } else {
                0
            }
        },
        serde_json::Value::Object(values) => schema["properties"]
            .as_object()
            .map(|properties| {
                values
                    .iter_mut()
                    .filter_map(|(name, value)| {
                        properties
                            .get(name)
                            .map(|field_schema| normalize_stringified_booleans(value, field_schema))
                    })
                    .sum()
            })
            .unwrap_or_default(),
        serde_json::Value::Array(values) => match &schema["items"] {
            serde_json::Value::Array(item_schemas) => values
                .iter_mut()
                .zip(item_schemas)
                .map(|(value, item_schema)| normalize_stringified_booleans(value, item_schema))
                .sum(),
            serde_json::Value::Object(_) => values
                .iter_mut()
                .map(|value| normalize_stringified_booleans(value, &schema["items"]))
                .sum(),
            _ => 0,
        },
        _ => 0,
    }
}

#[async_trait::async_trait]
impl Tool for HandlerTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.definition.execution_mode
    }

    fn prompt_metadata(&self) -> Option<astrcode_extension_sdk::tool::ToolPromptMetadata> {
        self.prompt_metadata.clone()
    }

    fn resource_accesses(
        &self,
        _arguments: &serde_json::Value,
        _working_dir: &Path,
    ) -> Result<Vec<ResourceAccess>, ToolError> {
        // SessionControl 工具（如 agent）在父 turn 内只编排子 session，不直接碰文件；
        // 若声明 ResourceAccess::All，冲突图会把同批 agent 调用串行化。
        if self
            .capabilities
            .contains(&ExtensionCapability::SessionControl)
        {
            return Ok(Vec::new());
        }
        Ok(vec![ResourceAccess::all()])
    }

    async fn execute(
        &self,
        mut arguments: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        let normalized_booleans =
            normalize_stringified_booleans(&mut arguments, &self.definition.parameters);
        if normalized_booleans > 0 {
            tracing::debug!(
                extension_id = %self.extension_id,
                tool_name = %self.definition.name,
                normalized_booleans,
                "normalized stringified boolean extension tool arguments"
            );
        }
        let mut ctx = ctx.clone();
        if !self
            .capabilities
            .contains(&ExtensionCapability::SessionControl)
        {
            ctx.capabilities.session.ops = None;
        }
        if !self.capabilities.contains(&ExtensionCapability::MainModel) {
            ctx.capabilities.models.main = None;
            ctx.capabilities.models.tiers.main = None;
        }
        if !self.capabilities.contains(&ExtensionCapability::SmallModel) {
            ctx.capabilities.models.small = None;
            ctx.capabilities.models.tiers.small = None;
        }
        ctx.capabilities.host.extension_event_sink = if self
            .capabilities
            .contains(&ExtensionCapability::EmitEvents)
        {
            ctx.event_tx.clone().and_then(|event_tx| {
                bind_extension_event_sink(&self.extension_id, &self.event_declarations, event_tx)
            })
        } else {
            None
        };
        let mut result = match self
            .handler
            .execute(&self.definition.name, arguments, &self.working_dir, &ctx)
            .await
        {
            Ok(result) => result,
            Err(err) => {
                return Ok(extension_error_result(
                    &self.definition.name,
                    "handler",
                    err,
                ));
            },
        };

        if let Some(outcome_value) = result
            .metadata
            .remove(astrcode_extension_sdk::extension::EXTENSION_TOOL_OUTCOME_KEY)
        {
            match serde_json::from_value::<ExtensionToolOutcome>(outcome_value) {
                Ok(ExtensionToolOutcome::Text { content, is_error }) => {
                    result.content = content;
                    result.is_error = is_error;
                },
                Err(e) => {
                    tracing::warn!(error = %e, "failed to parse ExtensionToolOutcome, treating as plain result");
                },
            }
        }

        Ok(result)
    }
}

/// 将 [`ExtensionError`] 转换为结构化的错误 [`ToolResult`]。
fn extension_error_result(tool_name: &str, extension_id: &str, err: ExtensionError) -> ToolResult {
    use astrcode_extension_sdk::tool::tool_metadata;

    let (message, suggestion) = match &err {
        ExtensionError::NotFound(_) => (
            format!("Tool `{tool_name}` is not available."),
            "This tool may have been unregistered. Try `tool_search_tool` to discover available \
             tools, or proceed without it.",
        ),
        ExtensionError::Timeout(ms) => (
            format!("Tool `{tool_name}` timed out after {ms}ms."),
            "The extension is still processing. Try again with a simpler request, or proceed \
             without this tool.",
        ),
        ExtensionError::Blocked { reason } => (
            format!("Tool `{tool_name}` was blocked: {reason}"),
            "A hook policy prevented this. Read the reason and adjust your approach.",
        ),
        ExtensionError::Internal(message) => (
            format!("Tool `{tool_name}` failed: {message}"),
            "Try different arguments or use a builtin tool as an alternative. Do not retry the \
             identical call.",
        ),
    };

    // suggestion 拼进 content 让 LLM 看到——metadata 不会进 LLM prompt。
    let content = format!("{message}\nSuggestion: {suggestion}");

    let mut metadata = tool_metadata([
        ("extensionId", serde_json::json!(extension_id)),
        ("toolName", serde_json::json!(tool_name)),
        ("suggestion", serde_json::json!(suggestion)),
    ]);
    if let ExtensionError::Timeout(ms) = &err {
        metadata.insert("timeoutMs".into(), serde_json::json!(ms));
    }

    ToolResult::text(content, true, metadata)
}
