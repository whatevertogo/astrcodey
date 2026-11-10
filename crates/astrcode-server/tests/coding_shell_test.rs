#![cfg(unix)]

use std::{collections::BTreeMap, path::Path, time::Duration};

use astrcode_core::{
    tool::{
        ToolCapabilities, ToolExecutionContext, ToolPlanningContext, ToolResult,
        access::ResourceLease,
    },
    types::SessionId,
};
use astrcode_extensions::{runner::ExtensionRunner, testing::extension_runner_with_extensions};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

async fn invoke(
    runner: &ExtensionRunner,
    workspace: &Path,
    name: &str,
    arguments: Value,
    cancellation: CancellationToken,
) -> ToolResult {
    let working_dir = workspace.to_string_lossy().into_owned();
    let tool = runner
        .tool_catalog_snapshot_typed(&working_dir)
        .await
        .tools
        .into_iter()
        .find(|tool| tool.definition().name == name)
        .unwrap();
    let call_id = Some(uuid::Uuid::new_v4().to_string());
    let planning =
        ToolPlanningContext::new(SessionId::new("shell-test"), &working_dir, call_id.clone());
    let plan = tool.plan(&arguments, &planning).await.unwrap();
    let execution = ToolExecutionContext::new(
        SessionId::new("shell-test"),
        working_dir,
        call_id,
        None,
        ToolCapabilities::default(),
    )
    .with_cancellation(cancellation)
    .with_resource_lease(ResourceLease::from_plan(&plan));
    tool.execute(arguments, &execution)
        .await
        .unwrap()
        .into_parts()
        .0
}

#[tokio::test]
async fn shell_yields_without_restarting_and_collects_terminal_output() {
    let workspace = tempfile::tempdir().unwrap();
    let extension = astrcode_bundled_extensions::bundled_extensions(&BTreeMap::new())
        .into_iter()
        .find(|extension| extension.manifest().id() == "astrcode-coding")
        .unwrap();
    let runner = extension_runner_with_extensions(Duration::from_secs(5), None, vec![extension])
        .await
        .unwrap();

    let invocation = CancellationToken::new();
    let started = invoke(
        &runner, workspace.path(), "shell",
        json!({"command": "printf x >> executions; printf first; sleep 0.15; printf last; exit 7", "yieldTimeMs": 0}),
        invocation.clone(),
    ).await;
    assert!(!started.is_error, "{started:?}");
    assert_eq!(started.metadata["executionStatus"], "running");
    assert!(started.metadata["timeoutSecs"].is_null());
    invocation.cancel();
    let completed = invoke(
        &runner,
        workspace.path(),
        "shell_poll",
        json!({"shellId": started.metadata["shellId"], "blockUntilMs": 3000}),
        CancellationToken::new(),
    )
    .await;
    assert!(completed.is_error);
    assert_eq!(completed.metadata["exitCode"], 7);
    assert_eq!(completed.metadata["executionStatus"], "failed");
    assert!(completed.metadata["shellId"].is_null());
    assert!(completed.content.contains("do not poll"));
    let output = format!("{}{}", started.content, completed.content);
    assert!(output.contains("first"));
    assert!(output.contains("last"));
    assert_eq!(
        std::fs::read(workspace.path().join("executions")).unwrap(),
        b"x"
    );

    let foreground = invoke(
        &runner,
        workspace.path(),
        "shell",
        json!({"command": "printf first; sleep 0.05; printf last", "yieldTimeMs": 3000}),
        CancellationToken::new(),
    )
    .await;
    assert!(!foreground.is_error);
    assert_eq!(foreground.metadata["executionStatus"], "succeeded");
    assert!(foreground.content.contains("firstlast"));

    let background = invoke(
        &runner,
        workspace.path(),
        "shell",
        json!({"command": "sleep 10", "runInBackground": true, "timeout": 1}),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(background.metadata["executionStatus"], "running");
    assert_eq!(background.metadata["timeoutSecs"], 1);
    let timed_out = invoke(
        &runner,
        workspace.path(),
        "shell_poll",
        json!({"shellId": background.metadata["shellId"], "blockUntilMs": 5000}),
        CancellationToken::new(),
    )
    .await;
    assert!(timed_out.is_error);
    assert_eq!(timed_out.metadata["executionStatus"], "timed_out");
    assert_eq!(timed_out.metadata["timedOut"], true);
    assert!(timed_out.metadata["shellId"].is_null());

    let cancellation = CancellationToken::new();
    let execution = invoke(
        &runner,
        workspace.path(),
        "shell",
        json!({"command": "sleep 10", "yieldTimeMs": 3000}),
        cancellation.clone(),
    );
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancellation.cancel();
    };
    let (cancelled, ()) = tokio::join!(execution, cancel);
    assert!(cancelled.is_error);
    assert!(!cancelled.metadata.contains_key("shellId"));
    assert!(runner.shutdown().await.is_empty());
}
