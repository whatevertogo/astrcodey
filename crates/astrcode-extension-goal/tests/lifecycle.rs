use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use astrcode_extension_sdk::{
    extension::{
        ContinueAfterStopResult, ExtensionCapability, ExtensionCommandResult,
        PreparedProviderEffect, Registrar, ToolRegistration,
    },
    host::{
        ExtensionHost, HostError, HostOperation,
        internal::{HostInvoker, HostScope, extension_host},
    },
    testing::{CommandContextBuilder, HookContextBuilder, ToolContextBuilder},
};
use serde_json::{Value, json};

#[derive(Default)]
struct SessionUsage(AtomicU64);

#[async_trait::async_trait]
impl HostInvoker for SessionUsage {
    async fn invoke(&self, operation: HostOperation, _input: Value) -> Result<Value, HostError> {
        assert_eq!(operation, HostOperation::SessionHistoryTokenUsage);
        let total = self.0.load(Ordering::Relaxed);
        Ok(json!({ "usage": (total > 0).then(|| json!({ "total_tokens": total })) }))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn find_tool<'a>(tools: &'a [ToolRegistration], name: &str) -> &'a ToolRegistration {
    tools
        .iter()
        .find(|tool| tool.definition().name == name)
        .unwrap()
}

fn test_host() -> (Arc<SessionUsage>, ExtensionHost) {
    let usage = Arc::new(SessionUsage::default());
    let host = extension_host(
        usage.clone(),
        HostScope::new(
            [ExtensionCapability::SessionHistory],
            [HostOperation::SessionHistoryTokenUsage],
            true,
            true,
        ),
    );
    (usage, host)
}

#[tokio::test]
async fn registered_handlers_deliver_continuation_and_budget_wrap_up_on_demand() {
    let extension = astrcode_extension_goal::extension();
    let mut registrar = Registrar::new();
    extension.register(&mut registrar);
    let (_, registrations) = registrar.finish(extension.manifest()).unwrap();
    assert!(registrations.provider().is_empty());

    let (usage, host) = test_host();
    let session = tempfile::tempdir().unwrap();
    let id = extension.manifest().id().to_owned();
    let hooks = || {
        HookContextBuilder::new(&id)
            .session("s", ".", Some(session.path().into()))
            .host(host.clone())
            .turn_id("turn")
    };
    let command = |arg: &str| {
        CommandContextBuilder::new(&id, "goal")
            .session("s", ".", Some(session.path().into()))
            .host(host.clone())
            .argument(arg)
            .build()
    };

    let create = find_tool(registrations.tools(), "createGoal");
    let result = create
        .handler()
        .execute(
            ToolContextBuilder::new(&id, "createGoal")
                .session("s", ".", Some(session.path().into()))
                .host(host.clone())
                .arguments(json!({"objective": "Finish work", "tokenBudget": 100}))
                .build(),
        )
        .await
        .unwrap()
        .into_parts()
        .0;
    assert!(!result.is_error);

    let provider = &registrations.provider_contributions()[0].1;
    let prepare = || provider.prepare(hooks().build_provider(vec![]));
    let stop = || {
        registrations.continue_after_stop()[0]
            .handler
            .handle(hooks().build_continue_after_stop("progress", "stop", 0))
    };
    let slash = registrations.commands()[0].1.as_ref().unwrap();

    assert!(prepare().await.unwrap().is_none());

    usage.0.store(25, Ordering::Relaxed);
    assert_eq!(
        stop().await.unwrap(),
        ContinueAfterStopResult::ContinueOneStep
    );

    let (continuation, effect) = prepare().await.unwrap().unwrap().into_parts();
    assert!(matches!(
        effect,
        PreparedProviderEffect::AppendMessages(messages)
            if messages[0].content[0].to_display_text().contains("Tokens remaining: 75")
                && messages[0].content[0].to_display_text().contains("Continuation step: 1")
                && messages[0].content[0].to_display_text().contains("todoWrite")
    ));
    assert_eq!(
        prepare().await.unwrap().unwrap().into_parts().0,
        continuation
    );

    assert!(
        provider
            .prepare(hooks().turn_id("new-user-turn").build_provider(vec![]))
            .await
            .unwrap()
            .is_none()
    );

    usage.0.store(105, Ordering::Relaxed);
    let (limit, effect) = prepare().await.unwrap().unwrap().into_parts();
    assert_ne!(limit, continuation);
    assert!(matches!(
        effect,
        PreparedProviderEffect::AppendMessages(messages)
            if messages[0].content[0].to_display_text().contains("budget_limited")
                && messages[0]
                    .content[0]
                    .to_display_text()
                    .contains("/goal budget <new_total_budget>")
    ));
    assert_eq!(prepare().await.unwrap().unwrap().into_parts().0, limit);
    assert!(
        provider
            .prepare(hooks().turn_id("new-user-turn").build_provider(vec![]))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(stop().await.unwrap(), ContinueAfterStopResult::EndTurn);

    assert!(matches!(
        slash.execute(command("resume")).await.unwrap(),
        ExtensionCommandResult::Display { is_error: true, .. }
    ));
    assert!(matches!(
        slash.execute(command("budget")).await.unwrap(),
        ExtensionCommandResult::Display { content, is_error: true, .. }
            if content.contains("usage: /goal budget")
    ));
    assert!(matches!(
        slash.execute(command("budget 150")).await.unwrap(),
        ExtensionCommandResult::StartTurn { instructions }
            if instructions.contains("45 tokens remaining")
    ));
    assert!(prepare().await.unwrap().is_none());
    assert!(matches!(
        slash.execute(command("complete")).await.unwrap(),
        ExtensionCommandResult::Display { content, is_error: false, .. }
            if content.contains("105/150")
    ));
    assert_eq!(stop().await.unwrap(), ContinueAfterStopResult::EndTurn);
}

#[tokio::test]
async fn continue_after_stop_stops_on_refusal() {
    let extension = astrcode_extension_goal::extension();
    let mut registrar = Registrar::new();
    extension.register(&mut registrar);
    let (_, registrations) = registrar.finish(extension.manifest()).unwrap();

    let (_, host) = test_host();
    let session = tempfile::tempdir().unwrap();
    let id = extension.manifest().id().to_owned();

    let create = find_tool(registrations.tools(), "createGoal");
    let created = create
        .handler()
        .execute(
            ToolContextBuilder::new(&id, "createGoal")
                .session("s", ".", Some(session.path().into()))
                .host(host.clone())
                .arguments(json!({"objective": "Attempt task"}))
                .build(),
        )
        .await
        .unwrap()
        .into_parts()
        .0;
    assert!(!created.is_error);

    let hooks = || {
        HookContextBuilder::new(&id)
            .session("s", ".", Some(session.path().into()))
            .host(host.clone())
            .turn_id("turn-1")
    };
    let decision = registrations.continue_after_stop()[0]
        .handler
        .handle(hooks().build_continue_after_stop("cannot comply", "refusal", 0))
        .await
        .unwrap();
    assert_eq!(decision, ContinueAfterStopResult::EndTurn);

    let provider = &registrations.provider_contributions()[0].1;
    assert!(
        provider
            .prepare(hooks().build_provider(vec![]))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn update_goal_pause_and_resume_through_handlers() {
    let extension = astrcode_extension_goal::extension();
    let mut registrar = Registrar::new();
    extension.register(&mut registrar);
    let (_, registrations) = registrar.finish(extension.manifest()).unwrap();

    let (usage, host) = test_host();
    let session = tempfile::tempdir().unwrap();
    let id = extension.manifest().id().to_owned();
    let tool_ctx = |name: &str, args: Value| {
        ToolContextBuilder::new(&id, name)
            .session("s", ".", Some(session.path().into()))
            .host(host.clone())
            .arguments(args)
            .build()
    };

    let create = find_tool(registrations.tools(), "createGoal");
    let get = find_tool(registrations.tools(), "getGoal");
    let update = find_tool(registrations.tools(), "updateGoal");
    let slash = registrations.commands()[0].1.as_ref().unwrap();

    assert!(
        !create
            .handler()
            .execute(tool_ctx(
                "createGoal",
                json!({"objective": "Finish work", "tokenBudget": 100})
            ))
            .await
            .unwrap()
            .into_parts()
            .0
            .is_error
    );

    usage.0.store(30, Ordering::Relaxed);
    let paused = update
        .handler()
        .execute(tool_ctx("updateGoal", json!({"status": "paused"})))
        .await
        .unwrap()
        .into_parts()
        .0;
    assert!(!paused.is_error);
    assert!(paused.content.contains("paused"));

    // Tokens spent while paused must not reduce the remaining goal budget.
    usage.0.store(80, Ordering::Relaxed);
    let report = get
        .handler()
        .execute(tool_ctx("getGoal", json!({})))
        .await
        .unwrap()
        .into_parts()
        .0;
    assert!(report.content.contains("Status: paused"));
    assert!(report.content.contains("Tokens: 30/100 (70 remaining)"));

    let resumed = slash
        .execute(
            CommandContextBuilder::new(&id, "goal")
                .session("s", ".", Some(session.path().into()))
                .host(host.clone())
                .argument("resume")
                .build(),
        )
        .await
        .unwrap();
    assert!(matches!(
        resumed,
        ExtensionCommandResult::StartTurn { instructions }
            if instructions.contains("70/100 budget tokens remaining")
    ));

    usage.0.store(90, Ordering::Relaxed);
    let report = get
        .handler()
        .execute(tool_ctx("getGoal", json!({})))
        .await
        .unwrap()
        .into_parts()
        .0;
    assert!(report.content.contains("Status: active"));
    assert!(report.content.contains("Tokens: 40/100 (60 remaining)"));

    let blocked = update
        .handler()
        .execute(tool_ctx("updateGoal", json!({"status": "blocked"})))
        .await
        .unwrap()
        .into_parts()
        .0;
    assert!(!blocked.is_error);

    let resumed_from_blocked = slash
        .execute(
            CommandContextBuilder::new(&id, "goal")
                .session("s", ".", Some(session.path().into()))
                .host(host.clone())
                .argument("resume")
                .build(),
        )
        .await
        .unwrap();
    assert!(matches!(
        resumed_from_blocked,
        ExtensionCommandResult::StartTurn { instructions }
            if instructions.contains("fresh blocked audit")
    ));
}
