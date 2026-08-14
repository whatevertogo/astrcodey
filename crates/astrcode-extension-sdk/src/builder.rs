//! Convenience adapters for writing extension handlers and tool definitions.

use std::{future::Future, sync::Arc};

use serde::de::DeserializeOwned;

use crate::{
    extension::{
        CommandAvailability, CommandContext, CommandExecution, CommandHandler,
        ContinueAfterStopContext, ContinueAfterStopResult, CustomEventDeclaration,
        CustomEventDelivery, DEFAULT_CUSTOM_EVENT_MAX_PAYLOAD_BYTES,
        DEFAULT_CUSTOM_EVENT_SCHEMA_VERSION, ExtensionCapability, ExtensionCommandResult,
        ExtensionError, ExtensionHttpAccess, ExtensionHttpHandler, ExtensionHttpMethod,
        ExtensionHttpResponse, ExtensionHttpRoute, ExtensionManifest, ExtensionManifestError,
        HttpContext, Keybinding, SessionCommandKind, SlashCommand, StatusItem, ToolContext,
        ToolHandler, ToolPlanContext,
    },
    tool::{
        ExecutionMode, ToolDefinition, ToolExecutionResult, ToolOrigin, ToolPlan,
        ToolPromptMetadata,
    },
    transport::TransportFeature,
};

// ─── Extension manifest builder ────────────────────────────────────────

pub fn manifest(id: impl Into<String>) -> ExtensionManifestBuilder {
    let id = id.into();
    ExtensionManifestBuilder {
        name: id.clone(),
        id,
        version: String::new(),
        description: None,
        capabilities: Vec::new(),
        required_transport_features: Vec::new(),
    }
}

pub struct ExtensionManifestBuilder {
    id: String,
    name: String,
    version: String,
    description: Option<String>,
    capabilities: Vec<ExtensionCapability>,
    required_transport_features: Vec<TransportFeature>,
}

impl ExtensionManifestBuilder {
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn capability(mut self, capability: ExtensionCapability) -> Self {
        if !self.capabilities.contains(&capability) {
            self.capabilities.push(capability);
        }
        self
    }

    pub fn requires_transport(mut self, feature: TransportFeature) -> Self {
        if !self.required_transport_features.contains(&feature) {
            self.required_transport_features.push(feature);
        }
        self
    }

    pub fn build(self) -> ExtensionManifest {
        ExtensionManifest::new(
            self.id,
            self.name,
            self.version,
            self.description,
            self.capabilities,
            self.required_transport_features,
        )
    }

    pub fn build_checked(self) -> Result<ExtensionManifest, ExtensionManifestError> {
        let manifest = self.build();
        manifest.validate()?;
        Ok(manifest)
    }
}

// ─── Pure declaration builders ─────────────────────────────────────────

pub fn command(name: impl Into<String>) -> SlashCommandBuilder {
    SlashCommandBuilder {
        command: SlashCommand {
            name: name.into(),
            description: String::new(),
            args_schema: None,
            requires_idle: false,
            argument_completions: false,
            priority: 0,
            availability: CommandAvailability::AllTransports,
            execution: CommandExecution::Extension,
        },
    }
}

pub struct SlashCommandBuilder {
    command: SlashCommand,
}

impl SlashCommandBuilder {
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.command.description = description.into();
        self
    }

    pub fn arguments(mut self, schema: serde_json::Value) -> Self {
        self.command.args_schema = Some(schema);
        self
    }

    pub fn requires_idle(mut self, requires_idle: bool) -> Self {
        self.command.requires_idle = requires_idle;
        self
    }

    pub fn argument_completions(mut self, enabled: bool) -> Self {
        self.command.argument_completions = enabled;
        self
    }

    pub fn priority(mut self, priority: i32) -> Self {
        self.command.priority = priority;
        self
    }

    pub fn availability(mut self, availability: CommandAvailability) -> Self {
        self.command.availability = availability;
        self
    }

    pub fn host_command(mut self, command: SessionCommandKind) -> Self {
        self.command.execution = CommandExecution::Host(command);
        self
    }

    pub fn build(self) -> SlashCommand {
        self.command
    }
}

pub fn http_route(
    method: ExtensionHttpMethod,
    path: impl Into<String>,
) -> ExtensionHttpRouteBuilder {
    ExtensionHttpRouteBuilder {
        route: ExtensionHttpRoute::authenticated(method, path),
    }
}

pub struct ExtensionHttpRouteBuilder {
    route: ExtensionHttpRoute,
}

impl ExtensionHttpRouteBuilder {
    pub fn public(mut self) -> Self {
        self.route.access = ExtensionHttpAccess::Public;
        self
    }

    pub fn authenticated(mut self) -> Self {
        self.route.access = ExtensionHttpAccess::Authenticated;
        self
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.route.description = description.into();
        self
    }

    pub fn max_body_bytes(mut self, max_body_bytes: usize) -> Self {
        self.route.max_body_bytes = max_body_bytes;
        self
    }

    pub fn build(self) -> ExtensionHttpRoute {
        self.route
    }
}

pub fn keybinding(key: impl Into<String>, command: impl Into<String>) -> KeybindingBuilder {
    KeybindingBuilder {
        binding: Keybinding {
            key: key.into(),
            command: command.into(),
            arguments: String::new(),
            description: String::new(),
        },
    }
}

pub struct KeybindingBuilder {
    binding: Keybinding,
}

impl KeybindingBuilder {
    pub fn arguments(mut self, arguments: impl Into<String>) -> Self {
        self.binding.arguments = arguments.into();
        self
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.binding.description = description.into();
        self
    }

    pub fn build(self) -> Keybinding {
        self.binding
    }
}

pub fn status_item(id: impl Into<String>, text: impl Into<String>) -> StatusItemBuilder {
    StatusItemBuilder {
        item: StatusItem {
            id: id.into(),
            text: text.into(),
            priority: 0,
            tooltip: None,
        },
    }
}

pub struct StatusItemBuilder {
    item: StatusItem,
}

impl StatusItemBuilder {
    pub fn priority(mut self, priority: i32) -> Self {
        self.item.priority = priority;
        self
    }

    pub fn tooltip(mut self, tooltip: impl Into<String>) -> Self {
        self.item.tooltip = Some(tooltip.into());
        self
    }

    pub fn build(self) -> StatusItem {
        self.item
    }
}

pub fn custom_event(event_type: impl Into<String>) -> CustomEventDeclarationBuilder {
    CustomEventDeclarationBuilder {
        event: CustomEventDeclaration {
            event_type: event_type.into(),
            schema_version: DEFAULT_CUSTOM_EVENT_SCHEMA_VERSION,
            delivery: CustomEventDelivery::SessionDurable,
            max_payload_bytes: DEFAULT_CUSTOM_EVENT_MAX_PAYLOAD_BYTES,
        },
    }
}

pub struct CustomEventDeclarationBuilder {
    event: CustomEventDeclaration,
}

impl CustomEventDeclarationBuilder {
    pub fn schema_version(mut self, schema_version: u32) -> Self {
        self.event.schema_version = schema_version;
        self
    }

    pub fn delivery(mut self, delivery: CustomEventDelivery) -> Self {
        self.event.delivery = delivery;
        self
    }

    pub fn max_payload_bytes(mut self, max_payload_bytes: usize) -> Self {
        self.event.max_payload_bytes = max_payload_bytes;
        self
    }

    pub fn build(self) -> CustomEventDeclaration {
        self.event
    }
}

// ─── Handler closure adapters ────────────────────────────────────────────

/// Wraps an async closure into an execute-only [`CommandHandler`].
///
/// Commands that advertise argument completion should implement [`CommandHandler`] directly so
/// their completion behavior and [`CommandHandler::supports_argument_completions`] stay explicit.
pub fn command_handler<F, Fut>(f: F) -> Arc<dyn CommandHandler>
where
    F: Fn(CommandContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ExtensionCommandResult, ExtensionError>> + Send + 'static,
{
    Arc::new(FnCommandHandler { f })
}

struct FnCommandHandler<F> {
    f: F,
}

#[async_trait::async_trait]
impl<F, Fut> CommandHandler for FnCommandHandler<F>
where
    F: Fn(CommandContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ExtensionCommandResult, ExtensionError>> + Send + 'static,
{
    async fn execute(&self, ctx: CommandContext) -> Result<ExtensionCommandResult, ExtensionError> {
        (self.f)(ctx).await
    }
}

/// Wraps an async closure into an [`ExtensionHttpHandler`].
pub fn http_handler<F, Fut>(f: F) -> Arc<dyn ExtensionHttpHandler>
where
    F: Fn(HttpContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ExtensionHttpResponse, ExtensionError>> + Send + 'static,
{
    Arc::new(FnHttpHandler { f })
}

struct FnHttpHandler<F> {
    f: F,
}

#[async_trait::async_trait]
impl<F, Fut> ExtensionHttpHandler for FnHttpHandler<F>
where
    F: Fn(HttpContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ExtensionHttpResponse, ExtensionError>> + Send + 'static,
{
    async fn handle(&self, ctx: HttpContext) -> Result<ExtensionHttpResponse, ExtensionError> {
        (self.f)(ctx).await
    }
}

/// Wraps explicit plan and execute closures into a [`ToolHandler`].
pub fn tool_handler<P, PlanFut, F, Fut, R>(planner: P, f: F) -> Arc<dyn ToolHandler>
where
    P: Fn(ToolPlanContext) -> PlanFut + Send + Sync + 'static,
    PlanFut: Future<Output = Result<ToolPlan, ExtensionError>> + Send + 'static,
    F: Fn(ToolContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<R, ExtensionError>> + Send + 'static,
    R: Into<ToolExecutionResult> + Send + 'static,
{
    Arc::new(FnToolHandler { planner, f })
}

struct FnToolHandler<P, F> {
    planner: P,
    f: F,
}

#[async_trait::async_trait]
impl<P, PlanFut, F, Fut, R> ToolHandler for FnToolHandler<P, F>
where
    P: Fn(ToolPlanContext) -> PlanFut + Send + Sync + 'static,
    PlanFut: Future<Output = Result<ToolPlan, ExtensionError>> + Send + 'static,
    F: Fn(ToolContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<R, ExtensionError>> + Send + 'static,
    R: Into<ToolExecutionResult> + Send + 'static,
{
    async fn plan(&self, ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        (self.planner)(ctx).await
    }

    async fn execute(&self, ctx: ToolContext) -> Result<ToolExecutionResult, ExtensionError> {
        (self.f)(ctx).await.map(Into::into)
    }
}

/// Wraps an async closure that receives typed tool arguments and the complete call context.
pub fn tool_handler_args<A, P, PlanFut, F, Fut, R>(planner: P, f: F) -> Arc<dyn ToolHandler>
where
    A: DeserializeOwned + Send + 'static,
    P: Fn(A, ToolPlanContext) -> PlanFut + Send + Sync + 'static,
    PlanFut: Future<Output = Result<ToolPlan, ExtensionError>> + Send + 'static,
    F: Fn(A, ToolContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<R, ExtensionError>> + Send + 'static,
    R: Into<ToolExecutionResult> + Send + 'static,
{
    Arc::new(FnToolArgsHandler {
        planner,
        f,
        _arguments: std::marker::PhantomData,
    })
}

struct FnToolArgsHandler<A, P, F> {
    planner: P,
    f: F,
    _arguments: std::marker::PhantomData<fn() -> A>,
}

#[async_trait::async_trait]
impl<A, P, PlanFut, F, Fut, R> ToolHandler for FnToolArgsHandler<A, P, F>
where
    A: DeserializeOwned + Send + 'static,
    P: Fn(A, ToolPlanContext) -> PlanFut + Send + Sync + 'static,
    PlanFut: Future<Output = Result<ToolPlan, ExtensionError>> + Send + 'static,
    F: Fn(A, ToolContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<R, ExtensionError>> + Send + 'static,
    R: Into<ToolExecutionResult> + Send + 'static,
{
    async fn plan(&self, ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        let arguments = ctx.arguments::<A>()?;
        (self.planner)(arguments, ctx).await
    }

    async fn execute(&self, ctx: ToolContext) -> Result<ToolExecutionResult, ExtensionError> {
        let arguments = ctx.arguments::<A>()?;
        (self.f)(arguments, ctx).await.map(Into::into)
    }
}

// ─── continue_after_stop_handler_fn ──────────────────────────────────────

/// Wraps an async closure into `Arc<dyn ContinueAfterStopHandler>`.
pub fn continue_after_stop_handler_fn<F, Fut>(
    f: F,
) -> Arc<dyn crate::extension::ContinueAfterStopHandler>
where
    F: Fn(ContinueAfterStopContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ContinueAfterStopResult, ExtensionError>> + Send + 'static,
{
    Arc::new(FnContinueAfterStopHandler { f })
}

struct FnContinueAfterStopHandler<F> {
    f: F,
}

#[async_trait::async_trait]
impl<F, Fut> crate::extension::ContinueAfterStopHandler for FnContinueAfterStopHandler<F>
where
    F: Fn(ContinueAfterStopContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<ContinueAfterStopResult, ExtensionError>> + Send + 'static,
{
    async fn handle(
        &self,
        ctx: ContinueAfterStopContext,
    ) -> Result<ContinueAfterStopResult, ExtensionError> {
        (self.f)(ctx).await
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
///         "properties": { "name": { "type": "string" } },
///         "required": ["name"],
///         "additionalProperties": false
///     }))
///     .build();
/// ```
pub fn tool(name: impl Into<String>) -> ToolDefinitionBuilder {
    tool_with_origin(name, ToolOrigin::Bundled)
}

/// Worker-only tool builder re-exported by the worker runtime prelude.
pub fn worker_tool(name: impl Into<String>) -> ToolDefinitionBuilder {
    tool_with_origin(name, ToolOrigin::Extension)
}

fn tool_with_origin(name: impl Into<String>, origin: ToolOrigin) -> ToolDefinitionBuilder {
    ToolDefinitionBuilder {
        name: name.into(),
        description: String::new(),
        parameters: serde_json::json!({"type": "object"}),
        strict: false,
        execution_mode: ExecutionMode::Sequential,
        prompt: ToolPromptMetadata::default(),
        origin,
    }
}

/// Extension-owned tool declaration kept together with its prompt metadata.
///
/// The runtime binds this value to a handler in one [`crate::extension::Registrar::tool`] call.
/// It dereferences to the provider-visible [`ToolDefinition`] for read-only authoring code.
#[derive(Debug, Clone)]
pub struct ExtensionToolDefinition {
    definition: ToolDefinition,
    prompt: ToolPromptMetadata,
}

impl ExtensionToolDefinition {
    pub fn from_definition(definition: ToolDefinition) -> Self {
        Self {
            definition,
            prompt: ToolPromptMetadata::default(),
        }
    }

    pub fn with_prompt(mut self, prompt: ToolPromptMetadata) -> Self {
        self.prompt = prompt;
        self
    }

    pub fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    pub fn prompt(&self) -> &ToolPromptMetadata {
        &self.prompt
    }

    pub fn into_parts(self) -> (ToolDefinition, ToolPromptMetadata) {
        (self.definition, self.prompt)
    }
}

impl std::ops::Deref for ExtensionToolDefinition {
    type Target = ToolDefinition;

    fn deref(&self) -> &Self::Target {
        &self.definition
    }
}

impl From<ToolDefinition> for ExtensionToolDefinition {
    fn from(definition: ToolDefinition) -> Self {
        Self::from_definition(definition)
    }
}

impl From<ExtensionToolDefinition> for ToolDefinition {
    fn from(definition: ExtensionToolDefinition) -> Self {
        definition.definition
    }
}

pub struct ToolDefinitionBuilder {
    name: String,
    description: String,
    parameters: serde_json::Value,
    strict: bool,
    execution_mode: ExecutionMode,
    prompt: ToolPromptMetadata,
    origin: ToolOrigin,
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

    /// Explicitly require provider-side schema-constrained tool arguments.
    ///
    /// Use this only when the schema satisfies every targeted provider's strict JSON Schema
    /// subset. Provider-specific validation runs before the model request is sent.
    pub fn strict(mut self) -> Self {
        self.strict = true;
        self
    }

    /// Use provider-side non-strict tool arguments.
    ///
    /// This is the builder default. The method remains useful when configuration code wants to
    /// state the contract explicitly or override an earlier [`Self::strict`] call.
    pub fn non_strict(mut self) -> Self {
        self.strict = false;
        self
    }

    pub fn execution_mode(mut self, mode: ExecutionMode) -> Self {
        self.execution_mode = mode;
        self
    }

    pub fn prompt(mut self, prompt: ToolPromptMetadata) -> Self {
        self.prompt = prompt;
        self
    }

    pub fn build(self) -> ExtensionToolDefinition {
        ExtensionToolDefinition {
            definition: ToolDefinition {
                name: self.name,
                description: self.description,
                parameters: self.parameters,
                strict: self.strict,
                origin: self.origin,
                execution_mode: self.execution_mode,
            },
            prompt: self.prompt,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use astrcode_core::config::ModelSelection;
    use serde::Deserialize;

    use super::*;
    use crate::{
        WireErrorCode,
        extension::{
            ContinueAfterStopContext, ContinueAfterStopPayload, ContinueAfterStopResult,
            ExtensionCall, ExtensionHttpRequest,
            internal::{RuntimeContinueAfterStopContext, RuntimeHookCallContext},
        },
        testing::{CommandContextBuilder, HttpContextBuilder, ToolContextBuilder},
        tool::ToolResult,
    };

    #[test]
    fn tool_builder_sets_defaults() {
        let def = tool("test").description("A test tool").build();
        assert_eq!(def.name, "test");
        assert_eq!(def.description, "A test tool");
        assert!(!def.strict);
        assert_eq!(def.origin, ToolOrigin::Bundled);
        assert_eq!(def.execution_mode, ExecutionMode::Sequential);
        assert_eq!(worker_tool("worker").build().origin, ToolOrigin::Extension);

        assert!(tool("strict").strict().build().strict);
        assert!(!tool("dynamic").strict().non_strict().build().strict);
    }

    #[test]
    fn authoring_builders_keep_identity_permissions_and_declarations_explicit() {
        assert!(matches!(
            manifest("incomplete").build_checked(),
            Err(ExtensionManifestError::MissingVersion { .. })
        ));
        for invalid_id in [
            "../escape",
            ".hidden",
            "nested/path",
            "nested\\path",
            "bad id",
        ] {
            assert!(matches!(
                manifest(invalid_id).version("1.0.0").build_checked(),
                Err(ExtensionManifestError::InvalidId { .. })
            ));
        }
        let extension = manifest("example")
            .name("Example")
            .version("1.2.3")
            .description("Example extension")
            .capability(ExtensionCapability::SessionHistory)
            .capability(ExtensionCapability::SessionHistory)
            .build_checked()
            .unwrap();
        assert_eq!(extension.id(), "example");
        assert_eq!(extension.name(), "Example");
        assert_eq!(extension.version(), "1.2.3");
        assert_eq!(extension.description(), Some("Example extension"));
        assert_eq!(
            extension.capabilities(),
            &[ExtensionCapability::SessionHistory]
        );

        let command = command("review")
            .description("Review changes")
            .arguments(serde_json::json!({"type": "string"}))
            .requires_idle(true)
            .argument_completions(true)
            .priority(7)
            .build();
        assert!(command.args_schema.is_some());
        assert!(command.requires_idle);
        assert!(command.argument_completions);
        assert_eq!(command.priority, 7);

        let route = http_route(ExtensionHttpMethod::Post, "/review")
            .description("Start review")
            .max_body_bytes(1024)
            .build();
        assert_eq!(route.access, ExtensionHttpAccess::Authenticated);
        assert_eq!(route.max_body_bytes, 1024);

        let event = custom_event("review.completed")
            .schema_version(2)
            .delivery(CustomEventDelivery::GlobalLive)
            .max_payload_bytes(2048)
            .build();
        assert_eq!(event.schema_version, 2);
        assert_eq!(event.delivery, CustomEventDelivery::GlobalLive);
        assert_eq!(event.max_payload_bytes, 2048);
    }

    #[tokio::test]
    async fn command_and_http_adapters_receive_owned_attributed_contexts() {
        let command = command_handler(|ctx| async move {
            Ok(ExtensionCommandResult::handled(format!(
                "{}:{}:{}:{}:{}:{}:{}",
                ctx.extension_id(),
                ctx.session_id(),
                ctx.command_name(),
                ctx.argument(),
                ctx.working_dir().display(),
                ctx.model().model,
                ctx.paths()
                    .session_data_dir()
                    .ok()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
            )))
        });
        let result = command
            .execute(
                CommandContextBuilder::new("command-extension", "review")
                    .session(
                        "session-1",
                        "/workspace",
                        Some(PathBuf::from("/session-store")),
                    )
                    .model(ModelSelection::simple("model-1"))
                    .argument("staged")
                    .build(),
            )
            .await
            .unwrap();
        assert!(matches!(
            result,
            ExtensionCommandResult::Handled { message }
                if message == "command-extension:session-1:review:staged:/workspace:model-1:/session-store/extension_data/command-extension"
        ));
        assert!(!command.supports_argument_completions());

        #[derive(Deserialize)]
        struct Body {
            value: String,
        }

        let http = http_handler(|ctx| async move {
            let body = ctx.json::<Body>()?;
            Ok(ExtensionHttpResponse::json(
                200,
                serde_json::json!({
                    "extensionId": ctx.extension_id(),
                    "route": ctx.route().path,
                    "request": ctx.request().path,
                    "value": body.value,
                }),
            ))
        });
        let route = ExtensionHttpRoute::authenticated(ExtensionHttpMethod::Post, "/review/{id}");
        let request = ExtensionHttpRequest::new(ExtensionHttpMethod::Post, "/review/42")
            .json_body(serde_json::json!({ "value": "ok" }));
        let response = http
            .handle(HttpContextBuilder::new("http-extension", route, request).build())
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body["extensionId"], "http-extension");
        assert_eq!(response.body["route"], "/review/{id}");
        assert_eq!(response.body["request"], "/review/42");
        assert_eq!(response.body["value"], "ok");
    }

    #[tokio::test]
    async fn tool_handler_adapters_receive_owned_context_and_decode_arguments() {
        #[derive(Deserialize)]
        struct Args {
            count: usize,
        }

        let handler = tool_handler_args(
            |_args: Args, _ctx| async move { Ok(ToolPlan::default()) },
            |args: Args, ctx| async move {
                Ok(ToolResult::success(format!(
                    "{}:{}:{}",
                    ctx.extension_id(),
                    ctx.tool_name(),
                    args.count
                )))
            },
        );
        let ctx = ToolContextBuilder::new("test-extension", "count")
            .session("session-1", "/workspace", None)
            .arguments(serde_json::json!({ "count": 3 }))
            .build();
        let result = handler.execute(ctx).await.unwrap();
        let (result, discovered) = result.into_parts();
        assert_eq!(result.content, "test-extension:count:3");
        assert!(!result.is_error);
        assert!(discovered.is_empty());

        let error = handler
            .execute(
                ToolContextBuilder::new("test-extension", "count")
                    .arguments(serde_json::json!({ "count": "three" }))
                    .build(),
            )
            .await
            .unwrap_err();
        let ExtensionError::InvalidInput { code, message, .. } = error else {
            panic!("typed arguments should preserve the invalid-input error class");
        };
        assert_eq!(code, WireErrorCode::InvalidInput.as_str());
        assert!(message.contains("tool `count`"));
        assert!(message.contains("count"));

        let plain = tool_handler(
            |_ctx| async move { Ok(ToolPlan::default()) },
            |ctx| async move { Ok(ToolResult::success(ctx.working_dir().display().to_string())) },
        );
        let result = plain
            .execute(
                ToolContextBuilder::new("test-extension", "plain")
                    .session("session-1", "/workspace", None)
                    .build(),
            )
            .await
            .unwrap();
        assert_eq!(result.into_parts().0.content, "/workspace");
    }

    #[tokio::test]
    async fn continue_after_stop_handler_fn_dispatches_to_closure() {
        let handler = continue_after_stop_handler_fn(|ctx| async move {
            if ctx.finish_reason() == "stop" {
                Ok(ContinueAfterStopResult::ContinueOneStep)
            } else {
                Ok(ContinueAfterStopResult::EndTurn)
            }
        });
        let call = ToolContextBuilder::new("test-extension", "fixture")
            .session("s1", "/tmp", None)
            .build()
            .call()
            .clone();
        let input = RuntimeContinueAfterStopContext::new(
            RuntimeHookCallContext::new("s1", "/tmp", ModelSelection::simple("test"), None),
            ContinueAfterStopPayload::new("done", "stop", 0),
        );
        let ctx = ContinueAfterStopContext::from_runtime(call, &input);
        let result = handler.handle(ctx).await.unwrap();
        assert_eq!(result, ContinueAfterStopResult::ContinueOneStep);
    }
}
