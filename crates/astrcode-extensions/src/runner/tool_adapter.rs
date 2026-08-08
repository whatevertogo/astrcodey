use std::{
    path::{Path, PathBuf},
    sync::{Arc, Weak},
};

use astrcode_core::tool::access::ResourceAccess;
use astrcode_extension_sdk::{
    extension::*,
    runtime_ports::{
        ToolCatalogCompleteness, ToolCatalogDiagnostic, ToolCatalogProvider, ToolCatalogScope,
        ToolCatalogSnapshot,
    },
    tool::{
        ExecutionMode, Tool, ToolDefinition, ToolError, ToolExecutionContext, ToolExecutionResult,
        ToolResult,
    },
};

use super::{
    ExtensionCallContextFactory, ExtensionCallContextInput, ExtensionRunner, ExtensionView,
    HandlerIndex,
};

impl ExtensionView {
    /// 从 HandlerIndex 缓存收集工具适配器。
    pub async fn tool_catalog_snapshot_typed(&self, working_dir: &str) -> ToolCatalogSnapshot {
        let scope = ToolCatalogScope {
            working_dir: working_dir.to_owned(),
            session_store_dir: None,
        };
        self.tool_catalog_snapshot_for_scope(&scope).await
    }

    pub(super) async fn tool_catalog_snapshot_for_scope(
        &self,
        scope: &ToolCatalogScope,
    ) -> ToolCatalogSnapshot {
        let working_dir = &scope.working_dir;
        let index = &self.index;
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut diagnostics = Vec::new();
        for (def, handler, ext_id, capabilities) in &index.static_tools {
            let prompt_metadata = index.tool_metadata.get(&def.name).cloned();
            tools.push(Arc::new(HandlerTool::new(
                def.clone(),
                Arc::clone(handler),
                prompt_metadata,
                working_dir,
                ext_id,
                capabilities,
                self,
            )));
        }
        for (ext_id, discovery, capabilities) in &index.tool_discoveries {
            let cancellation = tokio_util::sync::CancellationToken::new();
            let call = self.make_registered_extension_call_context(
                ext_id,
                ExtensionCallContextInput {
                    working_dir: Some(PathBuf::from(working_dir)),
                    ..ExtensionCallContextInput::unscoped(cancellation.clone())
                },
            );
            let discovered = match call {
                Ok(call) => {
                    let ctx = ToolDiscoveryContext::from_runtime(call, self.generation());
                    self.run_recorded_hook(
                        ext_id,
                        "tool_discovery",
                        cancellation,
                        discovery.discover(ctx),
                    )
                    .await
                },
                Err(error) => Err(error),
            };
            match discovered {
                Ok(discovered) => {
                    for discovered_tool in discovered.into_tools() {
                        let (definition, handler, prompt_metadata) = discovered_tool.into_parts();
                        tools.push(Arc::new(HandlerTool::new(
                            definition,
                            handler,
                            prompt_metadata,
                            working_dir,
                            ext_id,
                            capabilities,
                            self,
                        )));
                    }
                },
                Err(error) => {
                    let message = error.to_string();
                    tracing::warn!(extension_id = %ext_id, error = %message);
                    diagnostics.push(ToolCatalogDiagnostic {
                        source: ext_id.clone(),
                        message,
                    });
                },
            }
        }
        ToolCatalogSnapshot {
            revision: self.generation(),
            tools,
            completeness: if diagnostics.is_empty() {
                ToolCatalogCompleteness::Complete
            } else {
                ToolCatalogCompleteness::Partial
            },
            diagnostics,
        }
    }
}

impl ExtensionRunner {
    pub async fn tool_catalog_snapshot_typed(&self, working_dir: &str) -> ToolCatalogSnapshot {
        self.extension_view()
            .await
            .tool_catalog_snapshot_typed(working_dir)
            .await
    }
}

#[async_trait::async_trait]
impl ToolCatalogProvider for ExtensionView {
    fn revision(&self) -> u64 {
        self.generation()
    }

    async fn tool_catalog(
        &self,
        scope: &ToolCatalogScope,
    ) -> Result<ToolCatalogSnapshot, ExtensionError> {
        Ok(self.tool_catalog_snapshot_for_scope(scope).await)
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
    call_context_factory: ExtensionCallContextFactory,
    index: Weak<HandlerIndex>,
}

impl HandlerTool {
    fn new(
        definition: ToolDefinition,
        handler: Arc<dyn ToolHandler>,
        prompt_metadata: Option<astrcode_extension_sdk::tool::ToolPromptMetadata>,
        working_dir: &str,
        extension_id: &str,
        capabilities: &[ExtensionCapability],
        view: &ExtensionView,
    ) -> Self {
        Self {
            definition,
            handler,
            prompt_metadata,
            working_dir: working_dir.to_owned(),
            extension_id: extension_id.to_owned(),
            capabilities: capabilities.to_vec(),
            event_declarations: view
                .index
                .extension_event_decls
                .get(extension_id)
                .cloned()
                .unwrap_or_default(),
            call_context_factory: view.call_context_factory.clone(),
            index: Arc::downgrade(&view.index),
        }
    }
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
        // Session-control tools coordinate through their own runtime state and
        // do not touch file resources.
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
    ) -> Result<ToolExecutionResult, ToolError> {
        let Some(active_index) = self.index.upgrade() else {
            return Ok(extension_error_result(
                &self.definition.name,
                &self.extension_id,
                ExtensionError::NotFound("extension generation is no longer available".into()),
            )
            .into());
        };
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
        let Some(tasks) = active_index
            .extension_tasks
            .get(&self.extension_id)
            .cloned()
        else {
            return Ok(extension_error_result(
                &self.definition.name,
                &self.extension_id,
                ExtensionError::Internal("extension task scope is unavailable".into()),
            )
            .into());
        };
        let call = self.call_context_factory.make_extension_call_context(
            &self.extension_id,
            &self.capabilities,
            &self.event_declarations,
            tasks,
            ExtensionCallContextInput {
                session_id: Some(ctx.scope.session_id.clone()),
                turn_id: ctx.turn_id().map(ToString::to_string),
                tool_call_id: ctx.scope.tool_call_id.clone(),
                working_dir: Some(PathBuf::from(&self.working_dir)),
                session_store_dir: ctx.capabilities.paths.store_dir.clone(),
                event_tx: ctx.scope.event_tx.clone(),
                cancellation: ctx.cancellation().clone(),
            },
        );
        let main_model_id = self
            .capabilities
            .contains(&ExtensionCapability::MainModel)
            .then(|| ctx.capabilities.models.tiers.main.clone())
            .flatten();
        let small_model_id = self
            .capabilities
            .contains(&ExtensionCapability::SmallModel)
            .then(|| ctx.capabilities.models.tiers.small.clone())
            .flatten();
        let available_tools = ctx
            .capabilities
            .host
            .available_tools
            .clone()
            .unwrap_or_default();
        let ctx = ToolContext::from_runtime(
            call,
            self.definition.name.clone(),
            ctx.scope.tool_call_id.clone(),
            arguments,
            main_model_id,
            small_model_id,
            available_tools,
        );
        let result = match self.handler.execute(ctx).await {
            Ok(result) => result,
            Err(err) => {
                return Ok(
                    extension_error_result(&self.definition.name, &self.extension_id, err).into(),
                );
            },
        };

        drop(active_index);
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
        ExtensionError::InvalidInput { message, hint, .. } => (
            format!("Tool `{tool_name}` received invalid input: {message}"),
            hint.as_deref()
                .unwrap_or("Check the tool arguments against its declared schema."),
        ),
        ExtensionError::Host(error) => (
            format!("Tool `{tool_name}` host call failed: {}", error.message),
            error.hint.as_deref().unwrap_or(if error.retryable {
                "The host marked this failure retryable; verify current state before retrying."
            } else {
                "Check the extension capability and current call context before retrying."
            }),
        ),
        ExtensionError::Path(error) => (
            format!("Tool `{tool_name}` path is unavailable: {error}"),
            "Run the tool from the session context required by this extension.",
        ),
        ExtensionError::Internal(message) => (
            format!("Tool `{tool_name}` failed: {message}"),
            "Try different arguments or use a builtin tool as an alternative. Do not retry the \
             identical call.",
        ),
        registration_error => (
            format!("Tool `{tool_name}` failed: {registration_error}"),
            "The extension is misconfigured. Disable or update it before retrying this tool.",
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
    if let ExtensionError::InvalidInput { code, hint, .. } = &err {
        metadata.insert("errorCode".into(), serde_json::json!(code));
        if let Some(hint) = hint {
            metadata.insert("hint".into(), serde_json::json!(hint));
        }
    }
    if let ExtensionError::Host(error) = &err {
        metadata.insert("errorCode".into(), serde_json::json!(error.code));
        metadata.insert("retryable".into(), serde_json::json!(error.retryable));
        if let Some(hint) = &error.hint {
            metadata.insert("hint".into(), serde_json::json!(hint));
        }
    }

    ToolResult::text(content, true, metadata)
}
