//! Integration test: extensions can block tool execution via PreToolUse hooks.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use astrcode_core::{
    extension::{
        Extension, ExtensionError, HookMode, HookResult, LifecycleContext, PreToolUseContext,
        PreToolUseResult, Registrar, ToolHandler,
    },
    tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult},
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_tools::registry::ToolRegistry;

// ─── Test extensions using register() ─────────────────────────────────────

struct SecurityExtension;

impl Extension for SecurityExtension {
    fn id(&self) -> &str {
        "test-security"
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_pre_tool_use(HookMode::Blocking, 0, Arc::new(SecurityHandler));
    }
}

struct SecurityHandler;

#[async_trait::async_trait]
impl astrcode_core::extension::PreToolUseHandler for SecurityHandler {
    async fn handle(&self, ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError> {
        if ctx.tool_name == "shell"
            && ctx
                .tool_input
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
    fn id(&self) -> &str {
        "test-always-block"
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_pre_tool_use(HookMode::Blocking, 0, Arc::new(AlwaysBlockHandler));
    }
}

struct AlwaysBlockHandler;

#[async_trait::async_trait]
impl astrcode_core::extension::PreToolUseHandler for AlwaysBlockHandler {
    async fn handle(&self, _ctx: PreToolUseContext) -> Result<PreToolUseResult, ExtensionError> {
        Ok(PreToolUseResult::Block {
            reason: "blocked by AlwaysBlockExtension".into(),
        })
    }
}

struct EchoToolExtension;

impl Extension for EchoToolExtension {
    fn id(&self) -> &str {
        "test-echo-tool"
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
                origin: ToolOrigin::Extension,
                execution_mode: ExecutionMode::Sequential,
            },
            Arc::new(EchoToolHandler),
        );
    }
}

struct EchoToolHandler;

#[async_trait::async_trait]
impl ToolHandler for EchoToolHandler {
    async fn execute(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
        working_dir: &str,
        _ctx: &astrcode_core::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        if tool_name != "extensionEcho" {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }
        let text = arguments
            .get("text")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        Ok(ToolResult {
            call_id: String::new(),
            content: format!("{working_dir}:{text}"),
            is_error: false,
            error: None,
            metadata: BTreeMap::new(),
            duration_ms: None,
        })
    }
}

struct FixedToolExtension {
    id: &'static str,
    tool_name: &'static str,
    content: &'static str,
}

impl Extension for FixedToolExtension {
    fn id(&self) -> &str {
        self.id
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
                origin: ToolOrigin::Extension,
                execution_mode: ExecutionMode::Sequential,
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
    async fn execute(
        &self,
        tool_name: &str,
        _arguments: serde_json::Value,
        _working_dir: &str,
        _ctx: &astrcode_core::tool::ToolExecutionContext,
    ) -> Result<ToolResult, ExtensionError> {
        if tool_name != self.tool_name {
            return Err(ExtensionError::NotFound(tool_name.into()));
        }
        Ok(ToolResult {
            call_id: String::new(),
            content: self.content.clone(),
            is_error: false,
            error: None,
            metadata: BTreeMap::new(),
            duration_ms: None,
        })
    }
}

// ─── Lifecycle extension for NonBlocking test ─────────────────────────────

struct FireAndForgetExt;

impl Extension for FireAndForgetExt {
    fn id(&self) -> &str {
        "test-faf"
    }

    fn register(&self, reg: &mut Registrar) {
        reg.on_event(
            astrcode_core::extension::ExtensionEvent::TurnStart,
            HookMode::NonBlocking,
            0,
            Arc::new(FafHandler),
        );
    }
}

struct FafHandler;

#[async_trait::async_trait]
impl astrcode_core::extension::LifecycleHandler for FafHandler {
    async fn handle(&self, ctx: LifecycleContext) -> Result<HookResult, ExtensionError> {
        assert_eq!(ctx.session_id, "test-session");
        assert_eq!(ctx.working_dir, "/tmp");
        Ok(HookResult::Allow)
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────

fn pre_tool_use_context(command: &str) -> PreToolUseContext {
    PreToolUseContext {
        session_id: "test-session".into(),
        working_dir: "/tmp".into(),
        model: astrcode_core::config::ModelSelection::simple("test-model"),
        tool_name: "shell".into(),
        tool_input: serde_json::json!({ "command": command }),
        available_tools: vec![],
        event_tx: None,
        extension_event_sink: None,
        session_store_dir: None,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn duplicate_extension_tools_keep_first_registration() {
    let runner = ExtensionRunner::new(Duration::from_secs(5));
    runner
        .register(Arc::new(FixedToolExtension {
            id: "project",
            tool_name: "sharedTool",
            content: "project",
        }))
        .await
        .unwrap();
    runner
        .register(Arc::new(FixedToolExtension {
            id: "global",
            tool_name: "sharedTool",
            content: "global",
        }))
        .await
        .unwrap();

    let tools = runner.collect_tool_adapters_typed("/workspace").await;
    let mut tool_registry = ToolRegistry::new();
    for tool in tools.into_iter().rev() {
        tool_registry.register(tool);
    }

    let ctx = astrcode_core::tool::ToolExecutionContext {
        session_id: "test".into(),
        working_dir: String::new(),
        tool_call_id: None,
        event_tx: None,
        capabilities: Default::default(),
    };
    let result = tool_registry
        .execute("sharedTool", serde_json::json!({}), &ctx)
        .await
        .unwrap();

    assert_eq!(result.content, "project");
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
    let runner = ExtensionRunner::new(Duration::from_secs(5));
    runner.register(Arc::new(EchoToolExtension)).await.unwrap();

    let tools = runner.collect_tool_adapters_typed("/workspace").await;
    let mut tool_registry = ToolRegistry::new();
    for tool in tools.into_iter().rev() {
        tool_registry.register(tool);
    }

    let definitions = tool_registry.list_definitions();
    assert!(definitions.iter().any(|def| def.name == "extensionEcho"));

    let ctx = astrcode_core::tool::ToolExecutionContext {
        session_id: "test".into(),
        working_dir: String::new(),
        tool_call_id: None,
        event_tx: None,
        capabilities: Default::default(),
    };
    let result = tool_registry
        .execute(
            "extensionEcho",
            serde_json::json!({ "text": "hello" }),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(result.content, "/workspace:hello");
    assert!(!result.is_error);
}

#[tokio::test]
async fn blocking_extension_returns_block_outcome() {
    let runner = ExtensionRunner::new(Duration::from_secs(5));
    runner
        .register(Arc::new(AlwaysBlockExtension))
        .await
        .unwrap();

    let ctx = pre_tool_use_context("pwd");
    let result = runner.emit_pre_tool_use(ctx).await.unwrap();
    match result {
        PreToolUseResult::Block { reason } => {
            assert_eq!(reason, "blocked by AlwaysBlockExtension");
        },
        other => panic!("Expected Block, got {other:?}"),
    }
}

#[tokio::test]
async fn allow_extension_returns_allow_outcome() {
    let runner = ExtensionRunner::new(Duration::from_secs(5));
    runner.register(Arc::new(SecurityExtension)).await.unwrap();

    let ctx = pre_tool_use_context("pwd");
    let result = runner.emit_pre_tool_use(ctx).await.unwrap();
    assert!(matches!(result, PreToolUseResult::Allow));
}

#[tokio::test]
async fn pre_tool_use_extension_can_inspect_tool_payload() {
    let runner = ExtensionRunner::new(Duration::from_secs(5));
    runner.register(Arc::new(SecurityExtension)).await.unwrap();

    let ctx = pre_tool_use_context("rm -rf /");
    let result = runner.emit_pre_tool_use(ctx).await.unwrap();
    match result {
        PreToolUseResult::Block { reason } => {
            assert_eq!(reason, "dangerous shell command");
        },
        other => panic!("Expected Block, got {other:?}"),
    }
}

#[tokio::test]
async fn extension_context_snapshot_works_for_nonblocking() {
    let runner = ExtensionRunner::new(Duration::from_secs(5));
    runner.register(Arc::new(FireAndForgetExt)).await.unwrap();

    let ctx = LifecycleContext {
        session_id: "test-session".into(),
        working_dir: "/tmp".into(),
        model: astrcode_core::config::ModelSelection::simple("test-model"),
        event_tx: None,
        extension_event_sink: None,
        last_exchange: None,
    };

    runner
        .emit_lifecycle(astrcode_core::extension::ExtensionEvent::TurnStart, ctx)
        .await
        .unwrap();

    // Give the fire-and-forget task a moment to run.
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[tokio::test]
async fn dispatch_with_no_registered_extensions_is_noop() {
    let runner = ExtensionRunner::new(Duration::from_secs(5));

    let ctx = LifecycleContext {
        session_id: "empty".into(),
        working_dir: "/tmp".into(),
        model: astrcode_core::config::ModelSelection::simple("noop"),
        event_tx: None,
        extension_event_sink: None,
        last_exchange: None,
    };
    runner
        .emit_lifecycle(astrcode_core::extension::ExtensionEvent::SessionStart, ctx)
        .await
        .unwrap();

    let pre_ctx = pre_tool_use_context("pwd");
    let result = runner.emit_pre_tool_use(pre_ctx).await.unwrap();
    assert!(matches!(result, PreToolUseResult::Allow));
}

#[tokio::test]
async fn extension_subscribes_only_to_matching_events() {
    let runner = ExtensionRunner::new(Duration::from_secs(5));
    runner
        .register(Arc::new(AlwaysBlockExtension))
        .await
        .unwrap();

    let lifecycle_ctx = LifecycleContext {
        session_id: "test-session".into(),
        working_dir: "/tmp".into(),
        model: astrcode_core::config::ModelSelection::simple("test-model"),
        event_tx: None,
        extension_event_sink: None,
        last_exchange: None,
    };
    // SessionStart should pass through without blocking.
    runner
        .emit_lifecycle(
            astrcode_core::extension::ExtensionEvent::SessionStart,
            lifecycle_ctx,
        )
        .await
        .unwrap();

    // PreToolUse should be blocked.
    let pre_ctx = pre_tool_use_context("pwd");
    let result = runner.emit_pre_tool_use(pre_ctx).await.unwrap();
    match result {
        PreToolUseResult::Block { reason } => {
            assert_eq!(reason, "blocked by AlwaysBlockExtension");
        },
        other => panic!("Expected Block, got {other:?}"),
    }
}
