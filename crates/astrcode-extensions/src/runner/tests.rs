use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use astrcode_core::{config::ModelSelection, event::EventPayload, tool_access::ResourceAccess};
use astrcode_extension_sdk::{
    extension::{
        AfterToolResult, AfterToolResultsContext, AfterToolResultsHandler, AfterToolResultsResult,
        CommandCompletionItem, CommandCompletions, CommandContext, CommandHandler,
        ContinueAfterStopContext, ContinueAfterStopHandler, ContinueAfterStopOptions,
        ContinueAfterStopResult, Extension, ExtensionCapability, ExtensionCommandResult,
        ExtensionCtx, ExtensionError, ExtensionHttpHandler, ExtensionHttpMethod,
        ExtensionHttpRequest, ExtensionHttpResponse, ExtensionHttpRoute, HookMode,
        PreToolUseContext, PreToolUseHandler, PreToolUseResult, ProviderContext, ProviderEvent,
        ProviderHandler, ProviderResult, Registrar, SlashCommand, StopReason, ToolHandler,
        ToolHookTarget, UserMessageEnvelopeContext, UserMessageEnvelopeHandler,
        UserMessageEnvelopeResult,
    },
    tool::{
        ExecutionMode, ToolCapabilities, ToolDefinition, ToolExecutionContext, ToolOrigin,
        ToolResult,
    },
};
use serde_json::json;
use tokio::sync::mpsc;

use super::{ExtensionHttpDispatchResult, ExtensionRunner};
use crate::runner::tool_adapter::normalize_stringified_booleans;

struct ManagedTaskExtension {
    started: Arc<AtomicUsize>,
    stopped: Arc<AtomicUsize>,
    task_stopped: Arc<AtomicBool>,
    expected_reason: StopReason,
}

struct StartupDirectoryExtension {
    received: Arc<Mutex<Option<String>>>,
}

struct StartupEventExtension;

struct UnhealthyExtension;

struct StateProbeExtension;

struct StateProbeTool;

struct HttpProbeExtension {
    id: &'static str,
    capabilities: Vec<ExtensionCapability>,
    route: ExtensionHttpRoute,
}

struct HttpProbeHandler;

#[async_trait::async_trait]
impl ExtensionHttpHandler for HttpProbeHandler {
    async fn handle(
        &self,
        request: ExtensionHttpRequest,
    ) -> Result<ExtensionHttpResponse, ExtensionError> {
        Ok(ExtensionHttpResponse::json(
            201,
            json!({
                "pathParams": request.path_params,
                "query": request.query,
                "body": request.body,
            }),
        ))
    }
}

#[async_trait::async_trait]
impl Extension for HttpProbeExtension {
    fn id(&self) -> &str {
        self.id
    }

    fn capabilities(&self) -> &[ExtensionCapability] {
        &self.capabilities
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

struct StartFailingExtension;

struct StartupTimeoutExtension {
    task_stopped: Arc<AtomicBool>,
    stop_reason: Arc<Mutex<Option<StopReason>>>,
}

struct BlockingProviderResponseExtension;

struct BlockingProviderHook;

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

struct AfterToolResultsProbeExtension {
    id: &'static str,
    priority: i32,
    result: AfterToolResultsResult,
    calls: Arc<AtomicUsize>,
}

struct AfterToolResultsProbe {
    result: AfterToolResultsResult,
    calls: Arc<AtomicUsize>,
}

struct CommandProbeExtension {
    id: &'static str,
    command_name: &'static str,
    priority: i32,
    argument_completions: bool,
}

struct CommandProbe {
    label: &'static str,
    argument_completions: bool,
}

#[async_trait::async_trait]
impl Extension for StateProbeExtension {
    fn id(&self) -> &str {
        "state-probe"
    }

    fn register(&self, reg: &mut Registrar) {
        reg.tool(
            ToolDefinition {
                name: "stateProbe".into(),
                description: String::new(),
                parameters: json!({"type": "object"}),
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
        _tool_name: &str,
        _arguments: serde_json::Value,
        _working_dir: &str,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        Ok(ToolResult::text(
            ctx.capabilities.paths.store_dir.is_some().to_string(),
            false,
            Default::default(),
        ))
    }
}

#[async_trait::async_trait]
impl Extension for CommandProbeExtension {
    fn id(&self) -> &str {
        self.id
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
        _command_name: &str,
        _args: &str,
        _working_dir: &str,
        _ctx: &CommandContext,
    ) -> Result<ExtensionCommandResult, ExtensionError> {
        Ok(ExtensionCommandResult::handled(self.label))
    }

    async fn complete(
        &self,
        _command_name: &str,
        argument: &str,
        cursor: usize,
        _working_dir: &str,
        _ctx: &CommandContext,
    ) -> Result<CommandCompletions, ExtensionError> {
        if !self.argument_completions {
            return Ok(CommandCompletions::default());
        }
        Ok(CommandCompletions {
            items: vec![CommandCompletionItem {
                label: format!("{}:{argument}:{cursor}", self.label),
                insert_text: self.label.into(),
                detail: Some("probe".into()),
            }],
            truncated: false,
        })
    }
}

#[async_trait::async_trait]
impl Extension for SmallModelProbeExtension {
    fn id(&self) -> &str {
        "small-model-probe"
    }

    fn capabilities(&self) -> &[ExtensionCapability] {
        match (self.small_model_allowed, self.session_control_allowed) {
            (true, true) => &[
                ExtensionCapability::SmallModel,
                ExtensionCapability::SessionControl,
            ],
            (true, false) => &[ExtensionCapability::SmallModel],
            (false, true) => &[ExtensionCapability::SessionControl],
            (false, false) => &[],
        }
    }

    fn register(&self, reg: &mut Registrar) {
        reg.tool(
            ToolDefinition {
                name: "smallModelProbe".into(),
                description: String::new(),
                parameters: json!({"type": "object"}),
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
        _tool_name: &str,
        _arguments: serde_json::Value,
        _working_dir: &str,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        Ok(ToolResult::text(
            ctx.capabilities.models.small.is_some().to_string(),
            false,
            Default::default(),
        ))
    }
}

#[async_trait::async_trait]
impl Extension for TargetedPreHookExtension {
    fn id(&self) -> &str {
        "targeted-pre-hook"
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
impl Extension for StartFailingExtension {
    fn id(&self) -> &str {
        "start-failing"
    }

    async fn start(&self, _ctx: ExtensionCtx) -> Result<(), ExtensionError> {
        Err(ExtensionError::Internal(
            "startup dependency missing".into(),
        ))
    }
}

#[async_trait::async_trait]
impl Extension for StartupTimeoutExtension {
    fn id(&self) -> &str {
        "startup-timeout"
    }

    async fn start(&self, ctx: ExtensionCtx) -> Result<(), ExtensionError> {
        let shutdown = ctx.shutdown();
        let task_stopped = Arc::clone(&self.task_stopped);
        ctx.tasks().spawn("startup-task", async move {
            shutdown.cancelled().await;
            task_stopped.store(true, Ordering::SeqCst);
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
    fn id(&self) -> &str {
        "provider-response-observer"
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_provider(
            ProviderEvent::AfterResponse,
            HookMode::Blocking,
            0,
            Arc::new(BlockingProviderHook),
        );
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
impl Extension for ContinueAfterStopProbeExtension {
    fn id(&self) -> &str {
        self.id
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
    fn id(&self) -> &str {
        self.id
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
impl Extension for AfterToolResultsProbeExtension {
    fn id(&self) -> &str {
        self.id
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_after_tool_results(
            self.priority,
            Arc::new(AfterToolResultsProbe {
                result: self.result.clone(),
                calls: Arc::clone(&self.calls),
            }),
        );
    }
}

#[async_trait::async_trait]
impl AfterToolResultsHandler for AfterToolResultsProbe {
    async fn handle(
        &self,
        _ctx: AfterToolResultsContext,
    ) -> Result<AfterToolResultsResult, ExtensionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.result.clone())
    }
}

fn continue_after_stop_ctx(continuations_this_turn: u32) -> ContinueAfterStopContext {
    ContinueAfterStopContext {
        session_id: "session".into(),
        working_dir: "D:/workspace".into(),
        model: ModelSelection::simple("model"),
        assistant_text: "done".into(),
        finish_reason: "stop".into(),
        continuations_this_turn,
    }
}

fn user_message_envelope_ctx(text: &str) -> UserMessageEnvelopeContext {
    UserMessageEnvelopeContext {
        session_id: "session".into(),
        turn_id: "turn".into(),
        working_dir: "D:/workspace".into(),
        model: ModelSelection::simple("model"),
        text: text.into(),
        attachments: Vec::new(),
        session_store_dir: None,
    }
}

fn after_tool_results_ctx() -> AfterToolResultsContext {
    AfterToolResultsContext {
        session_id: "session".into(),
        working_dir: "D:/workspace".into(),
        model: ModelSelection::simple("model"),
        tool_results: vec![AfterToolResult {
            call_id: "call-1".into(),
            tool_name: "probeTool".into(),
            tool_input: json!({"value": 1}),
            tool_result: ToolResult::text("ok".into(), false, Default::default()),
        }],
        session_store_dir: None,
    }
}

#[async_trait::async_trait]
impl Extension for StartupDirectoryExtension {
    fn id(&self) -> &str {
        "startup-directory"
    }

    async fn start(&self, ctx: ExtensionCtx) -> Result<(), ExtensionError> {
        *self.received.lock().unwrap() = ctx.startup_working_dir().map(str::to_string);
        Ok(())
    }
}

#[async_trait::async_trait]
impl Extension for StartupEventExtension {
    fn id(&self) -> &str {
        "startup-event"
    }

    fn register(&self, reg: &mut Registrar) {
        reg.extension_event("startup_ready").register();
    }

    async fn start(&self, ctx: ExtensionCtx) -> Result<(), ExtensionError> {
        let sink = ctx
            .event_sink()
            .ok_or_else(|| ExtensionError::Internal("missing startup event sink".into()))?;
        sink.emit("startup_ready", 1, json!({"ready": true})).await
    }
}

#[async_trait::async_trait]
impl Extension for UnhealthyExtension {
    fn id(&self) -> &str {
        "unhealthy"
    }

    async fn health(&self) -> Result<(), ExtensionError> {
        Err(ExtensionError::Internal("dependency unavailable".into()))
    }
}

#[async_trait::async_trait]
impl Extension for ManagedTaskExtension {
    fn id(&self) -> &str {
        "managed-task"
    }

    fn register(&self, _reg: &mut Registrar) {}

    async fn start(&self, ctx: ExtensionCtx) -> Result<(), ExtensionError> {
        self.started.fetch_add(1, Ordering::SeqCst);
        let shutdown = ctx.shutdown();
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
    assert_eq!(started.load(Ordering::SeqCst), 1);
    assert_eq!(stopped.load(Ordering::SeqCst), 1);
    assert!(task_stopped.load(Ordering::SeqCst));
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
async fn register_passes_startup_working_dir_to_extension() {
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

    assert_eq!(received.lock().unwrap().as_deref(), Some("D:/workspace"));
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
        EventPayload::ExtensionEvent {
            extension_id,
            event_type,
            schema_version: 1,
            payload,
        } if extension_id == "startup-event"
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
        .collect_tool_adapters_typed("D:/workspace")
        .await
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
            .collect_tool_adapters_typed("D:/workspace")
            .await
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
                    small: Some("small-model".into()),
                    ..Default::default()
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

    let base_ctx = |tool_name: &str| PreToolUseContext {
        session_id: "session".into(),
        working_dir: "D:/workspace".into(),
        model: astrcode_core::config::ModelSelection::simple("model"),
        tool_name: tool_name.into(),
        tool_input: json!({}),
        approval_mode: astrcode_core::permission::ApprovalMode::Manual,
        available_tools: Vec::new(),
        event_tx: None,
        extension_event_sink: None,
        session_store_dir: None,
    };

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
async fn startup_timeout_cleans_tasks_and_rolls_back_partial_start() {
    let task_stopped = Arc::new(AtomicBool::new(false));
    let stop_reason = Arc::new(Mutex::new(None));
    let runner = ExtensionRunner::new(Duration::from_millis(20));

    let error = runner
        .register(Arc::new(StartupTimeoutExtension {
            task_stopped: Arc::clone(&task_stopped),
            stop_reason: Arc::clone(&stop_reason),
        }))
        .await
        .unwrap_err();

    assert!(matches!(error, ExtensionError::Timeout(20)));
    assert!(task_stopped.load(Ordering::SeqCst));
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
            ProviderContext {
                session_id: "session".into(),
                working_dir: "D:/workspace".into(),
                model: astrcode_core::config::ModelSelection::simple("model"),
                messages: Vec::new(),
                session_store_dir: None,
            },
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
async fn after_tool_results_end_turn_short_circuits_later_handlers() {
    let end_calls = Arc::new(AtomicUsize::new(0));
    let continue_calls = Arc::new(AtomicUsize::new(0));
    let runner = ExtensionRunner::new(Duration::from_secs(1));
    runner
        .register(Arc::new(AfterToolResultsProbeExtension {
            id: "end-after-tools",
            priority: 10,
            result: AfterToolResultsResult::EndTurn {
                reason: "goal-complete".into(),
            },
            calls: Arc::clone(&end_calls),
        }))
        .await
        .unwrap();
    runner
        .register(Arc::new(AfterToolResultsProbeExtension {
            id: "continue-after-end",
            priority: 0,
            result: AfterToolResultsResult::Continue,
            calls: Arc::clone(&continue_calls),
        }))
        .await
        .unwrap();

    let result = runner
        .emit_after_tool_results(after_tool_results_ctx())
        .await
        .unwrap();

    assert_eq!(
        result,
        AfterToolResultsResult::EndTurn {
            reason: "goal-complete".into()
        }
    );
    assert_eq!(end_calls.load(Ordering::SeqCst), 1);
    assert_eq!(continue_calls.load(Ordering::SeqCst), 0);
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
    assert_eq!(demo.source, "extension");
    assert_eq!(demo.shadowed.len(), 2);
    assert!(
        demo.shadowed.iter().any(|command| {
            command.extension_id == "astrcode-skill" && command.source == "skill"
        })
    );
}

#[tokio::test]
async fn command_completion_dispatches_to_resolved_handler() {
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

    let completions = runner
        .complete_command_typed("pick", "de", 2, ".", &command_ctx())
        .await
        .unwrap();

    assert_eq!(completions.items.len(), 1);
    assert_eq!(completions.items[0].label, "complete-high:de:2");
    assert_eq!(completions.items[0].insert_text, "complete-high");
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
        .collect_tool_adapters_typed("D:/workspace")
        .await
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
        .collect_tool_adapters_typed("D:/workspace")
        .await
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
    assert_eq!(response.body["pathParams"]["jobId"], "job-1");
    assert_eq!(response.body["query"], "run=true");
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

    assert!(error.to_string().contains("public_http"));
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

fn command_ctx() -> CommandContext {
    CommandContext {
        session_id: "session".into(),
        working_dir: ".".into(),
        model: ModelSelection::simple("mock"),
        session_store_dir: None,
    }
}
