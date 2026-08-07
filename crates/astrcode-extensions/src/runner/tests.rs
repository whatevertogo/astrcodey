use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use astrcode_core::{
    config::ModelSelection,
    event::{DurableEventPayload, EventPayload, ExtensionEventData},
    tool::access::ResourceAccess,
};
use astrcode_extension_sdk::{
    builder::{extension_event, manifest},
    extension::{
        CommandCompletionContext, CommandCompletionItem, CommandCompletions, CommandContext,
        CommandHandler, CompactContext, CompactEvent, CompactHandler, CompactResult,
        ContinueAfterStopContext, ContinueAfterStopHandler, ContinueAfterStopOptions,
        ContinueAfterStopResult, Extension, ExtensionCapability, ExtensionCommandResult,
        ExtensionConfig, ExtensionError, ExtensionEvent, ExtensionHttpHandler, ExtensionHttpMethod,
        ExtensionHttpRequest, ExtensionHttpResponse, ExtensionHttpRoute, ExtensionManifest,
        ExtensionStartContext, ExtensionTasks, HookMode, HookResult, HttpContext, LifecycleContext,
        LifecycleHandler, PostToolUseContext, PostToolUseHandler, PostToolUseResult,
        PreToolUseContext, PreToolUseHandler, PreToolUseResult, ProviderContext, ProviderEvent,
        ProviderHandler, ProviderResult, Registrar, RuntimeContinueAfterStopContext,
        RuntimeHookCallContext, RuntimePreToolUseContext, RuntimeProviderContext,
        RuntimeUserMessageEnvelopeContext, SlashCommand, StatusItem, StopReason, ToolContext,
        ToolDiscovery, ToolDiscoveryContext, ToolDiscoveryHandler, ToolHandler, ToolHookTarget,
        UserMessageEnvelopeContext, UserMessageEnvelopeHandler, UserMessageEnvelopeResult,
    },
    runtime_ports::{
        RuntimeSnapshotProvider, RuntimeSnapshotState, ToolCatalogCompleteness,
        TurnExtensionViewProvider,
    },
    tool::{
        ExecutionMode, ToolCapabilities, ToolDefinition, ToolExecutionContext, ToolOrigin,
        ToolResult,
    },
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Notify, mpsc};

use super::{CommandRuntimeContext, CommandSource, ExtensionHttpDispatchResult, ExtensionRunner};
use crate::runner::tool_adapter::normalize_stringified_booleans;

fn extension_manifest(
    id: impl Into<String>,
    capabilities: &[ExtensionCapability],
) -> ExtensionManifest {
    capabilities
        .iter()
        .copied()
        .fold(
            manifest(id)
                .version("test")
                .description("Extension runner test probe"),
            |manifest, capability| manifest.capability(capability),
        )
        .build()
}

struct ManagedTaskExtension {
    started: Arc<AtomicUsize>,
    stopped: Arc<AtomicUsize>,
    task_stopped: Arc<AtomicBool>,
    expected_reason: StopReason,
}

struct DeferredTaskExtension {
    task_started: Arc<AtomicBool>,
}

struct StartupDirectoryExtension {
    received: Arc<Mutex<Option<StartupContextSnapshot>>>,
}

#[derive(Debug, PartialEq, Eq)]
struct StartupContextSnapshot {
    extension_id: String,
    startup_working_dir: Option<PathBuf>,
    global_data_dir: Option<PathBuf>,
    session_context_available: bool,
    config_version: u64,
    cancelled: bool,
}

struct StartupEventExtension;

struct UnhealthyExtension;

struct ConfigChangeProbeExtension(Arc<ConfigChangeProbeState>);

#[derive(Default)]
struct ConfigChangeProbeState {
    calls: AtomicUsize,
    fail_next: AtomicBool,
    block_version: AtomicUsize,
    entered: Notify,
    release: Notify,
    applied_version: AtomicUsize,
    stopped: AtomicBool,
}

struct StateProbeExtension;

struct StateProbeTool;

struct ToolRetirementProbeExtension {
    stopped: Arc<AtomicUsize>,
}

struct RetirementFailureExtension {
    fail_stop: bool,
}

struct SlowToolDiscoveryExtension;

struct SlowToolDiscovery;

struct HttpProbeExtension {
    id: &'static str,
    capabilities: Vec<ExtensionCapability>,
    route: ExtensionHttpRoute,
}

struct HttpProbeHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpProbeBody {
    name: String,
}

#[async_trait::async_trait]
impl ExtensionHttpHandler for HttpProbeHandler {
    async fn handle(&self, ctx: HttpContext) -> Result<ExtensionHttpResponse, ExtensionError> {
        let request = ctx.request();
        if request.query.as_deref() == Some("invalid-status") {
            return Ok(ExtensionHttpResponse {
                status: 99,
                body: serde_json::Value::Null,
            });
        }
        let body_name = if request.body.is_null() {
            None
        } else {
            Some(ctx.json::<HttpProbeBody>()?.name)
        };
        Ok(ExtensionHttpResponse::json(
            201,
            json!({
                "extensionId": ctx.extension_id(),
                "routePath": ctx.route().path,
                "requestPath": request.path,
                "pathParams": request.path_params,
                "query": request.query,
                "body": request.body,
                "bodyName": body_name,
            }),
        ))
    }
}

#[async_trait::async_trait]
impl Extension for HttpProbeExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest(self.id, &self.capabilities)
    }

    fn register(&self, registrar: &mut Registrar) {
        registrar.http_route(self.route.clone(), Arc::new(HttpProbeHandler));
    }
}

struct SmallModelProbeExtension {
    small_model_allowed: bool,
    session_control_allowed: bool,
}

struct SmallModelProbeTool;

struct TargetedPreHookExtension {
    calls: Arc<AtomicUsize>,
}

struct CountingPreHook {
    calls: Arc<AtomicUsize>,
}

struct GenerationProbeExtension {
    label: &'static str,
    stopped: Arc<AtomicUsize>,
    call_started: Option<Arc<Notify>>,
    lifecycle: Option<Arc<Mutex<Vec<&'static str>>>>,
}

struct GenerationProbeHook {
    label: &'static str,
    call_started: Option<Arc<Notify>>,
}

struct CancelledStartupExtension {
    tasks: Arc<Mutex<Option<ExtensionTasks>>>,
    write_started: Arc<Notify>,
    write_release: Arc<Notify>,
    stop_entered: Arc<Notify>,
    stop_release: Arc<Notify>,
    stop_reason: Arc<Mutex<Option<StopReason>>>,
    stops: Arc<AtomicUsize>,
    lifecycle: Arc<Mutex<Vec<&'static str>>>,
}

struct StartFailingExtension;

struct StartupTimeoutExtension {
    task_started: Arc<AtomicBool>,
    stop_reason: Arc<Mutex<Option<StopReason>>>,
}

struct BlockingProviderResponseExtension;

struct BlockingProviderHook;

struct OperationTimeoutExtension;

struct PendingProviderHook;

struct ContinueAfterStopProbeExtension {
    id: &'static str,
    options: ContinueAfterStopOptions,
    calls: Arc<AtomicUsize>,
}

struct ContinueAfterStopProbe {
    calls: Arc<AtomicUsize>,
}

struct UserMessageEnvelopeProbeExtension {
    id: &'static str,
    priority: i32,
    result: UserMessageEnvelopeResult,
    calls: Arc<AtomicUsize>,
}

struct UserMessageEnvelopeProbe {
    result: UserMessageEnvelopeResult,
    calls: Arc<AtomicUsize>,
}

struct CommandProbeExtension {
    id: &'static str,
    command_name: &'static str,
    priority: i32,
    argument_completions: bool,
}

#[derive(Clone, Copy, Debug)]
enum CapabilityRegistration {
    Event,
    Compact,
    UserMessageEnvelope,
    BeforeProvider,
    AfterProvider,
    BlockingPreTool,
    BlockingPostTool,
    ContinueAfterStop,
}

struct RegistrationProbeExtension {
    capabilities: Vec<ExtensionCapability>,
    registration: CapabilityRegistration,
}

struct LifecycleModeProbeExtension {
    event: ExtensionEvent,
}

struct RegistrationProbeHandler;

struct CommandProbe {
    label: &'static str,
    argument_completions: bool,
}

#[async_trait::async_trait]
impl Extension for ConfigChangeProbeExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("config-change-probe", &[])
    }

    fn register(&self, registrar: &mut Registrar) {
        registrar.status_item(StatusItem {
            id: "config-change-probe".into(),
            text: String::new(),
            priority: 0,
            tooltip: None,
        });
    }

    async fn on_config_changed(&self, config: ExtensionConfig) -> Result<(), ExtensionError> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        let value: serde_json::Value = config.deserialize().unwrap();
        let version = value["version"].as_u64().unwrap() as usize;
        if self.0.block_version.load(Ordering::SeqCst) == version {
            self.0.entered.notify_one();
            self.0.release.notified().await;
        }
        if self.0.fail_next.swap(false, Ordering::SeqCst) {
            Err(ExtensionError::Internal("injected config failure".into()))
        } else {
            self.0.applied_version.store(version, Ordering::SeqCst);
            Ok(())
        }
    }

    async fn stop(&self, _reason: StopReason) -> Result<(), ExtensionError> {
        self.0.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait::async_trait]
impl Extension for StateProbeExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("state-probe", &[])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.tool(
            ToolDefinition {
                name: "stateProbe".into(),
                description: String::new(),
                parameters: json!({"type": "object"}),
                strict: false,
                origin: ToolOrigin::Extension,
                execution_mode: ExecutionMode::Sequential,
            },
            Arc::new(StateProbeTool),
        );
    }
}

#[async_trait::async_trait]
impl ToolHandler for StateProbeTool {
    async fn execute(
        &self,
        ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        Ok(ToolResult::text(
            ctx.paths().session_data_dir().is_ok().to_string(),
            false,
            Default::default(),
        )
        .into())
    }
}

#[async_trait::async_trait]
impl Extension for ToolRetirementProbeExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("tool-retirement-probe", &[])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.tool(
            ToolDefinition {
                name: "retirementProbe".into(),
                description: String::new(),
                parameters: json!({"type": "object"}),
                strict: false,
                origin: ToolOrigin::Extension,
                execution_mode: ExecutionMode::Sequential,
            },
            Arc::new(StateProbeTool),
        );
    }

    async fn stop(&self, _reason: StopReason) -> Result<(), ExtensionError> {
        self.stopped.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait::async_trait]
impl Extension for RetirementFailureExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("retirement-failure-probe", &[])
    }

    async fn stop(&self, _reason: StopReason) -> Result<(), ExtensionError> {
        if self.fail_stop {
            Err(ExtensionError::Internal("intentional stop failure".into()))
        } else {
            Ok(())
        }
    }
}

#[async_trait::async_trait]
impl Extension for SlowToolDiscoveryExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("slow-discovery", &[])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.tool_discovery(Arc::new(SlowToolDiscovery));
    }
}

#[async_trait::async_trait]
impl ToolDiscoveryHandler for SlowToolDiscovery {
    async fn discover(&self, _ctx: ToolDiscoveryContext) -> Result<ToolDiscovery, ExtensionError> {
        std::future::pending().await
    }
}

#[async_trait::async_trait]
impl Extension for CommandProbeExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest(self.id, &[])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.command(
            SlashCommand {
                name: self.command_name.into(),
                description: format!("{} command", self.id),
                args_schema: None,
                requires_idle: false,
                argument_completions: self.argument_completions,
                priority: self.priority,
            },
            Arc::new(CommandProbe {
                label: self.id,
                argument_completions: self.argument_completions,
            }),
        );
    }
}

#[async_trait::async_trait]
impl CommandHandler for CommandProbe {
    async fn execute(
        &self,
        _ctx: CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        Ok(ExtensionCommandResult::handled(self.label))
    }

    async fn complete(
        &self,
        ctx: CommandCompletionContext,
    ) -> Result<CommandCompletions, ExtensionError> {
        if !self.argument_completions {
            return Ok(CommandCompletions::default());
        }
        Ok(CommandCompletions {
            items: vec![CommandCompletionItem {
                label: format!("{}:{}:{}", self.label, ctx.argument(), ctx.cursor()),
                insert_text: self.label.into(),
                detail: Some(format!(
                    "{}:{}:{}:{}:{}:{}",
                    ctx.extension_id(),
                    ctx.session_id()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                    ctx.command_name(),
                    ctx.working_dir()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default(),
                    ctx.model().model,
                    ctx.paths()
                        .session_data_dir()
                        .ok()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default(),
                )),
            }],
            truncated: false,
        })
    }

    fn supports_argument_completions(&self) -> bool {
        self.argument_completions
    }
}

struct ExecuteOnlyCommandProbe;

#[async_trait::async_trait]
impl CommandHandler for ExecuteOnlyCommandProbe {
    async fn execute(
        &self,
        _ctx: CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        Ok(ExtensionCommandResult::handled("execute-only"))
    }
}

#[async_trait::async_trait]
impl Extension for SmallModelProbeExtension {
    fn manifest(&self) -> ExtensionManifest {
        let capabilities = match (self.small_model_allowed, self.session_control_allowed) {
            (true, true) => &[
                ExtensionCapability::SmallModel,
                ExtensionCapability::SessionControl,
            ][..],
            (true, false) => &[ExtensionCapability::SmallModel][..],
            (false, true) => &[ExtensionCapability::SessionControl][..],
            (false, false) => &[],
        };
        extension_manifest("small-model-probe", capabilities)
    }

    fn register(&self, reg: &mut Registrar) {
        reg.tool(
            ToolDefinition {
                name: "smallModelProbe".into(),
                description: String::new(),
                parameters: json!({"type": "object"}),
                strict: false,
                origin: ToolOrigin::Extension,
                execution_mode: ExecutionMode::Sequential,
            },
            Arc::new(SmallModelProbeTool),
        );
    }
}

#[async_trait::async_trait]
impl ToolHandler for SmallModelProbeTool {
    async fn execute(
        &self,
        ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        Ok(ToolResult::text(
            ctx.small_model_id().is_some().to_string(),
            false,
            Default::default(),
        )
        .into())
    }
}

#[async_trait::async_trait]
impl Extension for TargetedPreHookExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("targeted-pre-hook", &[ExtensionCapability::ToolIntercept])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_pre_tool_use_for(
            ToolHookTarget::names(["targetTool"]),
            HookMode::Blocking,
            0,
            Arc::new(CountingPreHook {
                calls: Arc::clone(&self.calls),
            }),
        );
    }
}

#[async_trait::async_trait]
impl PreToolUseHandler for CountingPreHook {
    async fn handle(&self, _ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(PreToolUseResult::Allow)
    }
}

#[async_trait::async_trait]
impl Extension for GenerationProbeExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("generation-probe", &[ExtensionCapability::ToolIntercept])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_pre_tool_use(
            HookMode::Blocking,
            0,
            Arc::new(GenerationProbeHook {
                label: self.label,
                call_started: self.call_started.clone(),
            }),
        );
    }

    async fn start(&self, _ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        if let Some(lifecycle) = &self.lifecycle {
            lifecycle.lock().unwrap().push(self.label);
        }
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) -> Result<(), ExtensionError> {
        if let Some(lifecycle) = &self.lifecycle {
            lifecycle.lock().unwrap().push(match self.label {
                "v1" => "v1_stop",
                "v2" => "v2_stop",
                label => label,
            });
        }
        self.stopped.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait::async_trait]
impl Extension for CancelledStartupExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("generation-probe", &[])
    }

    async fn start(&self, ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        let tasks = ctx.tasks().clone();
        *self.tasks.lock().unwrap() = Some(tasks.clone());
        let write_started = Arc::clone(&self.write_started);
        let write_release = Arc::clone(&self.write_release);
        let lifecycle = Arc::clone(&self.lifecycle);
        tasks
            .run_to_completion("startup-write", async move {
                write_started.notify_one();
                write_release.notified().await;
                lifecycle.lock().unwrap().push("write_done");
            })
            .await
            .map_err(|error| ExtensionError::Internal(error.to_string()))
    }

    async fn stop(&self, reason: StopReason) -> Result<(), ExtensionError> {
        *self.stop_reason.lock().unwrap() = Some(reason);
        self.stop_entered.notify_one();
        self.stop_release.notified().await;
        self.lifecycle.lock().unwrap().push("v1_stop");
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait::async_trait]
impl PreToolUseHandler for GenerationProbeHook {
    async fn handle(&self, ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError> {
        if let Some(call_started) = &self.call_started {
            call_started.notify_one();
            ctx.cancellation().cancelled().await;
        }
        Ok(PreToolUseResult::ModifyInput {
            tool_input: json!({ "generation": self.label }),
        })
    }
}

#[async_trait::async_trait]
impl Extension for StartFailingExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("start-failing", &[])
    }

    async fn start(&self, _ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        Err(ExtensionError::Internal(
            "startup dependency missing".into(),
        ))
    }
}

#[async_trait::async_trait]
impl Extension for StartupTimeoutExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("startup-timeout", &[])
    }

    async fn start(&self, ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        let shutdown = ctx.cancellation().clone();
        let task_started = Arc::clone(&self.task_started);
        ctx.tasks().spawn("startup-task", async move {
            task_started.store(true, Ordering::SeqCst);
            shutdown.cancelled().await;
        });
        std::future::pending().await
    }

    async fn stop(&self, reason: StopReason) -> Result<(), ExtensionError> {
        *self.stop_reason.lock().unwrap() = Some(reason);
        Ok(())
    }
}

#[async_trait::async_trait]
impl Extension for BlockingProviderResponseExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest(
            "provider-response-observer",
            &[ExtensionCapability::ProviderRequest],
        )
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_after_provider_response(0, Arc::new(BlockingProviderHook));
    }
}

#[async_trait::async_trait]
impl ProviderHandler for BlockingProviderHook {
    async fn handle(&self, _ctx: ProviderContext) -> Result<ProviderResult, ExtensionError> {
        Ok(ProviderResult::Block {
            reason: "response observers cannot block".into(),
        })
    }
}

#[async_trait::async_trait]
impl Extension for OperationTimeoutExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("operation-timeout", &[ExtensionCapability::ProviderRequest])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_after_provider_response(0, Arc::new(PendingProviderHook));
    }

    async fn stop(&self, _reason: StopReason) -> Result<(), ExtensionError> {
        std::future::pending().await
    }
}

#[async_trait::async_trait]
impl ProviderHandler for PendingProviderHook {
    async fn handle(&self, _ctx: ProviderContext) -> Result<ProviderResult, ExtensionError> {
        std::future::pending().await
    }
}

#[async_trait::async_trait]
impl Extension for ContinueAfterStopProbeExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest(self.id, &[ExtensionCapability::TurnContinuationControl])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_continue_after_stop(
            0,
            self.options,
            Arc::new(ContinueAfterStopProbe {
                calls: Arc::clone(&self.calls),
            }),
        );
    }
}

#[async_trait::async_trait]
impl ContinueAfterStopHandler for ContinueAfterStopProbe {
    async fn handle(
        &self,
        _ctx: ContinueAfterStopContext,
    ) -> Result<ContinueAfterStopResult, ExtensionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ContinueAfterStopResult::ContinueOneStep)
    }
}

#[async_trait::async_trait]
impl Extension for UserMessageEnvelopeProbeExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest(self.id, &[ExtensionCapability::ProviderRequest])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_user_message_envelope(
            self.priority,
            Arc::new(UserMessageEnvelopeProbe {
                result: self.result.clone(),
                calls: Arc::clone(&self.calls),
            }),
        );
    }
}

#[async_trait::async_trait]
impl UserMessageEnvelopeHandler for UserMessageEnvelopeProbe {
    async fn handle(
        &self,
        _ctx: UserMessageEnvelopeContext,
    ) -> Result<UserMessageEnvelopeResult, ExtensionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.result.clone())
    }
}

#[async_trait::async_trait]
impl Extension for RegistrationProbeExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("registration-probe", &self.capabilities)
    }

    fn register(&self, reg: &mut Registrar) {
        match self.registration {
            CapabilityRegistration::Event => {
                reg.declare_event(extension_event("probe").build());
            },
            CapabilityRegistration::Compact => {
                reg.on_compact(
                    CompactEvent::PreCompact,
                    0,
                    Arc::new(RegistrationProbeHandler),
                );
            },
            CapabilityRegistration::UserMessageEnvelope => {
                reg.on_user_message_envelope(0, Arc::new(RegistrationProbeHandler));
            },
            CapabilityRegistration::BeforeProvider => {
                reg.on_before_provider_request(
                    HookMode::Advisory,
                    0,
                    Arc::new(RegistrationProbeHandler),
                );
            },
            CapabilityRegistration::AfterProvider => {
                reg.on_after_provider_response(0, Arc::new(RegistrationProbeHandler));
            },
            CapabilityRegistration::BlockingPreTool => {
                reg.on_pre_tool_use(HookMode::Blocking, 0, Arc::new(RegistrationProbeHandler));
            },
            CapabilityRegistration::BlockingPostTool => {
                reg.on_post_tool_use(HookMode::Blocking, 0, Arc::new(RegistrationProbeHandler));
            },
            CapabilityRegistration::ContinueAfterStop => {
                reg.on_continue_after_stop(
                    0,
                    ContinueAfterStopOptions::default(),
                    Arc::new(RegistrationProbeHandler),
                );
            },
        }
    }
}

#[async_trait::async_trait]
impl CompactHandler for RegistrationProbeHandler {
    async fn handle(&self, _ctx: CompactContext) -> Result<CompactResult, ExtensionError> {
        Ok(CompactResult::Allow)
    }
}

#[async_trait::async_trait]
impl UserMessageEnvelopeHandler for RegistrationProbeHandler {
    async fn handle(
        &self,
        _ctx: UserMessageEnvelopeContext,
    ) -> Result<UserMessageEnvelopeResult, ExtensionError> {
        Ok(UserMessageEnvelopeResult::Allow)
    }
}

#[async_trait::async_trait]
impl ProviderHandler for RegistrationProbeHandler {
    async fn handle(&self, _ctx: ProviderContext) -> Result<ProviderResult, ExtensionError> {
        Ok(ProviderResult::Allow)
    }
}

#[async_trait::async_trait]
impl PreToolUseHandler for RegistrationProbeHandler {
    async fn handle(&self, _ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError> {
        Ok(PreToolUseResult::Allow)
    }
}

#[async_trait::async_trait]
impl PostToolUseHandler for RegistrationProbeHandler {
    async fn handle(&self, _ctx: PostToolUseContext) -> Result<PostToolUseResult, ExtensionError> {
        Ok(PostToolUseResult::Allow)
    }
}

#[async_trait::async_trait]
impl ContinueAfterStopHandler for RegistrationProbeHandler {
    async fn handle(
        &self,
        _ctx: ContinueAfterStopContext,
    ) -> Result<ContinueAfterStopResult, ExtensionError> {
        Ok(ContinueAfterStopResult::EndTurn)
    }
}

#[async_trait::async_trait]
impl LifecycleHandler for RegistrationProbeHandler {
    async fn handle(&self, _ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        Ok(HookResult::Allow)
    }
}

#[async_trait::async_trait]
impl Extension for LifecycleModeProbeExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("lifecycle-mode-probe", &[])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_lifecycle(
            self.event.clone(),
            HookMode::Blocking,
            0,
            Arc::new(RegistrationProbeHandler),
        );
    }
}

fn runtime_hook_call() -> RuntimeHookCallContext {
    RuntimeHookCallContext::new(
        "session",
        "D:/workspace",
        ModelSelection::simple("model"),
        None,
    )
    .with_turn_id("turn")
}

fn continue_after_stop_ctx(continuations_this_turn: u32) -> RuntimeContinueAfterStopContext {
    RuntimeContinueAfterStopContext::new(
        runtime_hook_call(),
        "done",
        "stop",
        continuations_this_turn,
    )
}

fn user_message_envelope_ctx(text: &str) -> RuntimeUserMessageEnvelopeContext {
    RuntimeUserMessageEnvelopeContext::new(runtime_hook_call(), text, Vec::new())
}

fn pre_tool_use_ctx(tool_name: &str, tool_input: serde_json::Value) -> RuntimePreToolUseContext {
    RuntimePreToolUseContext::new(
        runtime_hook_call(),
        "call-1".into(),
        tool_name,
        tool_input,
        astrcode_core::permission::ApprovalMode::Manual,
        Vec::new(),
    )
}

#[async_trait::async_trait]
impl Extension for StartupDirectoryExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("startup-directory", &[])
    }

    async fn start(&self, ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        let config: serde_json::Value = ctx.config().deserialize()?;
        *self.received.lock().unwrap() = Some(StartupContextSnapshot {
            extension_id: ctx.extension_id().to_owned(),
            startup_working_dir: ctx.startup_working_dir().map(Path::to_path_buf),
            global_data_dir: ctx.paths().global_data_dir().map(Path::to_path_buf),
            session_context_available: ctx.call().session_id().is_some()
                || ctx.call().turn_id().is_some()
                || ctx.paths().session_data_dir().is_ok(),
            config_version: config["version"].as_u64().unwrap(),
            cancelled: ctx.cancellation().is_cancelled(),
        });
        Ok(())
    }
}

#[async_trait::async_trait]
impl Extension for StartupEventExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("startup-event", &[ExtensionCapability::EmitEvents])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.declare_event(extension_event("startup_ready").build());
    }

    async fn start(&self, ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        ctx.events()
            .emit("startup_ready", &json!({"ready": true}))
            .await
            .map_err(|error| ExtensionError::Internal(error.to_string()))
    }
}

#[async_trait::async_trait]
impl Extension for UnhealthyExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("unhealthy", &[])
    }

    async fn health(&self) -> Result<(), ExtensionError> {
        Err(ExtensionError::Internal("dependency unavailable".into()))
    }
}

#[async_trait::async_trait]
impl Extension for ManagedTaskExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("managed-task", &[])
    }

    fn register(&self, _reg: &mut Registrar) {}

    async fn start(&self, ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        let shutdown = ctx.cancellation().clone();
        let task_stopped = Arc::clone(&self.task_stopped);
        ctx.tasks().spawn("wait-for-stop", async move {
            shutdown.cancelled().await;
            task_stopped.store(true, Ordering::SeqCst);
        });
        Ok(())
    }

    async fn stop(&self, reason: StopReason) -> Result<(), ExtensionError> {
        assert_eq!(reason, self.expected_reason);
        assert!(self.task_stopped.load(Ordering::SeqCst));
        self.stopped.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait::async_trait]
impl Extension for DeferredTaskExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("deferred-task", &[])
    }

    async fn start(&self, ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        let task_started = Arc::clone(&self.task_started);
        ctx.tasks().spawn("deferred", async move {
            task_started.store(true, Ordering::SeqCst);
        });
        Ok(())
    }
}

#[tokio::test]
async fn privileged_registrations_require_their_declared_capabilities() {
    let cases = [
        (
            CapabilityRegistration::Event,
            "event",
            ExtensionCapability::EmitEvents,
        ),
        (
            CapabilityRegistration::Compact,
            "compact",
            ExtensionCapability::SessionHistory,
        ),
        (
            CapabilityRegistration::UserMessageEnvelope,
            "user_message_envelope",
            ExtensionCapability::ProviderRequest,
        ),
        (
            CapabilityRegistration::BeforeProvider,
            "provider",
            ExtensionCapability::ProviderRequest,
        ),
        (
            CapabilityRegistration::AfterProvider,
            "provider",
            ExtensionCapability::ProviderRequest,
        ),
        (
            CapabilityRegistration::BlockingPreTool,
            "pre_tool_use",
            ExtensionCapability::ToolIntercept,
        ),
        (
            CapabilityRegistration::BlockingPostTool,
            "post_tool_use",
            ExtensionCapability::ToolIntercept,
        ),
        (
            CapabilityRegistration::ContinueAfterStop,
            "continue_after_stop",
            ExtensionCapability::TurnContinuationControl,
        ),
    ];

    for (registration, hook, capability) in cases {
        let runner = ExtensionRunner::new(Duration::from_secs(1));
        let error = runner
            .register(Arc::new(RegistrationProbeExtension {
                capabilities: Vec::new(),
                registration,
            }))
            .await
            .expect_err("privileged registration must declare its capability");
        assert!(matches!(
            error,
            ExtensionError::MissingCapability {
                hook: actual_hook,
                capability: actual_capability,
                ..
            } if actual_hook == hook && actual_capability == capability
        ));

        let runner = ExtensionRunner::new(Duration::from_secs(1));
        runner
            .register(Arc::new(RegistrationProbeExtension {
                capabilities: vec![capability],
                registration,
            }))
            .await
            .expect("declared capability must allow registration");
    }
}

#[tokio::test]
async fn only_turn_entry_lifecycle_events_may_block() {
    for event in [ExtensionEvent::TurnStart, ExtensionEvent::UserPromptSubmit] {
        ExtensionRunner::new(Duration::from_secs(1))
            .register(Arc::new(LifecycleModeProbeExtension { event }))
            .await
            .expect("turn-entry lifecycle event may block");
    }

    for event in [
        ExtensionEvent::SessionStart,
        ExtensionEvent::SessionResume,
        ExtensionEvent::TurnEnd,
        ExtensionEvent::StepEnd,
        ExtensionEvent::SessionShutdown,
    ] {
        let error = ExtensionRunner::new(Duration::from_secs(1))
            .register(Arc::new(LifecycleModeProbeExtension {
                event: event.clone(),
            }))
            .await
            .expect_err("observe-only lifecycle event must reject blocking mode");
        assert!(matches!(
            error,
            ExtensionError::InvalidLifecycleMode {
                event: actual_event,
                ..
            } if actual_event == event
        ));
    }
}

#[tokio::test]
async fn unregister_stops_extension_and_managed_tasks() {
    let started = Arc::new(AtomicUsize::new(0));
    let stopped = Arc::new(AtomicUsize::new(0));
    let task_stopped = Arc::new(AtomicBool::new(false));
    let runner = ExtensionRunner::new(Duration::from_secs(1));

    let registered = runner
        .register(Arc::new(ManagedTaskExtension {
            started: Arc::clone(&started),
            stopped: Arc::clone(&stopped),
            task_stopped: Arc::clone(&task_stopped),
            expected_reason: StopReason::Disabled,
        }))
        .await
        .unwrap();
    assert!(registered);

    let unregistered = runner
        .unregister("managed-task", StopReason::Disabled)
        .await
        .unwrap();
    assert!(unregistered);
    assert!(runner.shutdown().await.is_empty());
    assert_eq!(started.load(Ordering::SeqCst), 1);
    assert_eq!(stopped.load(Ordering::SeqCst), 1);
    assert!(task_stopped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn cached_extension_tool_does_not_hold_retired_generation_alive() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(ToolRetirementProbeExtension {
            stopped: Arc::clone(&stopped),
        }))
        .await
        .unwrap();
    let cached_tool = runner
        .tool_catalog_snapshot_typed("D:/workspace")
        .await
        .tools
        .into_iter()
        .find(|tool| tool.definition().name == "retirementProbe")
        .unwrap();

    assert!(
        runner
            .unregister("tool-retirement-probe", StopReason::Reload)
            .await
            .unwrap()
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while stopped.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cached tool wrapper must not block extension retirement");
    assert_eq!(stopped.load(Ordering::SeqCst), 1);

    let result = cached_tool
        .execute(
            json!({}),
            &ToolExecutionContext::new(
                "session".into(),
                "D:/workspace",
                None,
                None,
                ToolCapabilities::default(),
            ),
        )
        .await
        .unwrap();
    assert!(result.is_error);
    assert!(result.content.contains("is not available"));
    assert_eq!(
        result.metadata.get("extensionId"),
        Some(&json!("tool-retirement-probe"))
    );
    assert!(runner.shutdown().await.is_empty());
}

#[tokio::test]
async fn reload_cancels_active_call_before_pinned_generation_retires() {
    let version_one_stops = Arc::new(AtomicUsize::new(0));
    let version_two_stops = Arc::new(AtomicUsize::new(0));
    let version_one_call_started = Arc::new(Notify::new());
    let lifecycle = Arc::new(Mutex::new(Vec::new()));
    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(5)));

    runner
        .register(Arc::new(GenerationProbeExtension {
            label: "v1",
            stopped: Arc::clone(&version_one_stops),
            call_started: Some(Arc::clone(&version_one_call_started)),
            lifecycle: Some(Arc::clone(&lifecycle)),
        }))
        .await
        .unwrap();
    let version_one_view = runner.extension_view().await;
    let version_one_generation = version_one_view.generation();
    let active_call_view = runner.extension_view().await;
    assert_eq!(active_call_view.generation(), version_one_generation);
    let mut active_call = tokio::spawn(async move {
        active_call_view
            .emit_pre_tool_use(pre_tool_use_ctx("probe", json!({})))
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), version_one_call_started.notified())
        .await
        .expect("v1 handler should begin before reload");

    let removed = tokio::time::timeout(
        Duration::from_secs(1),
        runner.unregister("generation-probe", StopReason::Reload),
    )
    .await
    .expect("unregister must not wait for an in-flight generation")
    .unwrap();
    assert!(removed);

    let mut register_version_two = {
        let runner = Arc::clone(&runner);
        let lifecycle = Arc::clone(&lifecycle);
        let version_two_stops = Arc::clone(&version_two_stops);
        tokio::spawn(async move {
            runner
                .register(Arc::new(GenerationProbeExtension {
                    label: "v2",
                    stopped: version_two_stops,
                    call_started: None,
                    lifecycle: Some(lifecycle),
                }))
                .await
        })
    };

    let version_one_result = tokio::time::timeout(Duration::from_secs(1), &mut active_call)
        .await
        .expect("reload must cancel the active v1 call before its pinned view is released")
        .unwrap()
        .unwrap();
    assert!(matches!(
        version_one_result,
        PreToolUseResult::ModifyInput { tool_input }
            if tool_input == json!({ "generation": "v1" })
    ));
    assert_eq!(version_one_stops.load(Ordering::SeqCst), 0);
    assert_eq!(lifecycle.lock().unwrap().as_slice(), &["v1"]);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut register_version_two)
            .await
            .is_err(),
        "replacement must wait for the old published view"
    );

    drop(version_one_view);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), &mut register_version_two)
            .await
            .expect("v2 registration should resume after v1 retirement")
            .unwrap()
            .unwrap()
    );
    assert_eq!(version_one_stops.load(Ordering::SeqCst), 1);
    assert_eq!(
        lifecycle.lock().unwrap().as_slice(),
        &["v1", "v1_stop", "v2"]
    );

    let context = || pre_tool_use_ctx("probe", json!({}));
    let version_two_view = runner.extension_view().await;
    assert!(version_two_view.generation() > version_one_generation);
    let turn_view = runner.turn_extension_view();
    assert_eq!(turn_view.generation(), version_two_view.generation());
    assert_eq!(
        turn_view.tool_catalog().revision(),
        version_two_view.generation()
    );
    let version_two_result = version_two_view.emit_pre_tool_use(context()).await.unwrap();
    assert!(matches!(
        version_two_result,
        PreToolUseResult::ModifyInput { tool_input }
            if tool_input == json!({ "generation": "v2" })
    ));

    drop(version_two_view);
    drop(turn_view);
    assert!(runner.shutdown().await.is_empty());
    assert_eq!(version_one_stops.load(Ordering::SeqCst), 1);
    assert_eq!(version_two_stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_start_hands_registration_to_the_retirement_barrier() {
    let startup_tasks = Arc::new(Mutex::new(None));
    let write_started = Arc::new(Notify::new());
    let write_release = Arc::new(Notify::new());
    let stop_entered = Arc::new(Notify::new());
    let stop_release = Arc::new(Notify::new());
    let stop_reason = Arc::new(Mutex::new(None));
    let version_one_stops = Arc::new(AtomicUsize::new(0));
    let version_two_stops = Arc::new(AtomicUsize::new(0));
    let lifecycle = Arc::new(Mutex::new(Vec::new()));
    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));

    let registration = {
        let runner = Arc::clone(&runner);
        let extension = CancelledStartupExtension {
            tasks: Arc::clone(&startup_tasks),
            write_started: Arc::clone(&write_started),
            write_release: Arc::clone(&write_release),
            stop_entered: Arc::clone(&stop_entered),
            stop_release: Arc::clone(&stop_release),
            stop_reason: Arc::clone(&stop_reason),
            stops: Arc::clone(&version_one_stops),
            lifecycle: Arc::clone(&lifecycle),
        };
        tokio::spawn(async move { runner.register(Arc::new(extension)).await })
    };
    tokio::time::timeout(Duration::from_secs(1), write_started.notified())
        .await
        .expect("startup must-finish write should begin");
    registration.abort();
    assert!(registration.await.unwrap_err().is_cancelled());

    let mut replacement = {
        let runner = Arc::clone(&runner);
        let lifecycle = Arc::clone(&lifecycle);
        let version_two_stops = Arc::clone(&version_two_stops);
        tokio::spawn(async move {
            runner
                .register(Arc::new(GenerationProbeExtension {
                    label: "v2",
                    stopped: version_two_stops,
                    call_started: None,
                    lifecycle: Some(lifecycle),
                }))
                .await
        })
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut replacement)
            .await
            .is_err(),
        "replacement must wait for the cancelled start's must-finish write"
    );

    write_release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), stop_entered.notified())
        .await
        .expect("cancelled registration should run StartupFailed cleanup");
    assert_eq!(
        *stop_reason.lock().unwrap(),
        Some(StopReason::StartupFailed)
    );
    assert_eq!(lifecycle.lock().unwrap().as_slice(), &["write_done"]);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut replacement)
            .await
            .is_err(),
        "replacement must wait until StartupFailed stop finishes"
    );

    stop_release.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), &mut replacement)
            .await
            .expect("replacement should resume after startup retirement")
            .unwrap()
            .unwrap()
    );
    assert_eq!(version_one_stops.load(Ordering::SeqCst), 1);
    assert_eq!(
        lifecycle.lock().unwrap().as_slice(),
        &["write_done", "v1_stop", "v2"]
    );
    let tasks = startup_tasks
        .lock()
        .unwrap()
        .clone()
        .expect("startup should expose its task owner");
    assert!(tasks.wait(Duration::from_millis(10)).await);
    assert!(matches!(
        tasks.run_to_completion("late-write", async {}).await,
        Err(astrcode_extension_sdk::extension::ExtensionTaskError::ShuttingDown { .. })
    ));

    assert!(runner.shutdown().await.is_empty());
    assert_eq!(version_one_stops.load(Ordering::SeqCst), 1);
    assert_eq!(version_two_stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_shutdown_does_not_abandon_pending_retirement() {
    let stopped = Arc::new(AtomicUsize::new(0));
    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
    runner
        .register(Arc::new(GenerationProbeExtension {
            label: "v1",
            stopped: Arc::clone(&stopped),
            call_started: None,
            lifecycle: None,
        }))
        .await
        .unwrap();
    let active_view = runner.extension_view().await;
    assert!(
        runner
            .unregister("generation-probe", StopReason::Reload)
            .await
            .unwrap()
    );

    let mut first_shutdown = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { runner.shutdown().await })
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut first_shutdown)
            .await
            .is_err(),
        "shutdown should wait for the active generation"
    );
    first_shutdown.abort();
    assert!(first_shutdown.await.unwrap_err().is_cancelled());

    drop(active_view);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), runner.shutdown())
            .await
            .expect("a later shutdown must recover the pending retirement")
            .is_empty()
    );
    assert_eq!(stopped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retirement_tickets_isolate_reload_failures_by_generation() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    assert!(
        runner
            .register(Arc::new(RetirementFailureExtension { fail_stop: true }))
            .await
            .unwrap()
    );
    let failed_retirement = runner
        .unregister_with_retirement("retirement-failure-probe", StopReason::Disabled)
        .await
        .unwrap()
        .expect("v1 should begin retirement");
    drop(failed_retirement);

    assert!(
        runner
            .register(Arc::new(RetirementFailureExtension { fail_stop: false }))
            .await
            .expect("the replacement only waits for the retirement barrier")
    );
    let successful_retirement = runner
        .unregister_with_retirement("retirement-failure-probe", StopReason::Reload)
        .await
        .unwrap()
        .expect("v2 should begin retirement");
    successful_retirement
        .wait()
        .await
        .expect("v2 retirement must not consume v1's orphaned failure");

    assert!(
        runner
            .register(Arc::new(RetirementFailureExtension { fail_stop: false }))
            .await
            .expect("v3 should start after v2 retirement")
    );

    let errors = runner.shutdown().await;
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("intentional stop failure"));
}

#[tokio::test]
async fn deferred_task_handle_activates_published_registration_on_drop() {
    let task_started = Arc::new(AtomicBool::new(false));
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    let activation = runner
        .register_deferred(
            Arc::new(DeferredTaskExtension {
                task_started: Arc::clone(&task_started),
            }),
            None,
            "test:deferred-task".into(),
            "v1".into(),
        )
        .await
        .unwrap()
        .expect("new registration should return an activation handle");

    assert_eq!(
        runner.registered_extension_ids().await,
        vec!["deferred-task"]
    );
    tokio::task::yield_now().await;
    assert!(!task_started.load(Ordering::SeqCst));

    drop(activation);
    tokio::time::timeout(Duration::from_secs(1), async {
        while !task_started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn shutdown_stops_all_extensions_with_shutdown_reason() {
    let started = Arc::new(AtomicUsize::new(0));
    let stopped = Arc::new(AtomicUsize::new(0));
    let task_stopped = Arc::new(AtomicBool::new(false));
    let runner = ExtensionRunner::new(Duration::from_secs(1));

    runner
        .register(Arc::new(ManagedTaskExtension {
            started: Arc::clone(&started),
            stopped: Arc::clone(&stopped),
            task_stopped: Arc::clone(&task_stopped),
            expected_reason: StopReason::Shutdown,
        }))
        .await
        .unwrap();

    let errors = runner.shutdown().await;
    assert!(errors.is_empty());
    assert_eq!(started.load(Ordering::SeqCst), 1);
    assert_eq!(stopped.load(Ordering::SeqCst), 1);
    assert!(task_stopped.load(Ordering::SeqCst));
    assert_eq!(runner.count().await, 0);
}

#[tokio::test]
async fn shutdown_is_terminal_and_idempotent() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));

    assert!(runner.shutdown().await.is_empty());
    assert!(matches!(
        runner.register(Arc::new(StateProbeExtension)).await,
        Err(ExtensionError::Internal(message))
            if message == "extension runner is shutting down"
    ));
    assert!(runner.shutdown().await.is_empty());
}

#[tokio::test]
async fn register_builds_attributed_startup_context() {
    let received = Arc::new(Mutex::new(None));
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner.update_extension_configs(BTreeMap::from([(
        "startup-directory".into(),
        json!({ "version": 7 }),
    )]));

    runner
        .register_with_startup_working_dir(
            Arc::new(StartupDirectoryExtension {
                received: Arc::clone(&received),
            }),
            Some("D:/workspace"),
        )
        .await
        .unwrap();

    assert_eq!(
        received.lock().unwrap().as_ref(),
        Some(&StartupContextSnapshot {
            extension_id: "startup-directory".into(),
            startup_working_dir: Some(PathBuf::from("D:/workspace")),
            global_data_dir: Some(
                astrcode_core::config::defaults::astrcode_dir()
                    .join("extension_data")
                    .join("startup-directory")
            ),
            session_context_available: false,
            config_version: 7,
            cancelled: false,
        })
    );
}

#[tokio::test]
async fn start_can_emit_declared_event_through_bound_startup_channel() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    runner.bind_startup_event_channel(event_tx);

    runner
        .register(Arc::new(StartupEventExtension))
        .await
        .unwrap();

    let event = event_rx.recv().await.unwrap();
    assert!(matches!(
        event,
        EventPayload::Durable(DurableEventPayload::ExtensionEvent(ExtensionEventData {
            extension_id,
            event_type,
            schema_version: 1,
            payload,
        })) if extension_id == "startup-event"
            && event_type == "startup_ready"
            && payload == json!({"ready": true})
    ));
}

#[tokio::test]
async fn check_health_reports_extension_failure() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner.register(Arc::new(UnhealthyExtension)).await.unwrap();

    let reports = runner.check_health().await;

    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].extension_id, "unhealthy");
    assert!(!reports[0].is_healthy());
    assert!(
        reports[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("dependency unavailable"))
    );
}

#[tokio::test]
async fn extension_tool_receives_session_state_by_default() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(StateProbeExtension))
        .await
        .unwrap();
    let tool = runner
        .tool_catalog_snapshot_typed("D:/workspace")
        .await
        .tools
        .into_iter()
        .next()
        .unwrap();
    let ctx = ToolExecutionContext::new(
        "session".into(),
        "D:/workspace",
        None,
        None,
        ToolCapabilities {
            paths: astrcode_core::tool::ToolSessionPaths {
                store_dir: Some("D:/session".into()),
            },
            ..Default::default()
        },
    );

    let result = tool.execute(json!({}), &ctx).await.unwrap();
    assert_eq!(result.content, "true");
}

#[tokio::test]
async fn timed_out_discovery_returns_partial_catalog_with_static_tools() {
    let runner = ExtensionRunner::new(Duration::from_millis(5));
    runner
        .register(Arc::new(StateProbeExtension))
        .await
        .unwrap();
    runner
        .register(Arc::new(SlowToolDiscoveryExtension))
        .await
        .unwrap();

    let snapshot = runner.tool_catalog_snapshot_typed("D:/workspace").await;

    assert_eq!(snapshot.completeness, ToolCatalogCompleteness::Partial);
    assert!(
        snapshot
            .tools
            .iter()
            .any(|tool| tool.definition().name == "stateProbe")
    );
    assert_eq!(snapshot.diagnostics.len(), 1);
    assert_eq!(snapshot.diagnostics[0].source, "slow-discovery");
}

#[tokio::test]
async fn extension_tool_receives_small_model_only_when_declared() {
    for (small_model_allowed, session_control_allowed, expected) in [
        (false, false, "false"),
        (true, false, "true"),
        (false, true, "false"),
    ] {
        let runner = ExtensionRunner::new(Duration::from_secs(1));
        runner
            .register(Arc::new(SmallModelProbeExtension {
                small_model_allowed,
                session_control_allowed,
            }))
            .await
            .unwrap();
        let tool = runner
            .tool_catalog_snapshot_typed("D:/workspace")
            .await
            .tools
            .into_iter()
            .next()
            .unwrap();
        let ctx = ToolExecutionContext::new(
            "session".into(),
            "D:/workspace",
            None,
            None,
            ToolCapabilities {
                models: astrcode_core::tool::ToolModelAccess {
                    tiers: astrcode_core::tool::LlmModelIds {
                        small: Some("small-model".into()),
                        ..Default::default()
                    },
                },
                ..Default::default()
            },
        );

        let result = tool.execute(json!({}), &ctx).await.unwrap();
        assert_eq!(result.content, expected);
    }
}

#[tokio::test]
async fn targeted_pre_tool_hook_only_runs_for_matching_tool() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(TargetedPreHookExtension {
            calls: Arc::clone(&calls),
        }))
        .await
        .unwrap();

    let base_ctx = |tool_name: &str| pre_tool_use_ctx(tool_name, json!({}));

    runner
        .emit_pre_tool_use(base_ctx("otherTool"))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    runner
        .emit_pre_tool_use(base_ctx("targetTool"))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let diagnostics = runner.diagnostics_snapshot();
    let hook_diagnostics = diagnostics.get("targeted-pre-hook").unwrap();
    assert_eq!(hook_diagnostics.hook_calls, 1);
    assert_eq!(hook_diagnostics.last_hook.as_deref(), Some("pre_tool_use"));
}

#[tokio::test]
async fn diagnostics_records_register_and_start_failure_states() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    let err = runner.register(Arc::new(StartFailingExtension)).await;
    assert!(err.is_err());

    let diagnostics = runner.diagnostics_snapshot();
    let diagnostics = diagnostics.get("start-failing").unwrap();
    assert_eq!(
        diagnostics.register.status,
        super::ExtensionStageStatus::Succeeded
    );
    assert_eq!(
        diagnostics.start.status,
        super::ExtensionStageStatus::Failed
    );
    assert!(
        diagnostics
            .start
            .error
            .as_deref()
            .is_some_and(|error| error.contains("startup dependency missing"))
    );
}

#[tokio::test]
async fn config_notifications_are_ordered_idempotent_and_retry_failures() {
    let state = Arc::new(ConfigChangeProbeState::default());
    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
    let config =
        |version| BTreeMap::from([("config-change-probe".into(), json!({"version": version}))]);
    runner.update_extension_configs(config(1));
    runner
        .register(Arc::new(ConfigChangeProbeExtension(Arc::clone(&state))))
        .await
        .unwrap();
    let stable_generation = || match runner.runtime_snapshot_state() {
        RuntimeSnapshotState::Stable(generation) => generation,
        RuntimeSnapshotState::Updating => panic!("runtime must be stable after update completion"),
    };
    let registered_generation = stable_generation();
    assert!(registered_generation > 0);

    runner.update_extension_configs(config(2));
    assert!(runner.notify_config_changed().await.is_empty());
    let version_two_generation = stable_generation();
    assert!(version_two_generation > registered_generation);
    assert!(runner.notify_config_changed().await.is_empty());
    assert_eq!(stable_generation(), version_two_generation);
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);
    assert_eq!(state.applied_version.load(Ordering::SeqCst), 2);

    state.fail_next.store(true, Ordering::SeqCst);
    runner.update_extension_configs(config(3));
    assert_eq!(runner.notify_config_changed().await.len(), 1);
    let failed_generation = stable_generation();
    assert!(failed_generation > version_two_generation);
    assert!(runner.notify_config_changed().await.is_empty());
    let version_three_generation = stable_generation();
    assert!(version_three_generation > failed_generation);
    assert!(runner.notify_config_changed().await.is_empty());
    assert_eq!(stable_generation(), version_three_generation);
    assert_eq!(state.calls.load(Ordering::SeqCst), 3);
    assert_eq!(state.applied_version.load(Ordering::SeqCst), 3);

    state.block_version.store(4, Ordering::SeqCst);
    runner.update_extension_configs(config(4));
    let notify_v4 = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { runner.notify_config_changed().await })
    };
    state.entered.notified().await;
    assert_eq!(
        runner.runtime_snapshot_state(),
        RuntimeSnapshotState::Updating
    );
    runner.update_extension_configs(config(5));
    let notify_v5 = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { runner.notify_config_changed().await })
    };
    tokio::task::yield_now().await;
    assert_eq!(state.applied_version.load(Ordering::SeqCst), 3);
    state.release.notify_one();
    assert!(notify_v4.await.unwrap().is_empty());
    assert!(notify_v5.await.unwrap().is_empty());
    assert!(matches!(
        runner.runtime_snapshot_state(),
        RuntimeSnapshotState::Stable(_)
    ));
    assert_eq!(state.applied_version.load(Ordering::SeqCst), 5);

    state.block_version.store(6, Ordering::SeqCst);
    runner.update_extension_configs(config(6));
    let notify_v6 = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { runner.notify_config_changed().await })
    };
    state.entered.notified().await;
    let unregister = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move {
            runner
                .unregister("config-change-probe", StopReason::Disabled)
                .await
        })
    };
    tokio::task::yield_now().await;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(250),
            runner.register(Arc::new(StateProbeExtension)),
        )
        .await
        .unwrap()
        .unwrap()
    );
    assert!(!state.stopped.load(Ordering::SeqCst));
    state.release.notify_one();
    assert!(notify_v6.await.unwrap().is_empty());
    assert!(unregister.await.unwrap().unwrap());
    assert!(runner.shutdown().await.is_empty());
    assert!(state.stopped.load(Ordering::SeqCst));
}

#[tokio::test]
async fn config_update_waits_for_turn_views_and_recovers_after_wait_timeout() {
    let state = Arc::new(ConfigChangeProbeState::default());
    let runner = Arc::new(ExtensionRunner::new(Duration::from_millis(100)));
    let config =
        |version| BTreeMap::from([("config-change-probe".into(), json!({"version": version}))]);
    runner.update_extension_configs(config(1));
    runner
        .register(Arc::new(ConfigChangeProbeExtension(Arc::clone(&state))))
        .await
        .unwrap();

    let version_one_view = runner.turn_extension_view();
    let version_one_generation = version_one_view.generation();
    runner.update_extension_configs(config(2));
    let notify_version_two = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { runner.notify_config_changed().await })
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        while runner.runtime_snapshot_state() != RuntimeSnapshotState::Updating {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("config update must publish Updating before waiting for turn views");
    assert_eq!(state.calls.load(Ordering::SeqCst), 0);

    drop(version_one_view);
    assert!(notify_version_two.await.unwrap().is_empty());
    assert_eq!(state.applied_version.load(Ordering::SeqCst), 2);
    let version_two_view = runner.turn_extension_view();
    assert!(version_two_view.generation() > version_one_generation);

    runner.update_extension_configs(config(3));
    let errors = runner.notify_config_changed().await;
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("active turn extension view"));
    assert!(errors[0].contains("was not applied"));
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);
    let stable_generation = match runner.runtime_snapshot_state() {
        RuntimeSnapshotState::Stable(generation) => generation,
        RuntimeSnapshotState::Updating => panic!("timed out update must restore stable state"),
    };
    let recovered_view = runner.turn_extension_view();
    assert_eq!(recovered_view.generation(), stable_generation);
    drop(recovered_view);

    drop(version_two_view);
    assert!(runner.notify_config_changed().await.is_empty());
    assert_eq!(state.applied_version.load(Ordering::SeqCst), 3);
    assert!(runner.shutdown().await.is_empty());
}

#[tokio::test]
async fn recorded_hook_tracks_error_and_timeout() {
    let runner = ExtensionRunner::new(Duration::from_millis(10));
    let view = runner.extension_view().await;

    assert!(
        view.run_recorded_hook::<()>(
            "probe",
            "pre_tool_use",
            tokio_util::sync::CancellationToken::new(),
            async { Err(ExtensionError::Internal("injected failure".into())) },
        )
        .await
        .is_err()
    );
    assert!(matches!(
        view.run_recorded_hook(
            "probe",
            "pre_tool_use",
            tokio_util::sync::CancellationToken::new(),
            std::future::pending::<Result<(), ExtensionError>>(),
        )
        .await,
        Err(ExtensionError::Timeout(10))
    ));

    let diagnostics = runner.diagnostics_snapshot();
    let diagnostics = diagnostics.get("probe").unwrap();
    assert_eq!(diagnostics.hook_calls, 2);
    assert_eq!(diagnostics.hook_timeouts, 1);
    assert!(
        diagnostics
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("timed out"))
    );
}

#[tokio::test]
async fn operation_timeout_bounds_advisory_hooks_and_stop() {
    let runner = ExtensionRunner::new(Duration::from_millis(10));
    runner
        .register(Arc::new(OperationTimeoutExtension))
        .await
        .unwrap();

    let result = runner
        .emit_provider(
            ProviderEvent::AfterResponse,
            RuntimeProviderContext::new(runtime_hook_call(), Vec::new()),
        )
        .await
        .unwrap();
    assert!(matches!(result, ProviderResult::Allow));

    let diagnostics = runner.diagnostics_snapshot();
    let diagnostics = diagnostics.get("operation-timeout").unwrap();
    assert_eq!(diagnostics.hook_calls, 1);
    assert_eq!(diagnostics.hook_timeouts, 1);

    assert!(
        runner
            .unregister("operation-timeout", StopReason::Disabled)
            .await
            .unwrap()
    );
    let errors = runner.shutdown().await;
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("timed out after 10ms"));
    assert_eq!(runner.count().await, 0);
}

#[tokio::test]
async fn startup_timeout_drops_suspended_tasks_and_rolls_back_partial_start() {
    let task_started = Arc::new(AtomicBool::new(false));
    let stop_reason = Arc::new(Mutex::new(None));
    let runner = ExtensionRunner::new(Duration::from_millis(20));

    let error = runner
        .register(Arc::new(StartupTimeoutExtension {
            task_started: Arc::clone(&task_started),
            stop_reason: Arc::clone(&stop_reason),
        }))
        .await
        .unwrap_err();

    assert!(matches!(error, ExtensionError::Timeout(20)));
    assert!(!task_started.load(Ordering::SeqCst));
    assert_eq!(
        *stop_reason.lock().unwrap(),
        Some(StopReason::StartupFailed)
    );
    assert_eq!(runner.count().await, 0);
}

#[test]
fn stringified_boolean_normalization_follows_nested_schema() {
    let schema = json!({
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "enabled": { "type": "boolean" },
                        "label": { "type": "string" }
                    }
                }
            }
        }
    });
    let mut arguments = json!({
        "items": [
            {"enabled": "true", "label": "true"},
            {"enabled": "FALSE", "label": "false"},
            {"enabled": "yes", "label": "unchanged"}
        ]
    });

    assert_eq!(normalize_stringified_booleans(&mut arguments, &schema), 2);
    assert_eq!(arguments["items"][0]["enabled"], true);
    assert_eq!(arguments["items"][1]["enabled"], false);
    assert_eq!(arguments["items"][2]["enabled"], "yes");
    assert_eq!(arguments["items"][0]["label"], "true");
}

#[tokio::test]
async fn provider_response_hook_observes_without_blocking() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(BlockingProviderResponseExtension))
        .await
        .unwrap();

    let result = runner
        .emit_provider(
            ProviderEvent::AfterResponse,
            RuntimeProviderContext::new(runtime_hook_call(), Vec::new()),
        )
        .await
        .unwrap();

    assert!(matches!(result, ProviderResult::Allow));
    let diagnostics = runner.diagnostics_snapshot();
    let diagnostics = diagnostics.get("provider-response-observer").unwrap();
    assert_eq!(diagnostics.hook_calls, 1);
    assert_eq!(
        diagnostics.last_hook.as_deref(),
        Some("after_provider_response")
    );
}

#[tokio::test]
async fn continue_after_stop_default_options_do_not_limit_continuations() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(ContinueAfterStopProbeExtension {
            id: "default-continue",
            options: ContinueAfterStopOptions::default(),
            calls: Arc::clone(&calls),
        }))
        .await
        .unwrap();

    let result = runner
        .emit_continue_after_stop(continue_after_stop_ctx(100))
        .await
        .unwrap();

    assert_eq!(result, ContinueAfterStopResult::ContinueOneStep);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn continue_after_stop_limited_options_stop_after_configured_continuations() {
    let calls = Arc::new(AtomicUsize::new(0));
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(ContinueAfterStopProbeExtension {
            id: "limited-continue",
            options: ContinueAfterStopOptions::limited(3),
            calls: Arc::clone(&calls),
        }))
        .await
        .unwrap();

    let allowed = runner
        .emit_continue_after_stop(continue_after_stop_ctx(2))
        .await
        .unwrap();
    assert_eq!(allowed, ContinueAfterStopResult::ContinueOneStep);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let blocked = runner
        .emit_continue_after_stop(continue_after_stop_ctx(3))
        .await
        .unwrap();
    assert_eq!(blocked, ContinueAfterStopResult::EndTurn);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn user_message_envelope_folds_text_by_priority() {
    let replace_calls = Arc::new(AtomicUsize::new(0));
    let append_calls = Arc::new(AtomicUsize::new(0));
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(UserMessageEnvelopeProbeExtension {
            id: "replace-envelope",
            priority: 10,
            result: UserMessageEnvelopeResult::ReplaceText {
                text: "rewritten".into(),
            },
            calls: Arc::clone(&replace_calls),
        }))
        .await
        .unwrap();
    runner
        .register(Arc::new(UserMessageEnvelopeProbeExtension {
            id: "append-envelope",
            priority: 0,
            result: UserMessageEnvelopeResult::AppendText {
                text: "tail".into(),
            },
            calls: Arc::clone(&append_calls),
        }))
        .await
        .unwrap();

    let result = runner
        .emit_user_message_envelope(user_message_envelope_ctx("original"))
        .await
        .unwrap();

    assert_eq!(
        result,
        UserMessageEnvelopeResult::ReplaceText {
            text: "rewritten\n\ntail".into()
        }
    );
    assert_eq!(replace_calls.load(Ordering::SeqCst), 1);
    assert_eq!(append_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn user_message_envelope_block_short_circuits_later_handlers() {
    let block_calls = Arc::new(AtomicUsize::new(0));
    let append_calls = Arc::new(AtomicUsize::new(0));
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(UserMessageEnvelopeProbeExtension {
            id: "block-envelope",
            priority: 10,
            result: UserMessageEnvelopeResult::Block {
                reason: "blocked".into(),
            },
            calls: Arc::clone(&block_calls),
        }))
        .await
        .unwrap();
    runner
        .register(Arc::new(UserMessageEnvelopeProbeExtension {
            id: "append-after-block",
            priority: 0,
            result: UserMessageEnvelopeResult::AppendText {
                text: "unreachable".into(),
            },
            calls: Arc::clone(&append_calls),
        }))
        .await
        .unwrap();

    let result = runner
        .emit_user_message_envelope(user_message_envelope_ctx("original"))
        .await
        .unwrap();

    assert_eq!(
        result,
        UserMessageEnvelopeResult::Block {
            reason: "blocked".into()
        }
    );
    assert_eq!(block_calls.load(Ordering::SeqCst), 1);
    assert_eq!(append_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn registry_snapshot_exposes_registered_extension_declarations() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(StateProbeExtension))
        .await
        .unwrap();

    let snapshot = runner.registry_snapshot().await;
    let declaration = snapshot
        .extensions
        .iter()
        .find(|extension| extension.id == "state-probe")
        .unwrap();

    assert!(declaration.capabilities.is_empty());
    assert_eq!(declaration.tools.len(), 1);
    assert_eq!(declaration.tools[0].name, "stateProbe");
    assert!(!declaration.dynamic_tools);
}

#[tokio::test]
async fn command_resolution_uses_source_priority_then_declared_priority() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(CommandProbeExtension {
            id: "astrcode-skill",
            command_name: "demo",
            priority: 100,
            argument_completions: false,
        }))
        .await
        .unwrap();
    runner
        .register(Arc::new(CommandProbeExtension {
            id: "normal-low",
            command_name: "demo",
            priority: 1,
            argument_completions: false,
        }))
        .await
        .unwrap();
    runner
        .register(Arc::new(CommandProbeExtension {
            id: "normal-high",
            command_name: "demo",
            priority: 5,
            argument_completions: false,
        }))
        .await
        .unwrap();

    let resolved = runner.resolve_commands_for_typed(".").await;
    let demo = resolved
        .iter()
        .find(|command| command.command.name == "demo")
        .expect("demo command");

    assert_eq!(demo.extension_id, "normal-high");
    assert_eq!(demo.source, CommandSource::Extension);
    assert_eq!(demo.shadowed.len(), 2);
    assert!(demo.shadowed.iter().any(|command| {
        command.extension_id == "astrcode-skill" && command.source == CommandSource::Skill
    }));
}

#[tokio::test]
async fn command_completion_dispatches_to_resolved_handler() {
    let mut invalid = Registrar::new();
    invalid.command(
        SlashCommand {
            name: "missing-completion".into(),
            description: "invalid declaration probe".into(),
            args_schema: None,
            requires_idle: false,
            argument_completions: true,
            priority: 0,
        },
        Arc::new(ExecuteOnlyCommandProbe),
    );
    let invalid = invalid.finish(extension_manifest("missing-completion", &[]));
    assert!(matches!(
        invalid,
        Err(error) if error.to_string().contains("handler does not support")
    ));

    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(CommandProbeExtension {
            id: "complete-low",
            command_name: "pick",
            priority: 0,
            argument_completions: true,
        }))
        .await
        .unwrap();
    runner
        .register(Arc::new(CommandProbeExtension {
            id: "complete-high",
            command_name: "pick",
            priority: 10,
            argument_completions: true,
        }))
        .await
        .unwrap();

    let runtime = command_ctx();
    let resolved = runner
        .resolve_commands_for_typed(runtime.working_dir())
        .await
        .into_iter()
        .find(|resolved| resolved.command.name == "pick")
        .expect("pick command");
    let completions = runner
        .complete_resolved_command_typed(&resolved, "de", 2, &runtime)
        .await
        .unwrap();

    assert_eq!(completions.items.len(), 1);
    assert_eq!(completions.items[0].label, "complete-high:de:2");
    assert_eq!(completions.items[0].insert_text, "complete-high");
    assert_eq!(
        completions.items[0].detail.as_deref(),
        Some("complete-high:session:pick:.:mock:/tmp/session-store/extension_data/complete-high")
    );
}

#[tokio::test]
async fn cached_command_does_not_block_retirement_and_becomes_unavailable() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(CommandProbeExtension {
            id: "retired-command",
            command_name: "retire",
            priority: 0,
            argument_completions: false,
        }))
        .await
        .unwrap();
    let command = runner
        .resolve_commands_for_typed(".")
        .await
        .into_iter()
        .find(|command| command.command.name == "retire")
        .unwrap();

    assert!(
        runner
            .unregister("retired-command", StopReason::Disabled)
            .await
            .unwrap()
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(1), runner.shutdown())
            .await
            .expect("cached command must not retain the retired generation")
            .is_empty()
    );
    assert!(matches!(
        runner
            .invoke_resolved_command_typed(&command, "", &command_ctx())
            .await,
        Err(ExtensionError::NotFound(message))
            if message.contains("generation is no longer available")
    ));
}

#[tokio::test]
async fn session_control_tools_declare_no_resource_conflicts() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(SmallModelProbeExtension {
            small_model_allowed: false,
            session_control_allowed: true,
        }))
        .await
        .unwrap();
    let session_control_tool = runner
        .tool_catalog_snapshot_typed("D:/workspace")
        .await
        .tools
        .into_iter()
        .next()
        .unwrap();
    assert!(
        session_control_tool
            .resource_accesses(&json!({}), Path::new("D:/workspace"))
            .unwrap()
            .is_empty()
    );

    runner
        .register(Arc::new(StateProbeExtension))
        .await
        .unwrap();
    let default_tool = runner
        .tool_catalog_snapshot_typed("D:/workspace")
        .await
        .tools
        .into_iter()
        .find(|tool| tool.definition().name == "stateProbe")
        .unwrap();
    assert_eq!(
        default_tool
            .resource_accesses(&json!({}), Path::new("D:/workspace"))
            .unwrap(),
        vec![ResourceAccess::all()]
    );
}

#[tokio::test]
async fn public_http_route_dispatches_with_path_params() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(HttpProbeExtension {
            id: "public-http",
            capabilities: vec![ExtensionCapability::PublicHttp],
            route: ExtensionHttpRoute::public(ExtensionHttpMethod::Post, "/future-tasks/{jobId}"),
        }))
        .await
        .expect("register public route");

    let result = runner
        .dispatch_public_http_route(
            ExtensionHttpRequest {
                method: ExtensionHttpMethod::Post,
                path: "/future-tasks/job-1".into(),
                path_params: Default::default(),
                query: Some("run=true".into()),
                body: serde_json::Value::Null,
            },
            br#"{"name":"probe"}"#,
        )
        .await
        .expect("dispatch route");

    let ExtensionHttpDispatchResult::Response(response) = result else {
        panic!("expected response");
    };
    assert_eq!(response.status, 201);
    assert_eq!(response.body["extensionId"], "public-http");
    assert_eq!(response.body["routePath"], "/future-tasks/{jobId}");
    assert_eq!(response.body["requestPath"], "/future-tasks/job-1");
    assert_eq!(response.body["pathParams"]["jobId"], "job-1");
    assert_eq!(response.body["query"], "run=true");
    assert_eq!(response.body["bodyName"], "probe");

    let error = runner
        .dispatch_public_http_route(
            ExtensionHttpRequest::new(ExtensionHttpMethod::Post, "/future-tasks/job-2"),
            br#"{"unexpected":true}"#,
        )
        .await
        .expect_err("typed body validation should return a structured input error");
    assert!(matches!(
        error,
        ExtensionError::InvalidInput { code, hint: Some(_), .. }
            if code == "invalid_http_body"
    ));

    let error = runner
        .dispatch_public_http_route(
            ExtensionHttpRequest::new(ExtensionHttpMethod::Post, "/future-tasks/job-3")
                .query("invalid-status"),
            &[],
        )
        .await
        .expect_err("in-process handlers must not bypass response status validation");
    assert!(matches!(
        error,
        ExtensionError::Internal(message)
            if message.contains("invalid HTTP status 99")
    ));
}

#[tokio::test]
async fn authenticated_http_routes_are_capability_checked_and_extension_scoped() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    let missing_capability = runner
        .register(Arc::new(HttpProbeExtension {
            id: "missing-authenticated-http",
            capabilities: Vec::new(),
            route: ExtensionHttpRoute::authenticated(ExtensionHttpMethod::Get, "/status"),
        }))
        .await
        .expect_err("authenticated route must declare its capability");
    assert!(matches!(
        missing_capability,
        ExtensionError::MissingCapability {
            capability: ExtensionCapability::AuthenticatedHttp,
            ..
        }
    ));

    for id in ["authenticated-one", "authenticated-two"] {
        runner
            .register(Arc::new(HttpProbeExtension {
                id,
                capabilities: vec![ExtensionCapability::AuthenticatedHttp],
                route: ExtensionHttpRoute::authenticated(ExtensionHttpMethod::Get, "/items/{id}"),
            }))
            .await
            .expect("same authenticated path is isolated by extension id");
    }

    let request = ExtensionHttpRequest::new(ExtensionHttpMethod::Get, "/items/item-1");
    let result = runner
        .dispatch_authenticated_http_route("authenticated-two", request.clone(), &[])
        .await
        .expect("dispatch authenticated route");
    let ExtensionHttpDispatchResult::Response(response) = result else {
        panic!("expected authenticated response");
    };
    assert_eq!(response.body["pathParams"]["id"], "item-1");

    assert!(matches!(
        runner
            .dispatch_public_http_route(request, &[])
            .await
            .expect("public lookup"),
        ExtensionHttpDispatchResult::NotFound
    ));
}

#[tokio::test]
async fn http_route_registration_requires_public_http_capability() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    let error = runner
        .register(Arc::new(HttpProbeExtension {
            id: "missing-http-capability",
            capabilities: Vec::new(),
            route: ExtensionHttpRoute::public(ExtensionHttpMethod::Get, "/status"),
        }))
        .await
        .expect_err("route without public_http must fail");

    assert!(matches!(
        error,
        ExtensionError::MissingCapability {
            capability: ExtensionCapability::PublicHttp,
            ..
        }
    ));
    assert_eq!(runner.count().await, 0);
}

#[tokio::test]
async fn conflicting_public_routes_are_rejected() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(HttpProbeExtension {
            id: "public-one",
            capabilities: vec![ExtensionCapability::PublicHttp],
            route: ExtensionHttpRoute::public(ExtensionHttpMethod::Get, "/items/{id}"),
        }))
        .await
        .expect("register first route");

    let error = runner
        .register(Arc::new(HttpProbeExtension {
            id: "public-two",
            capabilities: vec![ExtensionCapability::PublicHttp],
            route: ExtensionHttpRoute::public(ExtensionHttpMethod::Get, "/items/{name}"),
        }))
        .await
        .expect_err("overlapping public route must fail");

    assert!(error.to_string().contains("conflicts"));
}

fn command_ctx() -> CommandRuntimeContext {
    CommandRuntimeContext::new(
        "session",
        ".",
        ModelSelection::simple("mock"),
        Some(PathBuf::from("/tmp/session-store")),
    )
}
