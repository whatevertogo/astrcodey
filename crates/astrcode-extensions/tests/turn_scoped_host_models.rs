use std::sync::{Arc, Mutex};

use astrcode_core::{
    config::ModelSelection,
    llm::{
        LlmError, LlmEvent, LlmMessage, LlmProvider, LlmProviderBindings, LlmRequest, ModelLimits,
    },
    tool::{ToolCapabilities, ToolExecutionContext, ToolHostServices, access::ResourceLease},
    types::SessionId,
};
use astrcode_extension_sdk::{
    builder::manifest,
    extension::{
        Extension, ExtensionCall, ExtensionCapability, ExtensionError, HookMode, HookResult,
        LifecycleContext, LifecycleEvent, LifecycleHandler, Registrar, ToolContext, ToolHandler,
        ToolPlanContext,
        internal::{RuntimeHookCallContext, runtime_lifecycle_context},
    },
    tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolPlan, ToolResult},
};
use astrcode_extensions::{HostBackends, HostRouter, runner::ExtensionRunner};

struct TaggedProvider(&'static str);

#[async_trait::async_trait]
impl LlmProvider for TaggedProvider {
    async fn generate_request(
        &self,
        _request: LlmRequest,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        sender
            .send(LlmEvent::ContentDelta {
                delta: self.0.to_owned(),
            })
            .unwrap();
        sender
            .send(LlmEvent::Done {
                finish_reason: "stop".into(),
            })
            .unwrap();
        Ok(receiver)
    }

    fn model_limits(&self) -> ModelLimits {
        ModelLimits {
            max_input_tokens: 4_096,
            max_output_tokens: 1_024,
        }
    }
}

struct LiveProvider {
    current: Mutex<Arc<dyn LlmProvider>>,
}

impl LiveProvider {
    fn new(current: Arc<dyn LlmProvider>) -> Self {
        Self {
            current: Mutex::new(current),
        }
    }

    fn publish(&self, provider: Arc<dyn LlmProvider>) {
        *self.current.lock().unwrap() = provider;
    }

    fn current(&self) -> Arc<dyn LlmProvider> {
        Arc::clone(&self.current.lock().unwrap())
    }
}

#[async_trait::async_trait]
impl LlmProvider for LiveProvider {
    async fn generate_request(
        &self,
        request: LlmRequest,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<LlmEvent>, LlmError> {
        self.current().generate_request(request).await
    }

    fn model_limits(&self) -> ModelLimits {
        self.current().model_limits()
    }
}

struct ModelBindingProbeExtension {
    hook_calls: Arc<Mutex<Vec<String>>>,
}

struct ModelBindingProbeHook {
    calls: Arc<Mutex<Vec<String>>>,
}

struct ModelBindingProbeTool;

#[async_trait::async_trait]
impl Extension for ModelBindingProbeExtension {
    fn manifest(&self) -> astrcode_extension_sdk::extension::ExtensionManifest {
        manifest("turn-model-binding-probe")
            .version("test")
            .capability(ExtensionCapability::MainModel)
            .capability(ExtensionCapability::SmallModel)
            .build()
    }

    fn register(&self, registrar: &mut Registrar) {
        registrar.on_lifecycle(
            LifecycleEvent::TurnStart,
            HookMode::Blocking,
            0,
            Arc::new(ModelBindingProbeHook {
                calls: Arc::clone(&self.hook_calls),
            }),
        );
        registrar.tool(
            ToolDefinition {
                name: "turnModelBindingProbe".into(),
                description: "Exercises turn-scoped Host model bindings".into(),
                parameters: serde_json::json!({ "type": "object" }),
                strict: false,
                origin: ToolOrigin::Extension,
                execution_mode: ExecutionMode::Sequential,
            },
            Arc::new(ModelBindingProbeTool),
        );
    }
}

async fn invoke_model_pair(call: &impl ExtensionCall) -> Result<String, ExtensionError> {
    let models = call.host().models();
    let main = models
        .main_chat(vec![LlmMessage::user("main probe")])
        .await?;
    let small = models
        .small_chat(vec![LlmMessage::user("small probe")])
        .await?;
    Ok(format!("{}|{}", main.content, small.content))
}

#[async_trait::async_trait]
impl LifecycleHandler for ModelBindingProbeHook {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        let pair = invoke_model_pair(&ctx).await?;
        self.calls.lock().unwrap().push(pair);
        Ok(HookResult::Allow)
    }
}

#[async_trait::async_trait]
impl ToolHandler for ModelBindingProbeTool {
    async fn plan(&self, _ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        Ok(ToolPlan::host(
            astrcode_core::tool::access::HostResource::Model,
        ))
    }

    async fn execute(
        &self,
        ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        Ok(ToolResult::success(invoke_model_pair(&ctx).await?).into())
    }
}

fn runtime_hook_call(bindings: LlmProviderBindings) -> RuntimeHookCallContext {
    RuntimeHookCallContext::new(
        "session",
        "/workspace",
        ModelSelection::simple("main-model"),
        None,
    )
    .with_turn_id("turn")
    .with_llm_providers(bindings)
}

fn tool_context(bindings: LlmProviderBindings) -> ToolExecutionContext {
    ToolExecutionContext::new(
        SessionId::new("session"),
        "/workspace",
        Some("call".into()),
        None,
        ToolCapabilities {
            host: ToolHostServices {
                llm_providers: Some(bindings),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .with_turn_id("turn".into())
    .with_resource_lease(ResourceLease::from_plan(&ToolPlan::host(
        astrcode_core::tool::access::HostResource::Model,
    )))
}

#[tokio::test]
async fn hooks_and_tools_keep_their_turn_models_after_live_publication() {
    let old_main: Arc<dyn LlmProvider> = Arc::new(TaggedProvider("old-main"));
    let old_small: Arc<dyn LlmProvider> = Arc::new(TaggedProvider("old-small"));
    let new_main: Arc<dyn LlmProvider> = Arc::new(TaggedProvider("new-main"));
    let new_small: Arc<dyn LlmProvider> = Arc::new(TaggedProvider("new-small"));
    let live_main = Arc::new(LiveProvider::new(Arc::clone(&old_main)));
    let live_small = Arc::new(LiveProvider::new(Arc::clone(&old_small)));
    let router = Arc::new(HostRouter::from_backends(HostBackends {
        main_llm: Some(live_main.clone()),
        small_llm: Some(live_small.clone()),
        ..Default::default()
    }));
    let runner = ExtensionRunner::new(std::time::Duration::from_secs(1));
    runner.bind_host_router(router);
    let hook_calls = Arc::new(Mutex::new(Vec::new()));
    runner
        .register(Arc::new(ModelBindingProbeExtension {
            hook_calls: Arc::clone(&hook_calls),
        }))
        .await
        .unwrap();
    let tool = runner
        .tool_catalog_snapshot_typed("/workspace")
        .await
        .tools
        .into_iter()
        .find(|tool| tool.definition().name == "turnModelBindingProbe")
        .unwrap();
    let old_bindings = LlmProviderBindings::new(Arc::clone(&old_main), Arc::clone(&old_small));
    let old_hook = runtime_lifecycle_context(runtime_hook_call(old_bindings.clone()), None, 0);
    let old_tool = tool_context(old_bindings);

    live_main.publish(Arc::clone(&new_main));
    live_small.publish(Arc::clone(&new_small));

    runner
        .emit_lifecycle(LifecycleEvent::TurnStart, old_hook)
        .await
        .unwrap();
    let old_tool_result = tool
        .execute(serde_json::json!({}), &old_tool)
        .await
        .unwrap();

    let new_bindings = LlmProviderBindings::new(new_main, new_small);
    runner
        .emit_lifecycle(
            LifecycleEvent::TurnStart,
            runtime_lifecycle_context(runtime_hook_call(new_bindings.clone()), None, 0),
        )
        .await
        .unwrap();
    let new_tool_result = tool
        .execute(serde_json::json!({}), &tool_context(new_bindings))
        .await
        .unwrap();

    assert_eq!(
        hook_calls.lock().unwrap().as_slice(),
        ["old-main|old-small", "new-main|new-small"]
    );
    assert_eq!(old_tool_result.content, "old-main|old-small");
    assert_eq!(new_tool_result.content, "new-main|new-small");
}
