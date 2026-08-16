use std::{
    path::PathBuf,
    sync::{Arc, Weak},
    time::Duration,
};

use astrcode_core::tool::{
    Tool, ToolError, ToolExecutionContext, ToolExecutionPolicy, ToolExecutionResult,
    ToolPlanningContext,
};
use astrcode_extension_sdk::{
    extension::{
        internal::{tool_context, tool_discovery_context, tool_plan_context},
        *,
    },
    runtime_ports::{
        ToolCatalogCompleteness, ToolCatalogDiagnostic, ToolCatalogProvider, ToolCatalogScope,
        ToolCatalogSnapshot,
    },
    tool::{ToolDefinition, ToolPlan, ToolResult},
};

use super::{
    ExtensionCallContextFactory, ExtensionCallContextInput, ExtensionGenerationEntry,
    ExtensionRunner, ExtensionView, tool_catalog_cache::CatalogCacheLookup,
};

impl ExtensionView {
    /// 从 HandlerIndex 缓存收集工具适配器。
    #[cfg(any(test, feature = "testing"))]
    pub async fn tool_catalog_snapshot_typed(&self, working_dir: &str) -> ToolCatalogSnapshot {
        let scope = ToolCatalogScope {
            working_dir: working_dir.to_owned(),
        };
        self.tool_catalog_snapshot_for_scope(&scope).await
    }

    pub(super) async fn tool_catalog_snapshot_for_scope(
        &self,
        scope: &ToolCatalogScope,
    ) -> ToolCatalogSnapshot {
        loop {
            match self.index.tool_catalog_cache.lookup_or_reserve(scope) {
                CatalogCacheLookup::Hit(snapshot) => return snapshot,
                CatalogCacheLookup::Wait(mut notification) => {
                    let _ = notification.changed().await;
                },
                CatalogCacheLookup::Build(build) => {
                    let snapshot = self.build_tool_catalog_snapshot(scope).await;
                    build.complete(snapshot.clone());
                    return snapshot;
                },
            }
        }
    }

    async fn build_tool_catalog_snapshot(&self, scope: &ToolCatalogScope) -> ToolCatalogSnapshot {
        let working_dir = &scope.working_dir;
        let index = &self.index;
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut diagnostics = Vec::new();
        for entry in &index.static_tools {
            tools.push(Arc::new(HandlerTool::new(
                entry.definition.clone(),
                entry.execution_policy,
                Arc::clone(&entry.handler),
                entry.prompt_metadata.clone(),
                working_dir,
                &entry.generation,
                self,
            )));
        }
        for entry in &index.tool_discoveries {
            let ext_id = entry.generation.extension_id.as_ref();
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
                    let cancellation = call.cancellation().clone();
                    let ctx =
                        tool_discovery_context(call, PathBuf::from(working_dir), self.generation());
                    self.run_recorded_hook(
                        ext_id,
                        "tool_discovery",
                        cancellation,
                        entry.handler.discover(ctx),
                    )
                    .await
                },
                Err(error) => Err(error),
            };
            match discovered {
                Ok(discovered) => {
                    for discovered_tool in discovered.into_tools() {
                        let (definition, execution_policy, handler, prompt_metadata) =
                            discovered_tool.into_parts();
                        tools.push(Arc::new(HandlerTool::new(
                            definition,
                            execution_policy,
                            handler,
                            prompt_metadata,
                            working_dir,
                            &entry.generation,
                            self,
                        )));
                    }
                },
                Err(error) => {
                    let message = error.to_string();
                    tracing::warn!(extension_id = %ext_id, error = %message);
                    diagnostics.push(ToolCatalogDiagnostic {
                        source: ext_id.to_owned(),
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
    #[cfg(any(test, feature = "testing"))]
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
    execution_policy: ToolExecutionPolicy,
    handler: Arc<dyn ToolHandler>,
    prompt_metadata: Option<astrcode_extension_sdk::tool::ToolPromptMetadata>,
    working_dir: String,
    extension_id: Arc<str>,
    capabilities: Arc<[ExtensionCapability]>,
    generation: Weak<ExtensionGenerationEntry>,
    operation_timeout: Duration,
    call_context_factory: ExtensionCallContextFactory,
    public_http_dispatcher: Arc<dyn crate::host_router::PublicHttpDispatcher>,
}

impl HandlerTool {
    fn new(
        definition: ToolDefinition,
        execution_policy: ToolExecutionPolicy,
        handler: Arc<dyn ToolHandler>,
        prompt_metadata: Option<astrcode_extension_sdk::tool::ToolPromptMetadata>,
        working_dir: &str,
        generation: &Arc<ExtensionGenerationEntry>,
        view: &ExtensionView,
    ) -> Self {
        Self {
            definition,
            execution_policy,
            handler,
            prompt_metadata,
            working_dir: working_dir.to_owned(),
            extension_id: Arc::clone(&generation.extension_id),
            capabilities: Arc::clone(&generation.capabilities),
            generation: Arc::downgrade(generation),
            operation_timeout: view.operation_timeout,
            call_context_factory: view.call_context_factory.clone(),
            public_http_dispatcher: view.public_http_dispatcher_for_index(&view.index),
        }
    }
}

#[async_trait::async_trait]
impl Tool for HandlerTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn execution_policy(&self) -> ToolExecutionPolicy {
        self.execution_policy
    }

    fn prompt_metadata(&self) -> Option<astrcode_extension_sdk::tool::ToolPromptMetadata> {
        self.prompt_metadata.clone()
    }

    async fn plan(
        &self,
        arguments: &serde_json::Value,
        ctx: &ToolPlanningContext,
    ) -> Result<ToolPlan, ToolError> {
        let generation = self
            .generation
            .upgrade()
            .ok_or_else(|| ToolError::NotFound(self.definition.name.clone()))?;
        let draining = generation.admission.draining_token();
        let _admission = generation
            .admission
            .acquire()
            .await
            .map_err(extension_plan_error)?;
        let cancellation = ctx.cancellation().child_token();
        let plan_context = tool_plan_context(
            generation.extension_id.to_string(),
            ctx.session_id.clone(),
            ctx.turn_id.as_ref().map(ToString::to_string),
            PathBuf::from(&ctx.working_dir),
            self.definition.name.clone(),
            ctx.tool_call_id.clone(),
            arguments.clone(),
            cancellation.clone(),
        );
        let planning =
            tokio::time::timeout(self.operation_timeout, self.handler.plan(plan_context));
        tokio::select! {
            biased;
            () = ctx.cancellation().cancelled() => {
                cancellation.cancel();
                Err(ToolError::Execution("tool planning cancelled".into()))
            },
            () = draining.cancelled() => {
                cancellation.cancel();
                Err(extension_plan_error(generation.admission.draining_error()))
            },
            result = planning => match result {
                Ok(result) => result.map_err(extension_plan_error),
                Err(_) => Err(ToolError::Timeout(self.operation_timeout.as_millis() as u64)),
            },
        }
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolExecutionResult, ToolError> {
        let Some(generation) = self.generation.upgrade() else {
            return Ok(extension_error_result(
                &self.definition.name,
                &self.extension_id,
                ExtensionError::NotFound("extension generation is no longer available".into()),
            )
            .into());
        };
        let draining = generation.admission.draining_token();
        let _admission = match generation.admission.acquire().await {
            Ok(permit) => permit,
            Err(error) => {
                return Ok(extension_error_result(
                    &self.definition.name,
                    &generation.extension_id,
                    error,
                )
                .into());
            },
        };
        let resource_lease = ctx.resource_lease().cloned().ok_or_else(|| {
            ToolError::Execution(
                "extension tool execution requires an approved resource lease".into(),
            )
        })?;
        let session_id = ctx.scope.session_id.clone();
        let turn_id = ctx.turn_id().map(ToString::to_string);
        let working_dir = PathBuf::from(&self.working_dir);
        let call = self.call_context_factory.make_extension_call_context(
            &generation.extension_id,
            generation.instance_id,
            &generation.capabilities,
            &generation.custom_event_declarations,
            generation.tasks.clone(),
            ExtensionCallContextInput {
                session_id: Some(session_id.clone()),
                tool_call_id: ctx.scope.tool_call_id.clone(),
                working_dir: Some(working_dir.clone()),
                session_store_dir: ctx.capabilities.paths.store_dir.clone(),
                event_tx: ctx.scope.event_tx.clone(),
                event_causation: None,
                resource_lease: Some(resource_lease),
                file_observation_store: ctx.capabilities.files.observation_store.clone(),
                tool_result_reader: ctx.capabilities.host.result_reader.clone(),
                llm_providers: ctx.capabilities.host.llm_providers.clone(),
                generation_gate: generation.generation_gate.clone(),
                public_http_dispatcher: Some(Arc::clone(&self.public_http_dispatcher)),
                cancellation: ctx.cancellation().child_token(),
            },
        );
        let _call_lifetime = call.cancellation().clone().drop_guard();
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
        let ctx = tool_context(
            call,
            session_id,
            turn_id,
            working_dir,
            self.definition.name.clone(),
            ctx.scope.tool_call_id.clone(),
            arguments,
            main_model_id,
            small_model_id,
            available_tools,
        );
        let execution = async {
            tokio::select! {
                biased;
                result = self.handler.execute(ctx) => result,
                () = draining.cancelled() => {
                    Err(generation.admission.draining_error())
                },
            }
        };
        let execution = match self.execution_policy.timeout {
            Some(timeout) => match tokio::time::timeout(timeout, execution).await {
                Ok(result) => result,
                Err(_) => Err(ExtensionError::Timeout(timeout.as_millis() as u64)),
            },
            None => execution.await,
        };
        let result = match execution {
            Ok(result) => result,
            Err(err) => {
                return Ok(extension_error_result(
                    &self.definition.name,
                    &generation.extension_id,
                    err,
                )
                .into());
            },
        };

        Ok(result)
    }
}

fn extension_plan_error(error: ExtensionError) -> ToolError {
    match error {
        ExtensionError::InvalidInput { message, .. } => ToolError::InvalidArguments(message),
        ExtensionError::Timeout(timeout_ms) => ToolError::Timeout(timeout_ms),
        ExtensionError::Blocked { reason } => ToolError::Blocked { reason },
        other => ToolError::Execution(other.to_string()),
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
        ExtensionError::Cancelled => (
            format!("Tool `{tool_name}` was cancelled."),
            "The current operation was cancelled; retry only if the caller starts a new attempt.",
        ),
        ExtensionError::Draining { .. } => (
            format!("Tool `{tool_name}` is temporarily unavailable while its extension reloads."),
            "Retry after the extension finishes reloading, or proceed without this tool.",
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
            "Try different arguments or use another available tool. Do not retry the identical \
             call.",
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
    if matches!(&err, ExtensionError::Draining { .. }) {
        metadata.insert(
            "errorCode".into(),
            serde_json::json!(
                astrcode_extension_sdk::wire::WireErrorCode::ExtensionDraining.as_str()
            ),
        );
    }
    if matches!(&err, ExtensionError::Cancelled) {
        metadata.insert(
            "errorCode".into(),
            serde_json::json!(astrcode_extension_sdk::wire::WireErrorCode::Cancelled.as_str()),
        );
    }
    if let ExtensionError::Host(error) = &err {
        metadata.insert("errorCode".into(), serde_json::json!(error.code));
        metadata.insert("retryable".into(), serde_json::json!(error.retryable));
        if let Some(hint) = &error.hint {
            metadata.insert("hint".into(), serde_json::json!(hint));
        }
        if let Some(details) = &error.details {
            metadata.insert("errorDetails".into(), details.clone());
        }
    }

    ToolResult::text(content, true, metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_errors_keep_their_contract_error_codes() {
        let cases = [
            (
                ExtensionError::Draining {
                    extension_id: "extension-a".into(),
                },
                astrcode_extension_sdk::wire::WireErrorCode::ExtensionDraining.as_str(),
                None,
            ),
            (
                ExtensionError::Cancelled,
                astrcode_extension_sdk::wire::WireErrorCode::Cancelled.as_str(),
                None,
            ),
            (
                ExtensionError::InvalidInput {
                    code: astrcode_extension_sdk::wire::WireErrorCode::InvalidInput
                        .as_str()
                        .into(),
                    message: "bad input".into(),
                    hint: None,
                },
                astrcode_extension_sdk::wire::WireErrorCode::InvalidInput.as_str(),
                None,
            ),
            (
                ExtensionError::Host(
                    astrcode_extension_sdk::wire::protocol::ErrorPayload {
                        code: "future_worker_error".into(),
                        message: "worker failed".into(),
                        hint: None,
                        retryable: false,
                        details: Some(serde_json::json!({ "revision": 3 })),
                    }
                    .into(),
                ),
                "future_worker_error",
                Some(serde_json::json!({ "revision": 3 })),
            ),
        ];

        for (error, expected_code, expected_details) in cases {
            let result = extension_error_result("probe", "extension-a", error);
            assert_eq!(
                result.metadata.get("errorCode"),
                Some(&serde_json::json!(expected_code))
            );
            assert_eq!(
                result.metadata.get("errorDetails"),
                expected_details.as_ref()
            );
        }
    }
}
