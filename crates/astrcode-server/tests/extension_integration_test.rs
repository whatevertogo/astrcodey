//! Integration test: extensions can block tool execution via PreToolUse hooks.

use std::{sync::Arc, time::Duration};

use astrcode_core::{
    config::ModelSelection,
    extension::{
        Extension, ExtensionContext, ExtensionError, ExtensionEvent, HookEffect, HookMode,
        PreToolUseInput,
    },
};
use astrcode_extensions::{
    context::ServerExtensionContext,
    runner::{ExtensionRunner, ToolHookOutcome},
};

/// A test extension that blocks shell commands containing "rm -rf".
struct SecurityExtension;

#[async_trait::async_trait]
impl Extension for SecurityExtension {
    fn id(&self) -> &str {
        "test-security"
    }

    fn subscriptions(&self) -> Vec<(ExtensionEvent, HookMode)> {
        vec![(ExtensionEvent::PreToolUse, HookMode::Blocking)]
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

    fn subscriptions(&self) -> Vec<(ExtensionEvent, HookMode)> {
        vec![(ExtensionEvent::PreToolUse, HookMode::Blocking)]
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
async fn extension_registration_and_count() {
    let runner = ExtensionRunner::new(Duration::from_secs(5));
    assert_eq!(runner.count().await, 0);

    runner.register(Arc::new(SecurityExtension)).await;
    assert_eq!(runner.count().await, 1);
}

#[tokio::test]
async fn blocking_extension_returns_block_outcome() {
    let runner = ExtensionRunner::new(Duration::from_secs(5));
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
    let runner = ExtensionRunner::new(Duration::from_secs(5));
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
    let runner = ExtensionRunner::new(Duration::from_secs(5));
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
    let runner = ExtensionRunner::new(Duration::from_secs(5));

    // A fire-and-forget extension
    struct FireAndForgetExt;
    #[async_trait::async_trait]
    impl Extension for FireAndForgetExt {
        fn id(&self) -> &str {
            "test-faf"
        }
        fn subscriptions(&self) -> Vec<(ExtensionEvent, HookMode)> {
            vec![(ExtensionEvent::TurnStart, HookMode::NonBlocking)]
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
    let runner = ExtensionRunner::new(Duration::from_secs(5));
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
    let runner = ExtensionRunner::new(Duration::from_secs(5));
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
