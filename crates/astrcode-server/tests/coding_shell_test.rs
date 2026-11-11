#![cfg(unix)]

use std::{
    collections::BTreeMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

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

async fn coding_runner() -> Arc<ExtensionRunner> {
    let extension = astrcode_bundled_extensions::bundled_extensions(&BTreeMap::new())
        .into_iter()
        .find(|extension| extension.manifest().id() == "astrcode-coding")
        .unwrap();
    extension_runner_with_extensions(Duration::from_secs(5), None, vec![extension])
        .await
        .unwrap()
}

#[tokio::test]
async fn shell_yields_without_restarting_and_collects_terminal_output() {
    let workspace = tempfile::tempdir().unwrap();
    let runner = coding_runner().await;

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

    let execution_started = Instant::now();
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
    let short_poll = invoke(
        &runner,
        workspace.path(),
        "shell_poll",
        json!({"shellId": background.metadata["shellId"], "blockUntilMs": 100}),
        CancellationToken::new(),
    )
    .await;
    assert_eq!(short_poll.metadata["executionStatus"], "running");
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
    assert!(execution_started.elapsed() < Duration::from_secs(4));

    let cancellation = CancellationToken::new();
    let execution = invoke(
        &runner,
        workspace.path(),
        "shell",
        json!({"command": "printf '%s' $$ > cancelled.pid; exec sleep 10", "yieldTimeMs": 3000}),
        cancellation.clone(),
    );
    let cancel = async {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !std::fs::read_to_string(workspace.path().join("cancelled.pid"))
                .is_ok_and(|pid| pid.parse::<u32>().is_ok())
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        cancellation.cancel();
    };
    let (cancelled, ()) = tokio::join!(execution, cancel);
    assert!(cancelled.is_error);
    assert!(!cancelled.metadata.contains_key("shellId"));
    let pid = std::fs::read_to_string(workspace.path().join("cancelled.pid")).unwrap();
    let pid: u32 = pid.parse().unwrap();
    tokio::time::timeout(Duration::from_secs(4), async {
        while tokio::process::Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .output()
            .await
            .unwrap()
            .status
            .success()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancellation must terminate and reap the child before runner cleanup");
    assert!(runner.shutdown().await.is_empty());
}

#[tokio::test]
async fn shell_poll_keeps_partial_output_on_cancellation_and_excludes_overlapping_polls() {
    let workspace = tempfile::tempdir().unwrap();
    let runner = coding_runner().await;
    let started = invoke(
        &runner, workspace.path(), "shell",
        json!({"command": "sleep 0.05; printf preserved; sleep 0.4; printf tail", "yieldTimeMs": 0}),
        CancellationToken::new(),
    ).await;
    let cancellation = CancellationToken::new();
    let poll = invoke(
        &runner,
        workspace.path(),
        "shell_poll",
        json!({"shellId": started.metadata["shellId"], "blockUntilMs": 3000}),
        cancellation.clone(),
    );
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        cancellation.cancel();
    };
    let (interrupted, ()) = tokio::join!(poll, cancel);
    assert!(interrupted.is_error);
    assert_eq!(interrupted.metadata["errorCode"], "cancelled");
    assert!(interrupted.content.contains("preserved"), "{interrupted:?}");
    let completed = invoke(
        &runner,
        workspace.path(),
        "shell_poll",
        json!({"shellId": started.metadata["shellId"], "blockUntilMs": 3000}),
        CancellationToken::new(),
    )
    .await;
    assert!(!completed.is_error, "{completed:?}");
    assert!(completed.content.contains("tail"));

    let started = invoke(
        &runner,
        workspace.path(),
        "shell",
        json!({"command": "sleep 0.15; printf done", "yieldTimeMs": 0}),
        CancellationToken::new(),
    )
    .await;
    let first = invoke(
        &runner,
        workspace.path(),
        "shell_poll",
        json!({"shellId": started.metadata["shellId"], "blockUntilMs": 3000}),
        CancellationToken::new(),
    );
    let second = invoke(
        &runner,
        workspace.path(),
        "shell_poll",
        json!({"shellId": started.metadata["shellId"], "blockUntilMs": 0}),
        CancellationToken::new(),
    );
    let (completed, busy) = tokio::join!(first, second);
    assert!(!completed.is_error, "{completed:?}");
    assert!(completed.metadata["shellId"].is_null());
    assert_eq!(busy.metadata["errorCode"], "peer_busy");
    assert_eq!(busy.metadata["retryable"], true);
    assert!(runner.shutdown().await.is_empty());
}
