//! Integration test: extensions can block tool execution via PreToolUse hooks.

use std::{sync::Arc, time::Duration};

use astrcode_core::{
    config::ModelSelection,
    extension::{
        Extension, ExtensionContext, ExtensionError, ExtensionEvent, HookEffect, HookMode,
        HookSubscription, PreToolUseInput,
    },
    tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult},
};
use astrcode_extensions::{
    context::ServerExtensionContext,
    runner::{ExtensionRunner, ToolHookOutcome},
    runtime::ExtensionRuntime,
};
use astrcode_tools::registry::ToolRegistry;

/// A test extension that blocks shell commands containing "rm -rf".
struct SecurityExtension;

#[async_trait::async_trait]
impl Extension for SecurityExtension {
    fn id(&self) -> &str {
        "test-security"
    }

    fn hook_subscriptions(&self) -> Vec<HookSubscription> {
        vec![HookSubscription {
            event: ExtensionEvent::PreToolUse,
            mode: HookMode::Blocking,
            priority: 0,
        }]
    }

    async fn on_event(
        &self,
        event: ExtensionEvent,
        ctx: &dyn ExtensionContext,
    ) -> Result<HookEffect, ExtensionError> {
        match event {
            ExtensionEvent::PreToolUse => {
                let input = ctx
                    .pre_tool_use_input()
                    .expect("PreToolUse context should include tool payload");
                ctx.log_warn(&format!(
                    "SecurityExtension checking {} in session {}",
                    input.tool_name,
                    ctx.session_id(),
                ));
                if input.tool_name == "shell"
                    && input
                        .tool_input
                        .get("command")
                        .and_then(|value| value.as_str())
                        .is_some_and(|command| command.contains("rm -rf"))
                {
                    return Ok(HookEffect::Block {
                        reason: "dangerous shell command".into(),
                    });
                }
                Ok(HookEffect::Allow)
            },
            _ => Ok(HookEffect::Allow),
        }
    }
}

/// A test extension that always blocks.
struct AlwaysBlockExtension;

#[async_trait::async_trait]
impl Extension for AlwaysBlockExtension {
    fn id(&self) -> &str {
        "test-always-block"
    }

    fn hook_subscriptions(&self) -> Vec<HookSubscription> {
        vec![HookSubscription {
            event: ExtensionEvent::PreToolUse,
            mode: HookMode::Blocking,
            priority: 0,
        }]
    }

    async fn on_event(
        &self,
        _event: ExtensionEvent,
        _ctx: &dyn ExtensionContext,
    ) -> Result<HookEffect, ExtensionError> {
        Ok(HookEffect::Block {
            reason: "blocked by AlwaysBlockExtension".into(),
        })
    }
}

struct EchoToolExtension;

#[async_trait::async_trait]
impl Extension for EchoToolExtension {
    fn id(&self) -> &str {
        "test-echo-tool"
    }

    fn hook_subscriptions(&self) -> Vec<HookSubscription> {
        vec![]
    }

    async fn on_event(
        &self,
        _event: ExtensionEvent,
        _ctx: &dyn ExtensionContext,
    ) -> Result<HookEffect, ExtensionError> {
        Ok(HookEffect::Allow)
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
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
        }]
    }

    async fn execute_tool(
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
            metadata: Default::default(),
            duration_ms: None,
        })
    }
}

struct FixedToolExtension {
    id: &'static str,
    tool_name: &'static str,
    content: &'static str,
}

#[async_trait::async_trait]
impl Extension for FixedToolExtension {
    fn id(&self) -> &str {
        self.id
    }

    fn hook_subscriptions(&self) -> Vec<HookSubscription> {
        vec![]
    }

    async fn on_event(
        &self,
        _event: ExtensionEvent,
        _ctx: &dyn ExtensionContext,
    ) -> Result<HookEffect, ExtensionError> {
        Ok(HookEffect::Allow)
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: self.tool_name.into(),
            description: format!("{} tool", self.id),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            origin: ToolOrigin::Extension,
            execution_mode: ExecutionMode::Sequential,
        }]
    }

    async fn execute_tool(
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
            content: self.content.into(),
            is_error: false,
            error: None,
            metadata: Default::default(),
            duration_ms: None,
        })
    }
}

/// Checks that the block outcome carries the expected reason.
fn assert_blocked(outcome: &ToolHookOutcome, expected_reason: &str) {
    match outcome {
        ToolHookOutcome::Blocked { reason } => {
            assert_eq!(reason, expected_reason);
        },
        other => panic!("Expected Blocked, got {other:?}"),
    }
}

fn assert_allow(outcome: &ToolHookOutcome) {
    match outcome {
        ToolHookOutcome::Allow => {},
        other => panic!("Expected Allow, got {other:?}"),
    }
}

fn context_with_pre_tool_input(command: &str) -> ServerExtensionContext {
    let mut ctx = ServerExtensionContext::new(
        "test-session".into(),
        "/tmp".into(),
        ModelSelection {
            profile_name: String::new(),
            model: "test-model".into(),
            provider_kind: String::new(),
        },
    );
    ctx.set_pre_tool_use_input(PreToolUseInput {
        tool_name: "shell".into(),
        tool_input: serde_json::json!({ "command": command }),
    });
    ctx
}

#[tokio::test]
async fn duplicate_extension_tools_keep_first_registration() {
    let runner = ExtensionRunner::new(Duration::from_secs(5), Arc::new(ExtensionRuntime::new()));
    runner
        .register(Arc::new(FixedToolExtension {
            id: "project",
            tool_name: "sharedTool",
            content: "project",
        }))
        .await;
    runner
        .register(Arc::new(FixedToolExtension {
            id: "global",
            tool_name: "sharedTool",
            content: "global",
        }))
        .await;

    let tools = runner.collect_tool_adapters("/workspace").await;
    let mut tool_registry = ToolRegistry::new();
    for tool in tools.into_iter().rev() {
        tool_registry.register(tool);
    }

    let ctx = astrcode_core::tool::ToolExecutionContext {
        session_id: "test".into(),
        working_dir: String::new(),
        model_id: String::new(),
        available_tools: vec![],
        tool_call_id: None,
        event_tx: None,
        tool_result_reader: None,
    };
    let result = tool_registry
        .execute("sharedTool", serde_json::json!({}), &ctx)
        .await
        .unwrap();

    assert_eq!(result.content, "project");
}

#[tokio::test]
async fn extension_registration_and_count() {
    let runner = ExtensionRunner::new(Duration::from_secs(5), Arc::new(ExtensionRuntime::new()));
    assert_eq!(runner.count().await, 0);

    runner.register(Arc::new(SecurityExtension)).await;
    assert_eq!(runner.count().await, 1);
}

#[tokio::test]
async fn extension_tools_are_adapted_into_tool_registry() {
    let runner = ExtensionRunner::new(Duration::from_secs(5), Arc::new(ExtensionRuntime::new()));
    runner.register(Arc::new(EchoToolExtension)).await;

    let tools = runner.collect_tool_adapters("/workspace").await;
    let mut tool_registry = ToolRegistry::new();
    for tool in tools.into_iter().rev() {
        tool_registry.register(tool);
    }

    let definitions = tool_registry.list_definitions();
    assert!(definitions.iter().any(|def| def.name == "extensionEcho"));

    let ctx = astrcode_core::tool::ToolExecutionContext {
        session_id: "test".into(),
        working_dir: String::new(),
        model_id: String::new(),
        available_tools: vec![],
        tool_call_id: None,
        event_tx: None,
        tool_result_reader: None,
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
    let runner = ExtensionRunner::new(Duration::from_secs(5), Arc::new(ExtensionRuntime::new()));
    runner.register(Arc::new(AlwaysBlockExtension)).await;

    let ctx = context_with_pre_tool_input("pwd");

    let outcome = runner
        .dispatch_tool_hook(ExtensionEvent::PreToolUse, &ctx)
        .await
        .unwrap();
    assert_blocked(&outcome, "blocked by AlwaysBlockExtension");
}

#[tokio::test]
async fn allow_extension_returns_allow_outcome() {
    let runner = ExtensionRunner::new(Duration::from_secs(5), Arc::new(ExtensionRuntime::new()));
    runner.register(Arc::new(SecurityExtension)).await;

    let ctx = context_with_pre_tool_input("pwd");

    let outcome = runner
        .dispatch_tool_hook(ExtensionEvent::PreToolUse, &ctx)
        .await
        .unwrap();
    assert_allow(&outcome);
}

#[tokio::test]
async fn pre_tool_use_extension_can_inspect_tool_payload() {
    let runner = ExtensionRunner::new(Duration::from_secs(5), Arc::new(ExtensionRuntime::new()));
    runner.register(Arc::new(SecurityExtension)).await;

    let ctx = context_with_pre_tool_input("rm -rf /");
    let outcome = runner
        .dispatch_tool_hook(ExtensionEvent::PreToolUse, &ctx)
        .await
        .unwrap();

    assert_blocked(&outcome, "dangerous shell command");
}

#[tokio::test]
async fn extension_context_snapshot_works_for_nonblocking() {
    let runner = ExtensionRunner::new(Duration::from_secs(5), Arc::new(ExtensionRuntime::new()));

    // A fire-and-forget extension
    struct FireAndForgetExt;
    #[async_trait::async_trait]
    impl Extension for FireAndForgetExt {
        fn id(&self) -> &str {
            "test-faf"
        }
        fn hook_subscriptions(&self) -> Vec<HookSubscription> {
            vec![HookSubscription {
                event: ExtensionEvent::TurnStart,
                mode: HookMode::NonBlocking,
                priority: 0,
            }]
        }
        async fn on_event(
            &self,
            _event: ExtensionEvent,
            ctx: &dyn ExtensionContext,
        ) -> Result<HookEffect, ExtensionError> {
            // NonBlocking hooks should still have session context
            assert_eq!(ctx.session_id(), "test-session");
            assert_eq!(ctx.working_dir(), "/tmp");
            Ok(HookEffect::Allow)
        }
    }

    runner.register(Arc::new(FireAndForgetExt)).await;

    let ctx = ServerExtensionContext::new(
        "test-session".into(),
        "/tmp".into(),
        ModelSelection {
            profile_name: String::new(),
            model: "test-model".into(),
            provider_kind: String::new(),
        },
    );

    // dispatch() copies the extension list and releases the lock,
    // then spawns NonBlocking with a snapshot context.
    runner
        .dispatch(ExtensionEvent::TurnStart, &ctx)
        .await
        .unwrap();

    // Give the fire-and-forget task a moment to run.
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[tokio::test]
async fn dispatch_with_no_registered_extensions_is_noop() {
    let runner = ExtensionRunner::new(Duration::from_secs(5), Arc::new(ExtensionRuntime::new()));
    let ctx = ServerExtensionContext::new(
        "empty".into(),
        "/tmp".into(),
        ModelSelection {
            profile_name: String::new(),
            model: "noop".into(),
            provider_kind: String::new(),
        },
    );

    // Should not error or panic
    runner
        .dispatch(ExtensionEvent::SessionStart, &ctx)
        .await
        .unwrap();
    let outcome = runner
        .dispatch_tool_hook(ExtensionEvent::PreToolUse, &ctx)
        .await
        .unwrap();
    assert_allow(&outcome);
}

#[tokio::test]
async fn extension_subscribes_only_to_matching_events() {
    let runner = ExtensionRunner::new(Duration::from_secs(5), Arc::new(ExtensionRuntime::new()));
    runner.register(Arc::new(AlwaysBlockExtension)).await;

    let ctx = context_with_pre_tool_input("pwd");

    // AlwaysBlockExtension only subscribes to PreToolUse.
    // SessionStart should pass through without blocking.
    runner
        .dispatch(ExtensionEvent::SessionStart, &ctx)
        .await
        .unwrap();

    // PreToolUse should be blocked.
    let outcome = runner
        .dispatch_tool_hook(ExtensionEvent::PreToolUse, &ctx)
        .await
        .unwrap();
    assert_blocked(&outcome, "blocked by AlwaysBlockExtension");
}
