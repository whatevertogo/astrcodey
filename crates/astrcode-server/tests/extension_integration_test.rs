//! Integration test: extensions can block tool execution via PreToolUse hooks.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use astrcode_core::tool::{
    ToolDefinition, ToolExecutionContext, ToolOrigin, ToolPlanningContext, ToolResult,
    access::ResourceLease,
};
use astrcode_extension_sdk::{
    builder::manifest,
    extension::{
        ExtensionCapability, ExtensionError, ExtensionManifest, HookMode, HookResult,
        LifecycleContext, PreToolUseAdmission, PreToolUseContext, PreToolUseResult, Registrar,
        ToolContext, ToolHandler, ToolPlanContext,
        internal::{
            RuntimeHookCallContext, RuntimeLifecycleContext, RuntimePreToolUseContext,
            runtime_lifecycle_context, runtime_pre_tool_use_context,
        },
    },
    tool::ToolPlan,
};
use astrcode_extensions::{
    Extension, runner::ExtensionRunner, testing::extension_runner_with_extensions,
};
use astrcode_session::ToolRegistry;
use tokio::sync::Notify;

fn test_manifest(id: impl Into<String>, capabilities: &[ExtensionCapability]) -> ExtensionManifest {
    capabilities
        .iter()
        .copied()
        .fold(
            manifest(id)
                .version("test")
                .description("Extension integration test probe"),
            |manifest, capability| manifest.capability(capability),
        )
        .build()
}

// ─── Test extensions using register() ─────────────────────────────────────

struct SecurityExtension;

impl Extension for SecurityExtension {
    fn manifest(&self) -> ExtensionManifest {
        test_manifest("test-security", &[ExtensionCapability::ToolIntercept])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_pre_tool_use(0, Arc::new(SecurityHandler));
    }
}

struct SecurityHandler;

#[async_trait::async_trait]
impl astrcode_extension_sdk::extension::PreToolUseHandler for SecurityHandler {
    async fn handle(&self, ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError> {
        if ctx.tool_name() == "shell"
            && ctx
                .tool_input()
                .get("command")
                .and_then(|value| value.as_str())
                .is_some_and(|command| command.contains("rm -rf"))
        {
            return Ok(PreToolUseResult::Block {
                reason: "dangerous shell command".into(),
            });
        }
        Ok(PreToolUseResult::Allow)
    }
}

struct AlwaysBlockExtension;

impl Extension for AlwaysBlockExtension {
    fn manifest(&self) -> ExtensionManifest {
        test_manifest("test-always-block", &[ExtensionCapability::ToolIntercept])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_pre_tool_use(0, Arc::new(AlwaysBlockHandler));
    }
}

struct AlwaysBlockHandler;

#[async_trait::async_trait]
impl astrcode_extension_sdk::extension::PreToolUseHandler for AlwaysBlockHandler {
    async fn handle(&self, _ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError> {
        Ok(PreToolUseResult::Block {
            reason: "blocked by AlwaysBlockExtension".into(),
        })
    }
}

struct EchoToolExtension;

impl Extension for EchoToolExtension {
    fn manifest(&self) -> ExtensionManifest {
        test_manifest("test-echo-tool", &[])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.tool(
            ToolDefinition {
                name: "extensionEcho".into(),
                description: "echo from extension".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string" }
                    }
                }),
                strict: false,
                origin: ToolOrigin::Extension,
            },
            Arc::new(EchoToolHandler),
        );
    }
}

struct EchoToolHandler;

#[async_trait::async_trait]
impl ToolHandler for EchoToolHandler {
    async fn plan(&self, _ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        Ok(ToolPlan::default())
    }

    async fn execute(
        &self,
        ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        let tool_name = ctx.tool_name();
        if tool_name != "extensionEcho" {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }
        let text = ctx
            .raw_arguments()
            .get("text")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let working_dir = ctx.working_dir().display();
        Ok(ToolResult {
            content: format!("{working_dir}:{text}"),
            is_error: false,
            error: None,
            metadata: BTreeMap::new(),
            duration_ms: None,
        }
        .into())
    }
}

struct FixedToolExtension {
    id: &'static str,
    tool_name: &'static str,
    content: &'static str,
}

impl Extension for FixedToolExtension {
    fn manifest(&self) -> ExtensionManifest {
        test_manifest(self.id, &[])
    }

    fn register(&self, reg: &mut Registrar) {
        let tool_name = self.tool_name;
        let content = self.content;
        let description = format!("{} tool", self.id);
        reg.tool(
            ToolDefinition {
                name: tool_name.into(),
                description,
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
                strict: false,
                origin: ToolOrigin::Extension,
            },
            Arc::new(FixedToolHandler {
                tool_name: tool_name.to_string(),
                content: content.to_string(),
            }),
        );
    }
}

struct FixedToolHandler {
    tool_name: String,
    content: String,
}

#[async_trait::async_trait]
impl ToolHandler for FixedToolHandler {
    async fn plan(&self, _ctx: ToolPlanContext) -> Result<ToolPlan, ExtensionError> {
        Ok(ToolPlan::default())
    }

    async fn execute(
        &self,
        ctx: ToolContext,
    ) -> Result<astrcode_extension_sdk::tool::ToolExecutionResult, ExtensionError> {
        let tool_name = ctx.tool_name();
        if tool_name != self.tool_name {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }
        Ok(ToolResult {
            content: self.content.clone(),
            is_error: false,
            error: None,
            metadata: BTreeMap::new(),
            duration_ms: None,
        }
        .into())
    }
}

// ─── Lifecycle observer context probe ─────────────────────────────────────

struct FireAndForgetExt {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    completed: Arc<Notify>,
}

impl Extension for FireAndForgetExt {
    fn manifest(&self) -> ExtensionManifest {
        test_manifest("test-faf", &[])
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_lifecycle(
            astrcode_extension_sdk::extension::LifecycleEvent::TurnEnd,
            HookMode::NonBlocking,
            0,
            Arc::new(FafHandler {
                entered: Arc::clone(&self.entered),
                release: Arc::clone(&self.release),
                completed: Arc::clone(&self.completed),
            }),
        );
    }
}

struct FafHandler {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    completed: Arc<Notify>,
}

#[async_trait::async_trait]
impl astrcode_extension_sdk::extension::LifecycleHandler for FafHandler {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        assert_eq!(ctx.session_id().as_str(), "test-session");
        assert_eq!(ctx.working_dir(), std::path::Path::new("/tmp"));
        self.entered.notify_one();
        self.release.notified().await;
        self.completed.notify_one();
        Ok(HookResult::Allow)
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────

fn hook_call(session_id: &str, model_id: &str) -> RuntimeHookCallContext {
    RuntimeHookCallContext::new(
        session_id,
        "/tmp",
        astrcode_core::config::ModelSelection::simple(model_id),
        None,
    )
}

fn pre_tool_use_context(command: &str) -> RuntimePreToolUseContext {
    runtime_pre_tool_use_context(
        hook_call("test-session", "test-model"),
        "call-1".into(),
        "shell",
        serde_json::json!({ "command": command }),
        astrcode_core::permission::ApprovalMode::Manual,
        Vec::new(),
    )
}

fn lifecycle_context(session_id: &str, model_id: &str) -> RuntimeLifecycleContext {
    runtime_lifecycle_context(hook_call(session_id, model_id), None, 0)
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn duplicate_extension_tools_are_rejected_at_registration() {
    let runner = ExtensionRunner::new(Duration::from_secs(5));
    runner
        .register(Arc::new(FixedToolExtension {
            id: "project",
            tool_name: "sharedTool",
            content: "project",
        }))
        .await
        .unwrap();
    let error = runner
        .register(Arc::new(FixedToolExtension {
            id: "global",
            tool_name: "sharedTool",
            content: "global",
        }))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        ExtensionError::ToolConflict {
            extension_id,
            tool_name,
            conflicting_extension_id,
        } if extension_id == "global"
            && tool_name == "sharedTool"
            && conflicting_extension_id == "project"
    ));
}

#[tokio::test]
async fn extension_registration_and_count() {
    let runner = ExtensionRunner::new(Duration::from_secs(5));
    assert_eq!(runner.count().await, 0);

    runner.register(Arc::new(SecurityExtension)).await.unwrap();
    assert_eq!(runner.count().await, 1);
}

#[tokio::test]
async fn extension_tools_are_adapted_into_tool_registry() {
    let runner = extension_runner_with_extensions(
        Duration::from_secs(5),
        None,
        vec![Arc::new(EchoToolExtension)],
    )
    .await
    .unwrap();

    let tools = runner.tool_catalog_snapshot_typed("/workspace").await.tools;
    let mut tool_registry = ToolRegistry::new();
    for tool in tools {
        tool_registry.register(tool).unwrap();
    }

    let definitions = tool_registry.list_definitions();
    assert!(definitions.iter().any(|def| def.name == "extensionEcho"));

    let arguments = serde_json::json!({ "text": "hello" });
    let planning = ToolPlanningContext::new("test".into(), "/workspace", None);
    let plan = tool_registry
        .plan("extensionEcho", &arguments, &planning)
        .await
        .unwrap();
    let ctx =
        ToolExecutionContext::new("test".into(), "/workspace", None, None, Default::default())
            .with_resource_lease(ResourceLease::from_plan(&plan));
    let result = tool_registry
        .execute("extensionEcho", arguments, &ctx)
        .await
        .unwrap();
    assert_eq!(result.content, "/workspace:hello");
    assert!(!result.is_error);
}

#[tokio::test]
async fn blocking_extension_returns_block_outcome() {
    let runner = extension_runner_with_extensions(
        Duration::from_secs(5),
        None,
        vec![Arc::new(AlwaysBlockExtension)],
    )
    .await
    .unwrap();

    let ctx = pre_tool_use_context("pwd");
    let result = runner.emit_pre_tool_use(ctx).await.unwrap();
    match result {
        PreToolUseAdmission::Block { reason } => {
            assert_eq!(reason, "blocked by AlwaysBlockExtension");
        },
        other => panic!("Expected Block, got {other:?}"),
    }
}

#[tokio::test]
async fn allow_extension_returns_allow_outcome() {
    let runner = extension_runner_with_extensions(
        Duration::from_secs(5),
        None,
        vec![Arc::new(SecurityExtension)],
    )
    .await
    .unwrap();

    let ctx = pre_tool_use_context("pwd");
    let result = runner.emit_pre_tool_use(ctx).await.unwrap();
    assert!(matches!(result, PreToolUseAdmission::Allow));
}

#[tokio::test]
async fn pre_tool_use_extension_can_inspect_tool_payload() {
    let runner = extension_runner_with_extensions(
        Duration::from_secs(5),
        None,
        vec![Arc::new(SecurityExtension)],
    )
    .await
    .unwrap();

    let ctx = pre_tool_use_context("rm -rf /");
    let result = runner.emit_pre_tool_use(ctx).await.unwrap();
    match result {
        PreToolUseAdmission::Block { reason } => {
            assert_eq!(reason, "dangerous shell command");
        },
        other => panic!("Expected Block, got {other:?}"),
    }
}

#[tokio::test]
async fn extension_context_snapshot_works_for_nonblocking() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let completed = Arc::new(Notify::new());
    let runner = extension_runner_with_extensions(
        Duration::from_secs(5),
        None,
        vec![Arc::new(FireAndForgetExt {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            completed: Arc::clone(&completed),
        })],
    )
    .await
    .unwrap();

    let ctx = lifecycle_context("test-session", "test-model");

    tokio::time::timeout(
        Duration::from_secs(1),
        runner.emit_lifecycle(
            astrcode_extension_sdk::extension::LifecycleEvent::TurnEnd,
            ctx,
        ),
    )
    .await
    .expect("non-blocking lifecycle dispatch must not await the handler")
    .unwrap();

    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .expect("non-blocking lifecycle handler should start");
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), completed.notified())
        .await
        .expect("non-blocking lifecycle handler should finish after release");
}

#[tokio::test]
async fn dispatch_with_no_registered_extensions_is_noop() {
    let runner = ExtensionRunner::new(Duration::from_secs(5));

    let ctx = lifecycle_context("empty", "noop");
    runner
        .emit_lifecycle(
            astrcode_extension_sdk::extension::LifecycleEvent::SessionStart,
            ctx,
        )
        .await
        .unwrap();

    let pre_ctx = pre_tool_use_context("pwd");
    let result = runner.emit_pre_tool_use(pre_ctx).await.unwrap();
    assert!(matches!(result, PreToolUseAdmission::Allow));
}

#[tokio::test]
async fn extension_subscribes_only_to_matching_events() {
    let runner = extension_runner_with_extensions(
        Duration::from_secs(5),
        None,
        vec![Arc::new(AlwaysBlockExtension)],
    )
    .await
    .unwrap();

    let lifecycle_ctx = lifecycle_context("test-session", "test-model");
    // SessionStart should pass through without blocking.
    runner
        .emit_lifecycle(
            astrcode_extension_sdk::extension::LifecycleEvent::SessionStart,
            lifecycle_ctx,
        )
        .await
        .unwrap();

    // PreToolUse should be blocked.
    let pre_ctx = pre_tool_use_context("pwd");
    let result = runner.emit_pre_tool_use(pre_ctx).await.unwrap();
    match result {
        PreToolUseAdmission::Block { reason } => {
            assert_eq!(reason, "blocked by AlwaysBlockExtension");
        },
        other => panic!("Expected Block, got {other:?}"),
    }
}
