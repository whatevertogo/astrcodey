use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use astrcode_core::{
    config::ModelSelection,
    event::{
        CustomEventData, DurableEvent, DurableEventPayload, Event, EventSender, LiveEvent,
        LiveEventPayload, PersistedSystemPrompt, SessionStarted, SystemPromptSource,
    },
    tool::{
        SessionToolSelection, ToolCapabilities, ToolExecutionContext, ToolPlanningContext,
        access::{HostResource, ResourceLease},
    },
    types::SessionId,
};
use astrcode_extension_sdk::{
    WireErrorCode,
    builder::{ExtensionToolDefinition, command, custom_event, manifest},
    extension::{
        CommandAvailability, CommandCompletionContext, CommandCompletionItem, CommandCompletions,
        CommandContext, CommandExecution, CommandHandler, ContinueAfterStopContext,
        ContinueAfterStopHandler, ContinueAfterStopOptions, ContinueAfterStopResult,
        CustomEventContext, CustomEventDelivery, CustomEventDisposition, CustomEventHandler,
        CustomEventSubscription, Extension, ExtensionCall, ExtensionCapability,
        ExtensionCommandResult, ExtensionError, ExtensionHttpDispatchRequest, ExtensionHttpHandler,
        ExtensionHttpMethod, ExtensionHttpRequest, ExtensionHttpResponse, ExtensionHttpRoute,
        ExtensionManifest, ExtensionStartContext, ExtensionStopContext, ExtensionTasks, HookMode,
        HookResult, HttpContext, LifecycleContext, LifecycleEvent, LifecycleHandler,
        PostToolUseContext, PostToolUseHandler, PostToolUseResult, PreCompactContext,
        PreCompactHandler, PreCompactResult, PreToolUseAdmission, PreToolUseContext,
        PreToolUseHandler, PreToolUseResult, ProviderContext, ProviderEvent, ProviderHandler,
        ProviderRequestId, ProviderResult, Registrar, SessionCommandIntent, SessionCommandKind,
        SlashCommand, StopReason, ToolContext, ToolDiscovery, ToolDiscoveryContext,
        ToolDiscoveryHandler, ToolHandler, ToolHookTarget, ToolInputTransformHandler,
        ToolInputTransformResult, ToolPlanContext, UserMessageEnvelopeContext,
        UserMessageEnvelopeHandler, UserMessageEnvelopeResult,
        internal::{
            RuntimeContinueAfterStopContext, RuntimeHookCallContext, RuntimePreToolUseContext,
            RuntimeUserMessageEnvelopeContext, runtime_continue_after_stop_context,
            runtime_pre_tool_use_context, runtime_provider_context,
            runtime_user_message_envelope_context, wait_extension_tasks,
        },
    },
    host::{
        ExtensionHost, HostProcessHandleOutput, HostProcessListOutput, HostProcessStartRequest,
        HostSessionStateReadRequest, HostSessionStateWriteRequest,
    },
    runtime_ports::{ToolCatalogCompleteness, ToolCatalogProvider},
    tool::{ToolDefinition, ToolExecutionPolicy, ToolOrigin, ToolPlan, ToolResult},
};
use astrcode_storage::{
    SessionEventJournal, SessionPathResolver, SessionStore, testing::filesystem_session_repository,
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Notify, mpsc};

use super::{
    CustomEventConsumerAction, CustomEventSession, ExtensionHttpDispatchResult, ExtensionRunner,
    ExtensionRuntimeState, SourceGenerationEntry,
};
use crate::host_router::{HostBackends, build_host_router_with_public_http_dispatcher};

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

struct StartupDirectoryExtension {
    received: Arc<Mutex<Option<StartupContextSnapshot>>>,
}

#[derive(Debug, PartialEq, Eq)]
struct StartupContextSnapshot {
    extension_id: String,
    startup_working_dir: Option<PathBuf>,
    global_data_dir: Option<PathBuf>,
    session_context_available: bool,
    config_version: Option<u64>,
    cancelled: bool,
}

struct StartupEventExtension;

struct UnhealthyExtension;

struct StateProbeExtension;

struct StateProbeTool;

struct CallScopeProbeExtension {
    retained: Arc<Mutex<Option<ToolContext>>>,
}

struct CallScopeProbeTool {
    retained: Arc<Mutex<Option<ToolContext>>>,
}

#[derive(Deserialize)]
struct CallScopeProbeArguments {
    fail: bool,
}

struct ToolRetirementProbeExtension {
    stopped: Arc<AtomicUsize>,
}

struct RetirementFailureExtension {
    fail_stop: bool,
}

struct RetirementCleanupExtension {
    stop_entered: Arc<Notify>,
    stop_release: Arc<Notify>,
    stopped: Arc<AtomicBool>,
}

struct SlowToolDiscoveryExtension;

struct SlowToolDiscovery;

struct HttpProbeExtension {
    id: &'static str,
    capabilities: Vec<ExtensionCapability>,
    route: ExtensionHttpRoute,
}

struct HttpProbeHandler;

struct GenerationHttpCaller {
    startup_host: Arc<Mutex<Option<ExtensionHost>>>,
}

struct GenerationHttpCallerHandler {
    startup_host: Arc<Mutex<Option<ExtensionHost>>>,
}

struct GenerationHttpTarget {
    label: &'static str,
}

struct GenerationHttpTargetHandler {
    label: &'static str,
}

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

#[async_trait::async_trait]
impl Extension for GenerationHttpCaller {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest(
            "generation-http-caller",
            &[
                ExtensionCapability::ToolIntercept,
                ExtensionCapability::PublicHttpDispatch,
            ],
        )
    }

    fn register(&self, registrar: &mut Registrar) {
        registrar.on_pre_tool_use(
            0,
            Arc::new(GenerationHttpCallerHandler {
                startup_host: Arc::clone(&self.startup_host),
            }),
        );
    }

    async fn start(&self, ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        *self.startup_host.lock().unwrap() = Some(ctx.host().clone());
        Ok(())
    }
}

#[async_trait::async_trait]
impl PreToolUseHandler for GenerationHttpCallerHandler {
    async fn handle(&self, ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError> {
        let startup_host = self
            .startup_host
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| ExtensionError::Internal("startup host was not captured".into()))?;
        let handler_label = generation_http_label(ctx.host()).await?;
        let startup_label = generation_http_label(&startup_host).await?;
        Ok(PreToolUseResult::Ask {
            prompt: format!("{handler_label}/{startup_label}"),
            rule_key: None,
        })
    }
}

async fn generation_http_label(host: &ExtensionHost) -> Result<String, ExtensionError> {
    let response = host
        .extension_http()?
        .dispatch_public(ExtensionHttpDispatchRequest::new(
            ExtensionHttpMethod::Get,
            "/generation",
        ))
        .await?;
    response.body["generation"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| ExtensionError::Internal("missing generation label".into()))
}

#[async_trait::async_trait]
impl Extension for GenerationHttpTarget {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("generation-http-target", &[ExtensionCapability::PublicHttp])
    }

    fn register(&self, registrar: &mut Registrar) {
        registrar.http_route(
            ExtensionHttpRoute::public(ExtensionHttpMethod::Get, "/generation"),
            Arc::new(GenerationHttpTargetHandler { label: self.label }),
        );
    }
}

#[async_trait::async_trait]
impl ExtensionHttpHandler for GenerationHttpTargetHandler {
    async fn handle(&self, _ctx: HttpContext) -> Result<ExtensionHttpResponse, ExtensionError> {
        Ok(ExtensionHttpResponse::json(
            200,
            json!({ "generation": self.label }),
        ))
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

enum PreToolProbeBehavior {
    Transform {
        field: &'static str,
        value: serde_json::Value,
    },
    Admission(PreToolUseResult),
}

struct PreToolPhaseProbeExtension {
    id: &'static str,
    label: &'static str,
    priority: i32,
    behavior: PreToolProbeBehavior,
    observed: Arc<Mutex<Vec<(&'static str, serde_json::Value)>>>,
}

struct TransformProbeHandler {
    label: &'static str,
    field: &'static str,
    value: serde_json::Value,
    observed: Arc<Mutex<Vec<(&'static str, serde_json::Value)>>>,
}

struct AdmissionProbeHandler {
    label: &'static str,
    decision: PreToolUseResult,
    observed: Arc<Mutex<Vec<(&'static str, serde_json::Value)>>>,
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

struct PanickingSourceCandidate {
    stop_entered: Arc<Notify>,
    stop_release: Arc<Notify>,
}

struct ReplacementSourceCandidate {
    started: Arc<AtomicBool>,
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
    SessionCommand,
}

struct RegistrationProbeExtension {
    capabilities: Vec<ExtensionCapability>,
    registration: CapabilityRegistration,
}

struct LifecycleModeProbeExtension {
    event: LifecycleEvent,
}

struct RegistrationProbeHandler;

struct CommandProbe {
    label: &'static str,
    argument_completions: bool,
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
            },
            Arc::new(StateProbeTool),
        );
    }
}

#[async_trait::async_trait]
impl ToolHandler for StateProbeTool {
    async fn plan(&self, _ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        Ok(ToolPlan::host(HostResource::Session))
    }

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
impl Extension for CallScopeProbeExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("call-scope-probe", &[ExtensionCapability::EmitCustomEvents])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.declare_custom_event(
            custom_event("call_scope_probe")
                .delivery(CustomEventDelivery::SessionLive)
                .build(),
        );
        reg.tool(
            ToolDefinition {
                name: "callScopeProbe".into(),
                description: String::new(),
                parameters: json!({"type": "object"}),
                strict: false,
                origin: ToolOrigin::Extension,
            },
            Arc::new(CallScopeProbeTool {
                retained: Arc::clone(&self.retained),
            }),
        );
    }
}

#[async_trait::async_trait]
impl ToolHandler for CallScopeProbeTool {
    async fn plan(&self, _ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        Ok(ToolPlan::host(HostResource::Session))
    }

    async fn execute(
        &self,
        ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        let arguments = ctx.arguments::<CallScopeProbeArguments>()?;
        *self.retained.lock().unwrap() = Some(ctx);
        if arguments.fail {
            Err(ExtensionError::Internal("injected handler failure".into()))
        } else {
            Ok(ToolResult::text("ok".into(), false, Default::default()).into())
        }
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
            },
            Arc::new(StateProbeTool),
        );
    }

    async fn stop(&self, _ctx: ExtensionStopContext) -> Result<(), ExtensionError> {
        self.stopped.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait::async_trait]
impl Extension for RetirementFailureExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("retirement-failure-probe", &[])
    }

    async fn stop(&self, _ctx: ExtensionStopContext) -> Result<(), ExtensionError> {
        if self.fail_stop {
            Err(ExtensionError::Internal("intentional stop failure".into()))
        } else {
            Ok(())
        }
    }
}

#[async_trait::async_trait]
impl Extension for RetirementCleanupExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest(
            "retirement-cleanup-probe",
            &[ExtensionCapability::ProcessSpawn],
        )
    }

    async fn stop(&self, ctx: ExtensionStopContext) -> Result<(), ExtensionError> {
        assert_eq!(ctx.reason(), StopReason::Disabled);
        self.stop_entered.notify_one();
        self.stop_release.notified().await;
        self.stopped.store(true, Ordering::Release);
        Ok(())
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
                availability: CommandAvailability::AllTransports,
                execution: CommandExecution::Extension,
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
                    ctx.session_id(),
                    ctx.command_name(),
                    ctx.working_dir().display(),
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
            },
            Arc::new(SmallModelProbeTool),
        );
    }
}

#[async_trait::async_trait]
impl ToolHandler for SmallModelProbeTool {
    async fn plan(&self, _ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        Ok(ToolPlan::host(HostResource::Model))
    }

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
        Ok(PreToolUseResult::Ask {
            prompt: "approve target".into(),
            rule_key: Some("dangerous".into()),
        })
    }
}

#[async_trait::async_trait]
impl Extension for PreToolPhaseProbeExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest(self.id, &[ExtensionCapability::ToolIntercept])
    }

    fn register(&self, reg: &mut Registrar) {
        match &self.behavior {
            PreToolProbeBehavior::Transform { field, value } => reg.on_tool_input_transform(
                self.priority,
                Arc::new(TransformProbeHandler {
                    label: self.label,
                    field,
                    value: value.clone(),
                    observed: Arc::clone(&self.observed),
                }),
            ),
            PreToolProbeBehavior::Admission(decision) => reg.on_pre_tool_use(
                self.priority,
                Arc::new(AdmissionProbeHandler {
                    label: self.label,
                    decision: decision.clone(),
                    observed: Arc::clone(&self.observed),
                }),
            ),
        }
    }
}

#[async_trait::async_trait]
impl ToolInputTransformHandler for TransformProbeHandler {
    async fn transform(
        &self,
        ctx: PreToolUseContext,
    ) -> Result<ToolInputTransformResult, ExtensionError> {
        self.observed
            .lock()
            .unwrap()
            .push((self.label, ctx.tool_input().clone()));
        let mut transformed = ctx.tool_input().clone();
        transformed
            .as_object_mut()
            .unwrap()
            .insert(self.field.into(), self.value.clone());
        Ok(ToolInputTransformResult::Replace {
            tool_input: transformed,
        })
    }
}

#[async_trait::async_trait]
impl PreToolUseHandler for AdmissionProbeHandler {
    async fn handle(&self, ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError> {
        self.observed
            .lock()
            .unwrap()
            .push((self.label, ctx.tool_input().clone()));
        Ok(self.decision.clone())
    }
}

#[async_trait::async_trait]
impl Extension for GenerationProbeExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("generation-probe", &[ExtensionCapability::ToolIntercept])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_tool_input_transform(
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

    async fn stop(&self, _ctx: ExtensionStopContext) -> Result<(), ExtensionError> {
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

    async fn stop(&self, ctx: ExtensionStopContext) -> Result<(), ExtensionError> {
        *self.stop_reason.lock().unwrap() = Some(ctx.reason());
        self.stop_entered.notify_one();
        self.stop_release.notified().await;
        self.lifecycle.lock().unwrap().push("v1_stop");
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait::async_trait]
impl Extension for PanickingSourceCandidate {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("panicking-source-candidate", &[])
    }

    async fn start(&self, _ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        panic!("injected source candidate start panic");
    }

    async fn stop(&self, ctx: ExtensionStopContext) -> Result<(), ExtensionError> {
        assert_eq!(ctx.reason(), StopReason::StartupFailed);
        self.stop_entered.notify_one();
        self.stop_release.notified().await;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Extension for ReplacementSourceCandidate {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("panicking-source-candidate", &[])
    }

    async fn start(&self, _ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        self.started.store(true, Ordering::Release);
        Ok(())
    }
}

#[async_trait::async_trait]
impl ToolInputTransformHandler for GenerationProbeHook {
    async fn transform(
        &self,
        ctx: PreToolUseContext,
    ) -> Result<ToolInputTransformResult, ExtensionError> {
        if let Some(call_started) = &self.call_started {
            call_started.notify_one();
            ctx.cancellation().cancelled().await;
        }
        Ok(ToolInputTransformResult::Replace {
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

    async fn stop(&self, ctx: ExtensionStopContext) -> Result<(), ExtensionError> {
        *self.stop_reason.lock().unwrap() = Some(ctx.reason());
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

    async fn stop(&self, _ctx: ExtensionStopContext) -> Result<(), ExtensionError> {
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
                reg.declare_custom_event(custom_event("probe").build());
            },
            CapabilityRegistration::Compact => {
                reg.on_pre_compact(0, Arc::new(RegistrationProbeHandler));
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
                reg.on_pre_tool_use(0, Arc::new(RegistrationProbeHandler));
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
            CapabilityRegistration::SessionCommand => {
                reg.command(
                    command("compact-probe")
                        .host_command(SessionCommandKind::CompactSession)
                        .build(),
                    Arc::new(RegistrationProbeHandler),
                );
            },
        }
    }
}

#[async_trait::async_trait]
impl PreCompactHandler for RegistrationProbeHandler {
    async fn handle(&self, _ctx: PreCompactContext) -> Result<PreCompactResult, ExtensionError> {
        Ok(PreCompactResult::Allow)
    }
}

#[async_trait::async_trait]
impl CommandHandler for RegistrationProbeHandler {
    async fn execute(
        &self,
        _ctx: CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        Ok(ExtensionCommandResult::host_command(
            SessionCommandIntent::CompactSession {
                keep_recent_turns: None,
            },
        ))
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
    runtime_continue_after_stop_context(
        runtime_hook_call(),
        "done",
        "stop",
        continuations_this_turn,
    )
}

fn user_message_envelope_ctx(text: &str) -> RuntimeUserMessageEnvelopeContext {
    runtime_user_message_envelope_context(runtime_hook_call(), text, Vec::new())
}

fn pre_tool_use_ctx(tool_name: &str, tool_input: serde_json::Value) -> RuntimePreToolUseContext {
    runtime_pre_tool_use_context(
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
            session_context_available: ctx.paths().session_data_dir().is_ok(),
            config_version: config["version"].as_u64(),
            cancelled: ctx.cancellation().is_cancelled(),
        });
        Ok(())
    }
}

#[async_trait::async_trait]
impl Extension for StartupEventExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("startup-event", &[ExtensionCapability::EmitCustomEvents])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.declare_custom_event(custom_event("startup_ready").build());
    }

    async fn start(&self, ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        ctx.events()
            .emit("startup_ready", &json!({"ready": true}))
            .await
            .map(|_| ())
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

    async fn stop(&self, ctx: ExtensionStopContext) -> Result<(), ExtensionError> {
        assert_eq!(ctx.reason(), self.expected_reason);
        assert!(self.task_stopped.load(Ordering::SeqCst));
        self.stopped.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn privileged_registrations_require_their_declared_capabilities() {
    let cases = [
        (
            CapabilityRegistration::Event,
            "event",
            ExtensionCapability::EmitCustomEvents,
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
        (
            CapabilityRegistration::SessionCommand,
            "command",
            ExtensionCapability::SessionCommand,
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
    for event in [LifecycleEvent::TurnStart, LifecycleEvent::UserPromptSubmit] {
        ExtensionRunner::new(Duration::from_secs(1))
            .register(Arc::new(LifecycleModeProbeExtension { event }))
            .await
            .expect("turn-entry lifecycle event may block");
    }

    for event in [
        LifecycleEvent::SessionStart,
        LifecycleEvent::SessionResume,
        LifecycleEvent::TurnEnd,
        LifecycleEvent::StepEnd,
        LifecycleEvent::SessionShutdown,
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
            .transform_tool_input(pre_tool_use_ctx("probe", json!({})))
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
    assert_eq!(version_one_result, json!({ "generation": "v1" }));
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
    assert_eq!(turn_view.revision(), version_two_view.generation());
    let version_two_result = version_two_view
        .transform_tool_input(context())
        .await
        .unwrap();
    assert_eq!(version_two_result, json!({ "generation": "v2" }));

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
    assert!(wait_extension_tasks(&tasks, Duration::from_millis(10)).await);
    assert!(matches!(
        tasks.run_to_completion("late-write", async {}).await,
        Err(astrcode_extension_sdk::extension::ExtensionTaskError::ShuttingDown { .. })
    ));

    assert!(runner.shutdown().await.is_empty());
    assert_eq!(version_one_stops.load(Ordering::SeqCst), 1);
    assert_eq!(version_two_stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn panicking_source_start_finishes_rollback_before_the_next_reconcile() {
    let stop_entered = Arc::new(Notify::new());
    let stop_release = Arc::new(Notify::new());
    let replacement_waiting = Arc::new(Notify::new());
    let replacement_started = Arc::new(AtomicBool::new(false));
    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));

    let failed_candidate = {
        let runner = Arc::clone(&runner);
        let stop_entered = Arc::clone(&stop_entered);
        let stop_release = Arc::clone(&stop_release);
        tokio::spawn(async move {
            let source_transaction = runner.begin_source_transaction().await;
            runner
                .prepare_source_generation(
                    source_transaction,
                    vec![SourceGenerationEntry::Start {
                        extension: Arc::new(PanickingSourceCandidate {
                            stop_entered,
                            stop_release,
                        }),
                        key: "panicking-source-candidate".into(),
                        fingerprint: "panicking-v1".into(),
                        config: json!({}),
                    }],
                    None,
                )
                .await
                .map(|_| ())
        })
    };
    tokio::time::timeout(Duration::from_secs(1), stop_entered.notified())
        .await
        .expect("panicking candidate must enter StartupFailed cleanup");

    let mut replacement = {
        let runner = Arc::clone(&runner);
        let replacement_waiting = Arc::clone(&replacement_waiting);
        let replacement_started = Arc::clone(&replacement_started);
        tokio::spawn(async move {
            replacement_waiting.notify_one();
            let source_transaction = runner.begin_source_transaction().await;
            let candidate = runner
                .prepare_source_generation(
                    source_transaction,
                    vec![SourceGenerationEntry::Start {
                        extension: Arc::new(ReplacementSourceCandidate {
                            started: replacement_started,
                        }),
                        key: "panicking-source-candidate".into(),
                        fingerprint: "replacement-v2".into(),
                        config: json!({}),
                    }],
                    None,
                )
                .await?;
            candidate.commit_with(|_| {}).await;
            Ok::<_, ExtensionError>(())
        })
    };
    tokio::time::timeout(Duration::from_secs(1), replacement_waiting.notified())
        .await
        .expect("replacement must attempt to acquire the source transaction");
    assert!(!replacement_started.load(Ordering::Acquire));

    stop_release.notify_one();
    let error = tokio::time::timeout(Duration::from_secs(1), failed_candidate)
        .await
        .expect("panic rollback should finish")
        .expect("candidate task should return a typed error")
        .expect_err("panicking candidate must fail");
    assert!(error.to_string().contains("start panicked"));
    tokio::time::timeout(Duration::from_secs(1), &mut replacement)
        .await
        .expect("replacement should resume after rollback")
        .expect("replacement task should not panic")
        .expect("replacement should publish");
    assert!(replacement_started.load(Ordering::Acquire));

    assert!(runner.shutdown().await.is_empty());
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

#[cfg(unix)]
#[tokio::test]
async fn retirement_cleans_only_its_instance_resources_after_stop() {
    let workspace = tempfile::tempdir().unwrap();
    let stop_entered = Arc::new(Notify::new());
    let stop_release = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicBool::new(false));
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(RetirementCleanupExtension {
            stop_entered: Arc::clone(&stop_entered),
            stop_release: Arc::clone(&stop_release),
            stopped: Arc::clone(&stopped),
        }))
        .await
        .unwrap();
    let (instance_id, generation_gate) = {
        let extensions = runner.registry.extensions.read().await;
        (
            extensions[0].instance_id,
            extensions[0].generation_gate.clone(),
        )
    };
    let router = runner.host_router();
    let invoke_context = crate::host_router::InvokeContext {
        extension_id: "retirement-cleanup-probe".into(),
        extension_instance_id: instance_id,
        session_id: Some("retirement-cleanup-session".into()),
        working_dir: Some(workspace.path().to_string_lossy().into_owned()),
        declared_capabilities: vec![ExtensionCapability::ProcessSpawn],
        generation_gate,
        ..Default::default()
    };
    let mut inspection_context = invoke_context.clone();
    inspection_context.generation_gate = Default::default();
    let mut request = HostProcessStartRequest::new("/bin/sh");
    request.args = vec!["-c".into(), "sleep 30".into()];
    let started = router
        .invoke(
            "astrcode.process.start",
            serde_json::to_value(request).unwrap(),
            &invoke_context,
        )
        .await
        .unwrap();
    let started: HostProcessHandleOutput = serde_json::from_value(started).unwrap();
    let old_view = runner.turn_extension_view();

    assert!(
        runner
            .unregister("retirement-cleanup-probe", StopReason::Disabled)
            .await
            .unwrap()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(25), stop_entered.notified())
            .await
            .is_err(),
        "retirement must keep the old instance active while its index is pinned"
    );
    drop(old_view);
    tokio::time::timeout(Duration::from_secs(1), stop_entered.notified())
        .await
        .unwrap();
    let revoked = router
        .invoke("astrcode.process.list", json!({}), &invoke_context)
        .await
        .unwrap_err();
    assert_eq!(revoked.code_enum(), Some(WireErrorCode::HostNotReady));
    let listed = router
        .invoke("astrcode.process.list", json!({}), &inspection_context)
        .await
        .unwrap();
    let listed: HostProcessListOutput = serde_json::from_value(listed).unwrap();
    assert_eq!(listed.processes.len(), 1);
    assert_eq!(listed.processes[0].id, started.id);
    assert!(!stopped.load(Ordering::Acquire));

    stop_release.notify_one();
    assert!(runner.shutdown().await.is_empty());
    assert!(stopped.load(Ordering::Acquire));
    let listed = router
        .invoke("astrcode.process.list", json!({}), &inspection_context)
        .await
        .unwrap();
    let listed: HostProcessListOutput = serde_json::from_value(listed).unwrap();
    assert!(listed.processes.is_empty());
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
            config_version: None,
            cancelled: false,
        })
    );
}

#[tokio::test]
async fn candidate_start_cannot_emit_host_events_before_publication() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    runner.bind_startup_event_channel(event_tx);

    let error = runner
        .register(Arc::new(StartupEventExtension))
        .await
        .expect_err("candidate startup must not publish Host side effects");
    assert!(error.to_string().contains("generation is not active"));
    assert!(event_rx.try_recv().is_err());
    assert!(runner.registered_extension_ids().await.is_empty());
    assert!(runner.shutdown().await.is_empty());
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
    )
    .with_resource_lease(ResourceLease::from_plan(&ToolPlan::host(
        HostResource::Session,
    )));

    let result = tool.execute(json!({}), &ctx).await.unwrap();
    assert_eq!(result.content, "true");
}

struct TimeoutProbeExtension;

struct TimeoutProbeTool;

#[async_trait::async_trait]
impl Extension for TimeoutProbeExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("timeout-probe", &[])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.tool(
            ExtensionToolDefinition::from_definition(ToolDefinition {
                name: "timeoutProbe".into(),
                description: String::new(),
                parameters: json!({"type": "object"}),
                strict: false,
                origin: ToolOrigin::Extension,
            })
            .with_execution_policy(ToolExecutionPolicy {
                timeout: Some(Duration::from_millis(50)),
                ..ToolExecutionPolicy::SEQUENTIAL
            }),
            Arc::new(TimeoutProbeTool),
        );
    }
}

#[async_trait::async_trait]
impl ToolHandler for TimeoutProbeTool {
    async fn plan(&self, _ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        Ok(ToolPlan::host(HostResource::Session))
    }

    async fn execute(
        &self,
        _ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        std::future::pending::<
            Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError>,
        >()
        .await
    }
}

#[tokio::test]
async fn extension_execution_policy_times_out_in_process_execution() {
    let runner = ExtensionRunner::new(Duration::from_secs(30));
    runner
        .register(Arc::new(TimeoutProbeExtension))
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
        ToolCapabilities::default(),
    )
    .with_resource_lease(ResourceLease::from_plan(&ToolPlan::host(
        HostResource::Session,
    )));

    let result = tool.execute(json!({}), &ctx).await.unwrap();
    assert!(result.is_error);
    assert!(
        result.content.contains("timed out after 50ms"),
        "{}",
        result.content
    );
    assert_eq!(result.metadata.get("timeoutMs"), Some(&json!(50)));
}

#[tokio::test]
async fn extension_tool_call_context_expires_after_handler_completion() {
    let session_store = tempfile::tempdir().unwrap();
    let events_sent = Arc::new(AtomicUsize::new(0));
    let retained = Arc::new(Mutex::new(None));
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(CallScopeProbeExtension {
            retained: Arc::clone(&retained),
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
    let event_tx = EventSender::new({
        let events_sent = Arc::clone(&events_sent);
        move |_| {
            events_sent.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    });
    let ctx = ToolExecutionContext::new(
        "session".into(),
        "D:/workspace",
        None,
        Some(event_tx),
        ToolCapabilities {
            paths: astrcode_core::tool::ToolSessionPaths {
                store_dir: Some(session_store.path().to_path_buf()),
            },
            ..Default::default()
        },
    )
    .with_resource_lease(ResourceLease::from_plan(&ToolPlan::host(
        HostResource::Session,
    )));

    for fail in [false, true] {
        tool.execute(json!({"fail": fail}), &ctx).await.unwrap();
        let retained_context = retained.lock().unwrap().take().unwrap();
        assert!(
            retained_context.cancellation().is_cancelled(),
            "call context must expire after handler completion (fail={fail})"
        );
        let host_error = retained_context
            .host()
            .session_state()
            .unwrap()
            .read(HostSessionStateReadRequest {
                key: "retained".into(),
            })
            .await
            .unwrap_err();
        assert_eq!(host_error.code_enum(), Some(WireErrorCode::Cancelled));
        let event_error = retained_context
            .events()
            .emit("call_scope_probe", &json!({"fail": fail}))
            .await
            .unwrap_err();
        assert!(event_error.to_string().contains("no longer active"));
    }
    assert_eq!(events_sent.load(Ordering::SeqCst), 0);
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
        )
        .with_resource_lease(ResourceLease::from_plan(&ToolPlan::host(
            HostResource::Model,
        )));

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

    let result = runner
        .emit_pre_tool_use(base_ctx("targetTool"))
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        result,
        PreToolUseAdmission::Ask { requirements }
            if requirements[0].rule_key.as_deref()
                == Some("extension:targeted-pre-hook:dangerous")
    ));

    let diagnostics = runner.diagnostics_snapshot();
    let hook_diagnostics = diagnostics.get("targeted-pre-hook").unwrap();
    assert_eq!(hook_diagnostics.hook_calls, 1);
    assert_eq!(hook_diagnostics.last_hook.as_deref(), Some("pre_tool_use"));
}

#[tokio::test]
async fn pre_tool_use_transforms_then_composes_admission_on_final_input() {
    let observed = Arc::new(Mutex::new(Vec::new()));
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    let extensions = [
        PreToolPhaseProbeExtension {
            id: "transform-first",
            label: "transform-first",
            priority: 100,
            behavior: PreToolProbeBehavior::Transform {
                field: "first",
                value: json!(true),
            },
            observed: Arc::clone(&observed),
        },
        PreToolPhaseProbeExtension {
            id: "transform-second",
            label: "transform-second",
            priority: 0,
            behavior: PreToolProbeBehavior::Transform {
                field: "second",
                value: json!(2),
            },
            observed: Arc::clone(&observed),
        },
        PreToolPhaseProbeExtension {
            id: "ask-first",
            label: "ask-first",
            priority: 100,
            behavior: PreToolProbeBehavior::Admission(PreToolUseResult::Ask {
                prompt: "first approval".into(),
                rule_key: Some("first".into()),
            }),
            observed: Arc::clone(&observed),
        },
        PreToolPhaseProbeExtension {
            id: "ask-second",
            label: "ask-second",
            priority: 50,
            behavior: PreToolProbeBehavior::Admission(PreToolUseResult::Ask {
                prompt: "second approval".into(),
                rule_key: Some("second".into()),
            }),
            observed: Arc::clone(&observed),
        },
        PreToolPhaseProbeExtension {
            id: "block-last",
            label: "block-last",
            priority: 0,
            behavior: PreToolProbeBehavior::Admission(PreToolUseResult::Block {
                reason: "blocked after asks".into(),
            }),
            observed: Arc::clone(&observed),
        },
    ];
    for extension in extensions {
        runner.register(Arc::new(extension)).await.unwrap();
    }

    let final_input = runner
        .transform_tool_input(pre_tool_use_ctx("probe", json!({"raw": true})))
        .await
        .unwrap();
    assert_eq!(
        final_input,
        json!({"raw": true, "first": true, "second": 2})
    );
    assert_eq!(
        observed.lock().unwrap().as_slice(),
        &[
            ("transform-first", json!({"raw": true})),
            ("transform-second", json!({"raw": true, "first": true})),
        ]
    );

    observed.lock().unwrap().clear();
    let blocked = runner
        .emit_pre_tool_use(pre_tool_use_ctx("probe", final_input.clone()))
        .await
        .unwrap();
    assert_eq!(
        blocked,
        PreToolUseAdmission::Block {
            reason: "blocked after asks".into()
        }
    );
    assert_eq!(
        observed.lock().unwrap().as_slice(),
        &[
            ("ask-first", final_input.clone()),
            ("ask-second", final_input.clone()),
            ("block-last", final_input.clone()),
        ]
    );

    assert!(
        runner
            .unregister("block-last", StopReason::Disabled)
            .await
            .unwrap()
    );
    observed.lock().unwrap().clear();
    let admission = runner
        .emit_pre_tool_use(pre_tool_use_ctx("probe", final_input.clone()))
        .await
        .unwrap();
    assert_eq!(
        admission,
        PreToolUseAdmission::Ask {
            requirements: vec![
                astrcode_extension_sdk::extension::PreToolUseRequirement {
                    prompt: "first approval".into(),
                    rule_key: Some("extension:ask-first:first".into()),
                },
                astrcode_extension_sdk::extension::PreToolUseRequirement {
                    prompt: "second approval".into(),
                    rule_key: Some("extension:ask-second:second".into()),
                },
            ]
        }
    );
    assert_eq!(
        observed.lock().unwrap().as_slice(),
        &[
            ("ask-first", final_input.clone()),
            ("ask-second", final_input),
        ]
    );
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
async fn recorded_hook_tracks_error_and_timeout() {
    let runner = ExtensionRunner::new(Duration::from_millis(10));
    runner
        .register(Arc::new(StateProbeExtension))
        .await
        .unwrap();
    let view = runner.extension_view().await;

    assert!(
        view.run_recorded_hook::<()>(
            "state-probe",
            "pre_tool_use",
            tokio_util::sync::CancellationToken::new(),
            async { Err(ExtensionError::Internal("injected failure".into())) },
        )
        .await
        .is_err()
    );
    assert!(matches!(
        view.run_recorded_hook(
            "state-probe",
            "pre_tool_use",
            tokio_util::sync::CancellationToken::new(),
            std::future::pending::<Result<(), ExtensionError>>(),
        )
        .await,
        Err(ExtensionError::Timeout(10))
    ));

    let diagnostics = runner.diagnostics_snapshot();
    let diagnostics = diagnostics.get("state-probe").unwrap();
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
            runtime_provider_context(
                runtime_hook_call(),
                ProviderRequestId::new("operation-timeout"),
                Vec::new(),
            ),
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
            runtime_provider_context(
                runtime_hook_call(),
                ProviderRequestId::new("provider-response"),
                Vec::new(),
            ),
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
    let a_append_calls = Arc::new(AtomicUsize::new(0));
    let z_append_calls = Arc::new(AtomicUsize::new(0));
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
            id: "z-append-envelope",
            priority: 0,
            result: UserMessageEnvelopeResult::AppendText {
                text: "z-tail".into(),
            },
            calls: Arc::clone(&z_append_calls),
        }))
        .await
        .unwrap();
    runner
        .register(Arc::new(UserMessageEnvelopeProbeExtension {
            id: "a-append-envelope",
            priority: 0,
            result: UserMessageEnvelopeResult::AppendText {
                text: "a-tail".into(),
            },
            calls: Arc::clone(&a_append_calls),
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
            text: "rewritten\n\na-tail\n\nz-tail".into()
        }
    );
    assert_eq!(replace_calls.load(Ordering::SeqCst), 1);
    assert_eq!(a_append_calls.load(Ordering::SeqCst), 1);
    assert_eq!(z_append_calls.load(Ordering::SeqCst), 1);
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

    let declaration = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let snapshot = runner.registry_snapshot().await;
            let declaration = snapshot
                .extensions
                .into_iter()
                .find(|extension| extension.id == "state-probe")
                .unwrap();
            if declaration.runtime_state == ExtensionRuntimeState::Ready {
                break declaration;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    assert!(declaration.capabilities.is_empty());
    assert_eq!(declaration.runtime_state, ExtensionRuntimeState::Ready);
    assert!(declaration.generation > 0);
    assert_eq!(declaration.tools.len(), 1);
    assert_eq!(declaration.tools[0].name, "stateProbe");
    assert!(!declaration.dynamic_tools);
}

#[tokio::test]
async fn command_resolution_uses_declared_priority_then_extension_id() {
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

    assert_eq!(demo.extension_id, "astrcode-skill");
    assert_eq!(demo.shadowed.len(), 2);
    assert!(
        demo.shadowed
            .iter()
            .any(|command| command.extension_id == "normal-high" && command.priority == 5)
    );
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
            availability: CommandAvailability::AllTransports,
            execution: CommandExecution::Extension,
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
        .resolve_commands_for_typed(&runtime.working_dir().to_string_lossy())
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
async fn extension_tools_plan_the_resource_domain_their_handler_uses() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(SmallModelProbeExtension {
            small_model_allowed: false,
            session_control_allowed: true,
        }))
        .await
        .unwrap();
    let model_tool = runner
        .tool_catalog_snapshot_typed("D:/workspace")
        .await
        .tools
        .into_iter()
        .next()
        .unwrap();
    let planning = ToolPlanningContext::new(
        SessionId::new("session"),
        "D:/workspace",
        Some("call-model".into()),
    );
    assert_eq!(
        model_tool.plan(&json!({}), &planning).await.unwrap(),
        ToolPlan::host(HostResource::Model)
    );

    runner
        .register(Arc::new(StateProbeExtension))
        .await
        .unwrap();
    let state_tool = runner
        .tool_catalog_snapshot_typed("D:/workspace")
        .await
        .tools
        .into_iter()
        .find(|tool| tool.definition().name == "stateProbe")
        .unwrap();
    assert_eq!(
        state_tool.plan(&json!({}), &planning).await.unwrap(),
        ToolPlan::host(HostResource::Session)
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
            if code == WireErrorCode::InvalidInput.as_str()
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
async fn extension_http_uses_the_callers_pinned_generation_while_external_http_uses_current() {
    let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(1)));
    runner.bind_host_router(build_host_router_with_public_http_dispatcher(
        HostBackends::default(),
        runner.clone(),
    ));
    let startup_host = Arc::new(Mutex::new(None));

    let source_transaction = runner.begin_source_transaction().await;
    runner
        .prepare_source_generation(
            source_transaction,
            vec![
                SourceGenerationEntry::Start {
                    extension: Arc::new(GenerationHttpCaller {
                        startup_host: Arc::clone(&startup_host),
                    }),
                    key: "generation-http-caller".into(),
                    fingerprint: "caller-v1".into(),
                    config: json!({}),
                },
                SourceGenerationEntry::Start {
                    extension: Arc::new(GenerationHttpTarget { label: "v1" }),
                    key: "generation-http-target".into(),
                    fingerprint: "target-v1".into(),
                    config: json!({}),
                },
            ],
            None,
        )
        .await
        .unwrap()
        .commit_with(|_| {})
        .await;
    let version_one_view = runner.extension_view().await;
    let version_one_generation = version_one_view.generation();

    let source_transaction = runner.begin_source_transaction().await;
    let publication_probe = Arc::clone(&runner);
    runner
        .prepare_source_generation(
            source_transaction,
            vec![
                SourceGenerationEntry::Retain {
                    id: "generation-http-caller".into(),
                    key: "generation-http-caller".into(),
                    fingerprint: "caller-v1".into(),
                },
                SourceGenerationEntry::Start {
                    extension: Arc::new(GenerationHttpTarget { label: "v2" }),
                    key: "generation-http-target".into(),
                    fingerprint: "target-v2".into(),
                    config: json!({}),
                },
            ],
            None,
        )
        .await
        .unwrap()
        .commit_with(move |_| {
            assert_eq!(
                publication_probe.turn_extension_view().generation(),
                version_one_generation,
                "synchronous observers must not see the candidate before activation"
            );
        })
        .await;

    let old_turn_admission = version_one_view
        .emit_pre_tool_use(pre_tool_use_ctx("probe", json!({})))
        .await
        .unwrap();
    let PreToolUseAdmission::Ask { requirements } = old_turn_admission else {
        panic!("the caller should return its target generation");
    };
    assert_eq!(requirements.len(), 1);
    assert_eq!(requirements[0].prompt, "v1/v2");

    let external = runner
        .dispatch_public_http_route(
            ExtensionHttpRequest::new(ExtensionHttpMethod::Get, "/generation"),
            &[],
        )
        .await
        .unwrap();
    let ExtensionHttpDispatchResult::Response(response) = external else {
        panic!("the current external route should be available");
    };
    assert_eq!(response.body["generation"], "v2");

    drop(version_one_view);
    let retained_startup_host = startup_host.lock().unwrap().clone().unwrap();
    assert_eq!(
        generation_http_label(&retained_startup_host).await.unwrap(),
        "v2"
    );
    assert!(runner.shutdown().await.is_empty());
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

fn command_ctx() -> RuntimeHookCallContext {
    RuntimeHookCallContext::new(
        "session",
        ".",
        ModelSelection::simple("mock"),
        Some(PathBuf::from("/tmp/session-store")),
    )
}

struct CustomEventConsumerExtension {
    attempts: Arc<AtomicUsize>,
    calls: mpsc::UnboundedSender<(usize, String)>,
    blocking: Option<Arc<BlockingCustomEvent>>,
}

struct CustomEventConsumer {
    attempts: Arc<AtomicUsize>,
    calls: mpsc::UnboundedSender<(usize, String)>,
    blocking: Option<Arc<BlockingCustomEvent>>,
}

#[derive(Default)]
struct BlockingCustomEvent {
    entered: Notify,
    release: Notify,
}

struct StatefulCustomEventExtension {
    handler: Arc<StatefulCustomEvent>,
}

struct StatefulCustomEvent {
    entered: mpsc::UnboundedSender<PathBuf>,
    release: Notify,
}

#[async_trait::async_trait]
impl Extension for StatefulCustomEventExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest(
            "stateful-consumer",
            &[ExtensionCapability::ConsumeCustomEvents],
        )
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_custom_event(
            CustomEventSubscription::from_extension("producer", "job.completed"),
            0,
            Arc::clone(&self.handler) as Arc<dyn CustomEventHandler>,
        );
    }
}

#[async_trait::async_trait]
impl CustomEventHandler for StatefulCustomEvent {
    async fn handle(
        &self,
        ctx: CustomEventContext,
    ) -> Result<CustomEventDisposition, ExtensionError> {
        let session_data_dir = ctx.paths().session_data_dir()?.to_path_buf();
        let state = ctx.host().session_state()?;
        state
            .write(HostSessionStateWriteRequest {
                key: "delivery".into(),
                content: "started".into(),
            })
            .await?;
        let stored = state
            .read(HostSessionStateReadRequest {
                key: "delivery".into(),
            })
            .await?;
        assert_eq!(stored.content.as_deref(), Some("started"));
        let _ = self.entered.send(session_data_dir);
        self.release.notified().await;
        Ok(CustomEventDisposition::Ack)
    }
}

#[async_trait::async_trait]
impl Extension for CustomEventConsumerExtension {
    fn manifest(&self) -> ExtensionManifest {
        extension_manifest("consumer", &[ExtensionCapability::ConsumeCustomEvents])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_custom_event(
            CustomEventSubscription::from_extension("producer", "job.completed"),
            0,
            Arc::new(CustomEventConsumer {
                attempts: Arc::clone(&self.attempts),
                calls: self.calls.clone(),
                blocking: self.blocking.clone(),
            }),
        );
    }
}

#[async_trait::async_trait]
impl CustomEventHandler for CustomEventConsumer {
    async fn handle(
        &self,
        ctx: CustomEventContext,
    ) -> Result<CustomEventDisposition, ExtensionError> {
        assert_eq!(ctx.source_extension_id(), "producer");
        assert_eq!(ctx.event_type(), "job.completed");
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        let job_id = ctx.payload()["jobId"].as_str().unwrap().to_owned();
        let should_fail = attempt == 1 || job_id == "live-fails";
        let _ = self.calls.send((attempt, job_id.clone()));
        if let Some(blocking) = &self.blocking {
            blocking.entered.notify_one();
            blocking.release.notified().await;
        }
        if job_id == "dead-letter" {
            return Ok(CustomEventDisposition::dead_letter(
                "fixture requested dead letter",
            ));
        }
        if should_fail {
            Err(ExtensionError::Internal("injected consumer failure".into()))
        } else {
            Ok(CustomEventDisposition::Ack)
        }
    }
}

#[tokio::test]
async fn durable_custom_event_reconciles_from_checkpoint_and_retries_in_order() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    let attempts = Arc::new(AtomicUsize::new(0));
    let (calls_tx, mut calls_rx) = mpsc::unbounded_channel();
    runner
        .register(Arc::new(CustomEventConsumerExtension {
            attempts: Arc::clone(&attempts),
            calls: calls_tx,
            blocking: None,
        }))
        .await
        .unwrap();

    let session_id = SessionId::new("custom-event-session");
    let store = Arc::new(astrcode_storage::in_memory::InMemoryEventStore::new());
    store
        .create_session(DurableEvent::session(
            session_id.clone(),
            DurableEventPayload::SessionStarted(SessionStarted {
                working_dir: "/workspace".into(),
                model_id: "model".into(),
                parent: None,
                tool_selection: SessionToolSelection::default(),
                source_extension: None,
                initial_system_prompt: PersistedSystemPrompt {
                    text: "system".into(),
                    fingerprint: "fingerprint".into(),
                    extra_system_prompt: None,
                    source: SystemPromptSource::Native,
                },
            }),
        ))
        .await
        .unwrap();
    store
        .append_event(DurableEvent::session(
            session_id.clone(),
            DurableEventPayload::CustomEvent(CustomEventData {
                extension_id: "producer".into(),
                event_type: "job.completed".into(),
                schema_version: 1,
                audience: astrcode_core::event::CustomEventAudience::Session,
                causation_id: None,
                cascade_depth: 0,
                payload: json!({"jobId": "job-1"}),
            }),
        ))
        .await
        .unwrap();
    let second = store
        .append_event(DurableEvent::session(
            session_id.clone(),
            DurableEventPayload::CustomEvent(CustomEventData {
                extension_id: "producer".into(),
                event_type: "job.completed".into(),
                schema_version: 1,
                audience: astrcode_core::event::CustomEventAudience::Session,
                causation_id: None,
                cascade_depth: 0,
                payload: json!({"jobId": "job-2"}),
            }),
        ))
        .await
        .unwrap();
    let store_port: Arc<dyn SessionStore> = store.clone();
    let custom_event_session =
        CustomEventSession::new(Arc::clone(&store_port), |_| EventSender::new(|_| Ok(())));

    assert!(
        runner.observe_custom_event(Arc::new(Event::from(second)), custom_event_session.clone())
    );
    assert_eq!(calls_rx.recv().await, Some((1, "job-1".into())));
    assert_eq!(calls_rx.recv().await, Some((2, "job-1".into())));
    assert_eq!(calls_rx.recv().await, Some((3, "job-2".into())));

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if store_port
                .event_consumer_state(&session_id, "consumer:producer:job.completed:v1")
                .await
                .unwrap()
                .checkpoint
                == Some(2)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let status = runner
        .custom_event_consumer_statuses(&session_id, &custom_event_session)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(status.checkpoint, Some(2));
    assert_eq!(status.pending_events, 0);
    assert_eq!(status.failed_attempts, 1);

    let paused = runner
        .control_custom_event_consumer(
            &session_id,
            "consumer",
            "producer:job.completed",
            CustomEventConsumerAction::Pause,
            &custom_event_session,
        )
        .await
        .unwrap();
    assert!(paused.paused);
    let third = store
        .append_event(DurableEvent::session(
            session_id.clone(),
            DurableEventPayload::CustomEvent(CustomEventData {
                extension_id: "producer".into(),
                event_type: "job.completed".into(),
                schema_version: 1,
                audience: astrcode_core::event::CustomEventAudience::Session,
                causation_id: None,
                cascade_depth: 0,
                payload: json!({"jobId": "job-3"}),
            }),
        ))
        .await
        .unwrap();
    assert!(
        runner.observe_custom_event(Arc::new(Event::from(third)), custom_event_session.clone())
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(150), calls_rx.recv())
            .await
            .is_err()
    );
    let replay = runner
        .control_custom_event_consumer(
            &session_id,
            "consumer",
            "producer:job.completed",
            CustomEventConsumerAction::ReplayFromBeginning,
            &custom_event_session,
        )
        .await
        .unwrap();
    assert!(replay.paused);
    assert_eq!(replay.checkpoint, None);
    assert_eq!(replay.pending_events, 3);

    runner
        .control_custom_event_consumer(
            &session_id,
            "consumer",
            "producer:job.completed",
            CustomEventConsumerAction::Resume,
            &custom_event_session,
        )
        .await
        .unwrap();
    assert_eq!(calls_rx.recv().await, Some((4, "job-1".into())));
    assert_eq!(calls_rx.recv().await, Some((5, "job-2".into())));
    assert_eq!(calls_rx.recv().await, Some((6, "job-3".into())));

    runner
        .control_custom_event_consumer(
            &session_id,
            "consumer",
            "producer:job.completed",
            CustomEventConsumerAction::Pause,
            &custom_event_session,
        )
        .await
        .unwrap();
    let fourth = store
        .append_event(DurableEvent::session(
            session_id.clone(),
            DurableEventPayload::CustomEvent(CustomEventData {
                extension_id: "producer".into(),
                event_type: "job.completed".into(),
                schema_version: 1,
                audience: astrcode_core::event::CustomEventAudience::Session,
                causation_id: None,
                cascade_depth: 0,
                payload: json!({"jobId": "job-4"}),
            }),
        ))
        .await
        .unwrap();
    assert!(
        runner.observe_custom_event(Arc::new(Event::from(fourth)), custom_event_session.clone())
    );
    let skipped = runner
        .control_custom_event_consumer(
            &session_id,
            "consumer",
            "producer:job.completed",
            CustomEventConsumerAction::SkipToStreamHead,
            &custom_event_session,
        )
        .await
        .unwrap();
    assert!(skipped.paused);
    assert_eq!(skipped.checkpoint, Some(4));
    assert_eq!(skipped.pending_events, 0);
    runner
        .control_custom_event_consumer(
            &session_id,
            "consumer",
            "producer:job.completed",
            CustomEventConsumerAction::Resume,
            &custom_event_session,
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(150), calls_rx.recv())
            .await
            .is_err()
    );

    assert!(runner.observe_custom_event(
        Arc::new(Event::from(LiveEvent::session(
            session_id.clone(),
            LiveEventPayload::CustomEvent(CustomEventData {
                extension_id: "producer".into(),
                event_type: "job.completed".into(),
                schema_version: 1,
                audience: astrcode_core::event::CustomEventAudience::Session,
                causation_id: None,
                cascade_depth: 0,
                payload: json!({"jobId": "live-fails"}),
            }),
        ))),
        custom_event_session.clone(),
    ));
    assert_eq!(calls_rx.recv().await, Some((7, "live-fails".into())));
    assert!(
        tokio::time::timeout(Duration::from_millis(150), calls_rx.recv())
            .await
            .is_err()
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 7);
    let status = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let status = runner
                .custom_event_consumer_statuses(&session_id, &custom_event_session)
                .await
                .unwrap()
                .remove(0);
            if !status.in_flight && status.failed_attempts == 2 {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(status.failed_attempts, 2);
    assert_eq!(status.consecutive_failures, 1);

    let dead_letter = store
        .append_event(DurableEvent::session(
            session_id.clone(),
            DurableEventPayload::CustomEvent(CustomEventData {
                extension_id: "producer".into(),
                event_type: "job.completed".into(),
                schema_version: 1,
                audience: astrcode_core::event::CustomEventAudience::Session,
                causation_id: None,
                cascade_depth: 0,
                payload: json!({"jobId": "dead-letter"}),
            }),
        ))
        .await
        .unwrap();
    assert!(runner.observe_custom_event(
        Arc::new(Event::from(dead_letter.clone())),
        custom_event_session.clone(),
    ));
    assert_eq!(calls_rx.recv().await, Some((8, "dead-letter".into())));
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let state = store_port
                .event_consumer_state(&session_id, "consumer:producer:job.completed:v1")
                .await
                .unwrap();
            if state.checkpoint == Some(dead_letter.seq) {
                assert_eq!(state.quarantined.len(), 1);
                assert_eq!(state.quarantined[0].seq, dead_letter.seq);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let unrelated = store
        .append_event(DurableEvent::session(
            session_id.clone(),
            DurableEventPayload::CustomEvent(CustomEventData {
                extension_id: "producer".into(),
                event_type: "job.ignored".into(),
                schema_version: 1,
                audience: astrcode_core::event::CustomEventAudience::Session,
                causation_id: None,
                cascade_depth: 0,
                payload: json!({}),
            }),
        ))
        .await
        .unwrap();
    assert!(runner.reconcile_custom_events(&session_id, custom_event_session.clone()));
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if store_port
                .event_consumer_state(&session_id, "consumer:producer:job.completed:v1")
                .await
                .unwrap()
                .checkpoint
                == Some(unrelated.seq)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let status = runner
        .custom_event_consumer_statuses(&session_id, &custom_event_session)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(status.checkpoint, Some(unrelated.seq));
    assert_eq!(status.stream_head, Some(unrelated.seq));
    assert_eq!(status.pending_events, 0);

    let durable_before_live = store
        .append_event(DurableEvent::session(
            session_id.clone(),
            DurableEventPayload::CustomEvent(CustomEventData {
                extension_id: "producer".into(),
                event_type: "job.completed".into(),
                schema_version: 1,
                audience: astrcode_core::event::CustomEventAudience::Session,
                causation_id: None,
                cascade_depth: 0,
                payload: json!({"jobId": "durable-before-live"}),
            }),
        ))
        .await
        .unwrap();
    let durable_after_live = store
        .append_event(DurableEvent::session(
            session_id.clone(),
            DurableEventPayload::CustomEvent(CustomEventData {
                extension_id: "producer".into(),
                event_type: "job.completed".into(),
                schema_version: 1,
                audience: astrcode_core::event::CustomEventAudience::Session,
                causation_id: None,
                cascade_depth: 0,
                payload: json!({"jobId": "durable-after-live"}),
            }),
        ))
        .await
        .unwrap();
    assert!(runner.observe_custom_event(
        Arc::new(Event::from(durable_before_live)),
        custom_event_session.clone(),
    ));
    assert!(runner.observe_custom_event(
        Arc::new(Event::from(LiveEvent::session(
            session_id,
            LiveEventPayload::CustomEvent(CustomEventData {
                extension_id: "producer".into(),
                event_type: "job.completed".into(),
                schema_version: 1,
                audience: astrcode_core::event::CustomEventAudience::Session,
                causation_id: None,
                cascade_depth: 0,
                payload: json!({"jobId": "live-barrier"}),
            }),
        ))),
        custom_event_session.clone(),
    ));
    assert!(runner.observe_custom_event(
        Arc::new(Event::from(durable_after_live)),
        custom_event_session,
    ));
    assert_eq!(
        calls_rx.recv().await,
        Some((9, "durable-before-live".into()))
    );
    assert_eq!(calls_rx.recv().await, Some((10, "live-barrier".into())));
    assert_eq!(
        calls_rx.recv().await,
        Some((11, "durable-after-live".into()))
    );
}

#[tokio::test]
async fn skip_to_stream_head_waits_for_in_flight_delivery_and_suppresses_its_retry() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    let attempts = Arc::new(AtomicUsize::new(0));
    let blocking = Arc::new(BlockingCustomEvent::default());
    let (calls_tx, mut calls_rx) = mpsc::unbounded_channel();
    runner
        .register(Arc::new(CustomEventConsumerExtension {
            attempts,
            calls: calls_tx,
            blocking: Some(Arc::clone(&blocking)),
        }))
        .await
        .unwrap();

    let session_id = SessionId::new("custom-event-control-race");
    let store = Arc::new(astrcode_storage::in_memory::InMemoryEventStore::new());
    store
        .create_session(DurableEvent::session(
            session_id.clone(),
            DurableEventPayload::SessionStarted(SessionStarted {
                working_dir: "/workspace".into(),
                model_id: "model".into(),
                parent: None,
                tool_selection: SessionToolSelection::default(),
                source_extension: None,
                initial_system_prompt: PersistedSystemPrompt {
                    text: "system".into(),
                    fingerprint: "fingerprint".into(),
                    extra_system_prompt: None,
                    source: SystemPromptSource::Native,
                },
            }),
        ))
        .await
        .unwrap();
    let blocked = store
        .append_event(DurableEvent::session(
            session_id.clone(),
            DurableEventPayload::CustomEvent(CustomEventData {
                extension_id: "producer".into(),
                event_type: "job.completed".into(),
                schema_version: 1,
                audience: astrcode_core::event::CustomEventAudience::Session,
                causation_id: None,
                cascade_depth: 0,
                payload: json!({"jobId": "blocked"}),
            }),
        ))
        .await
        .unwrap();
    let store_port: Arc<dyn SessionStore> = store;
    let session = CustomEventSession::new(store_port, |_| EventSender::new(|_| Ok(())));
    assert!(runner.observe_custom_event(Arc::new(Event::from(blocked)), session.clone()));
    blocking.entered.notified().await;
    assert_eq!(calls_rx.recv().await, Some((1, "blocked".into())));

    let control = runner.control_custom_event_consumer(
        &session_id,
        "consumer",
        "producer:job.completed",
        CustomEventConsumerAction::SkipToStreamHead,
        &session,
    );
    tokio::pin!(control);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), control.as_mut())
            .await
            .is_err()
    );
    blocking.release.notify_one();
    let status = tokio::time::timeout(Duration::from_secs(1), control)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.checkpoint, Some(1));
    assert_eq!(status.pending_events, 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(250), calls_rx.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn custom_event_delivery_has_session_state_and_quiescence_blocks_new_admission() {
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let handler = Arc::new(StatefulCustomEvent {
        entered: entered_tx,
        release: Notify::new(),
    });
    runner
        .register(Arc::new(StatefulCustomEventExtension {
            handler: Arc::clone(&handler),
        }))
        .await
        .unwrap();

    let projects = tempfile::tempdir().unwrap();
    let store = Arc::new(filesystem_session_repository(projects.path().into()));
    let session_id = SessionId::new("stateful-custom-event");
    store
        .create_session(DurableEvent::session(
            session_id.clone(),
            DurableEventPayload::SessionStarted(SessionStarted {
                working_dir: "/workspace".into(),
                model_id: "model".into(),
                parent: None,
                tool_selection: SessionToolSelection::default(),
                source_extension: None,
                initial_system_prompt: PersistedSystemPrompt {
                    text: "system".into(),
                    fingerprint: "fingerprint".into(),
                    extra_system_prompt: None,
                    source: SystemPromptSource::Native,
                },
            }),
        ))
        .await
        .unwrap();
    let event = store
        .append_event(DurableEvent::session(
            session_id.clone(),
            DurableEventPayload::CustomEvent(CustomEventData {
                extension_id: "producer".into(),
                event_type: "job.completed".into(),
                schema_version: 1,
                audience: astrcode_core::event::CustomEventAudience::Session,
                causation_id: None,
                cascade_depth: 0,
                payload: json!({"jobId": "stateful"}),
            }),
        ))
        .await
        .unwrap();
    let expected_store_dir = store.session_store_dir(&session_id).await.unwrap().unwrap();
    let store_port: Arc<dyn SessionStore> = store;
    let session = CustomEventSession::new(store_port, |_| EventSender::new(|_| Ok(())));

    assert!(runner.observe_custom_event(Arc::new(Event::from(event.clone())), session.clone()));
    assert_eq!(
        entered_rx.recv().await,
        Some(expected_store_dir.join("extension_data/stateful-consumer"))
    );

    let quiesce = runner.quiesce_custom_event_session(&session_id);
    tokio::pin!(quiesce);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), quiesce.as_mut())
            .await
            .is_err()
    );
    handler.release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), quiesce)
        .await
        .unwrap();
    assert!(!runner.observe_custom_event(Arc::new(Event::from(event)), session.clone()));

    runner.resume_custom_event_session(&session_id, session);
}
