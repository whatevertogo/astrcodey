use super::*;

#[test]
fn command_description_shortens_long_input() {
    let long = "a".repeat(100);
    let t = command_description(&long);
    assert!(t.chars().count() <= 80);
    assert!(t.ends_with('…'));
}

#[test]
fn command_description_preserves_short_input() {
    let short = "echo hello";
    assert_eq!(command_description(short), short);
}

#[test]
fn background_output_dir_uses_store_dir_when_provided() {
    let store_dir = PathBuf::from("/data/sessions/abc");
    let dir = background_output_dir(Some(&store_dir), Path::new("/tmp"));
    assert_eq!(dir, PathBuf::from("/data/sessions/abc/background-shells"));
}

#[test]
fn background_output_dir_falls_back_to_cwd_astrcode() {
    let dir = background_output_dir(None, Path::new("/tmp"));
    assert_eq!(dir, PathBuf::from("/tmp/.astrcode/background-shells"));
}

#[test]
fn format_footer_contains_status() {
    let footer = format_footer(ShellRunStatus::Completed, Some(0));
    assert!(footer.contains("completed"));
    assert!(footer.contains("exit_code: 0"));

    let footer = format_footer(ShellRunStatus::TimedOut, None);
    assert!(footer.contains("timed_out"));
    assert!(!footer.contains("exit_code"));
}

#[test]
fn preview_output_by_tokens_keeps_small_output() {
    let preview = preview_output_by_tokens("short output", Some(256));

    assert_eq!(preview.content, "short output");
    assert!(!preview.truncated);
    assert_eq!(preview.omitted_tokens, 0);
    assert_eq!(preview.max_tokens, 256);
}

#[test]
fn preview_output_by_tokens_uses_head_tail_when_large() {
    let content = format!(
        "{}{}{}",
        "HEAD-".repeat(300),
        "MIDDLE-".repeat(2_000),
        "TAIL-".repeat(300)
    );

    let preview = preview_output_by_tokens(&content, Some(256));

    assert!(preview.truncated);
    assert!(preview.content.contains("HEAD-"));
    assert!(preview.content.contains("TAIL-"));
    assert!(preview.content.contains("omitted about"));
    assert!(preview.content.contains("output file"));
    assert!(preview.omitted_tokens > 0);
    assert!(preview.returned_tokens <= preview.max_tokens + 64);
}

#[test]
fn normalize_status_output_max_tokens_clamps_bounds() {
    assert_eq!(normalize_status_output_max_tokens(Some(1)), 256);
    assert_eq!(
        normalize_status_output_max_tokens(Some(MAX_STATUS_OUTPUT_MAX_TOKENS + 1)),
        MAX_STATUS_OUTPUT_MAX_TOKENS
    );
}

/// Registry cleanup removes entries for the target session and kills running shells.
/// This test uses a short-lived command so the shell exits naturally.
#[tokio::test]
async fn cleanup_session_removes_shells_and_kills_running() {
    let registry = BackgroundShellRegistry {
        shells: Mutex::new(HashMap::new()),
    };

    let temp = tempfile::tempdir().unwrap();
    let shell = astrcode_support::shell::resolve_shell();
    let echo_cmd = match shell.family {
        astrcode_support::shell::ShellFamily::PowerShell => "Write-Output hello".into(),
        astrcode_support::shell::ShellFamily::Cmd => "echo hello".into(),
        _ => "echo hello".into(),
    };

    let spawned = registry
        .spawn(BackgroundShellSpawnParams {
            session_id: "sess-1".into(),
            command: echo_cmd,
            intent: None,
            cwd: temp.path().to_path_buf(),
            shell,
            timeout_secs: 10,
            store_dir: None,
        })
        .await
        .expect("spawn should succeed");

    // Give the shell time to complete so kill_record's start_kill doesn't race.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    assert_eq!(registry.shells.lock().len(), 1);
    registry.cleanup_session("sess-1");
    assert!(registry.shells.lock().is_empty());

    // Output file should still exist (not deleted, just record removed).
    assert!(spawned.output_path.exists());
}

/// cleanup_session only targets the specified session.
#[tokio::test]
async fn cleanup_session_is_scoped_to_target_session() {
    let registry = BackgroundShellRegistry {
        shells: Mutex::new(HashMap::new()),
    };

    let temp = tempfile::tempdir().unwrap();
    let shell = astrcode_support::shell::resolve_shell();
    let echo_cmd = match shell.family {
        astrcode_support::shell::ShellFamily::PowerShell => "Write-Output hi".into(),
        astrcode_support::shell::ShellFamily::Cmd => "echo hi".into(),
        _ => "echo hi".into(),
    };

    let params = BackgroundShellSpawnParams {
        session_id: "sess-other".into(),
        command: echo_cmd,
        intent: None,
        cwd: temp.path().to_path_buf(),
        shell,
        timeout_secs: 10,
        store_dir: None,
    };

    let _spawned = registry.spawn(params).await.expect("spawn should succeed");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    assert_eq!(registry.shells.lock().len(), 1);
    registry.cleanup_session("sess-different");
    assert_eq!(registry.shells.lock().len(), 1);
}

#[tokio::test]
async fn wait_background_shell_allows_repeated_terminal_poll_without_error() {
    let temp = tempfile::tempdir().unwrap();
    let shell = astrcode_support::shell::resolve_shell();
    let echo_cmd = match shell.family {
        astrcode_support::shell::ShellFamily::PowerShell => "Write-Output done".into(),
        astrcode_support::shell::ShellFamily::Cmd => "echo done".into(),
        _ => "echo done".into(),
    };

    let spawned = spawn_background_shell(BackgroundShellSpawnParams {
        session_id: "sess-consume".into(),
        command: echo_cmd,
        intent: None,
        cwd: temp.path().to_path_buf(),
        shell,
        timeout_secs: 10,
        store_dir: None,
    })
    .await
    .expect("spawn should succeed");

    let status = wait_background_shell(&spawned.shell_id, 5_000, None)
        .await
        .expect("first wait should return terminal status");
    assert!(!status.running);
    assert_eq!(status.status, "completed");
    assert!(status.output.contains("done"));

    let second = wait_background_shell(&spawned.shell_id, 0, None)
        .await
        .expect("repeated terminal poll should be a status result, not an error");
    assert!(!second.running);
    assert_eq!(second.status, "completed");
    assert_eq!(second.exit_code, Some(0));
    assert_eq!(second.output, "");
    assert_eq!(second.output_tokens, 0);
}

#[tokio::test]
async fn wait_background_shell_returns_incremental_output() {
    let temp = tempfile::tempdir().unwrap();
    let shell = astrcode_support::shell::resolve_shell();
    let command = match shell.family {
        astrcode_support::shell::ShellFamily::PowerShell => {
            "Write-Output first; Start-Sleep -Seconds 2; Write-Output second".into()
        },
        astrcode_support::shell::ShellFamily::Cmd => "echo first & powershell -NoProfile -Command \
                                                      \"Start-Sleep -Seconds 2\" & echo second"
            .into(),
        _ => "echo first; sleep 2; echo second".into(),
    };

    let spawned = spawn_background_shell(BackgroundShellSpawnParams {
        session_id: "sess-incremental".into(),
        command,
        intent: None,
        cwd: temp.path().to_path_buf(),
        shell,
        timeout_secs: 10,
        store_dir: None,
    })
    .await
    .expect("spawn should succeed");

    let first = wait_background_shell(&spawned.shell_id, 5_000, None)
        .await
        .expect("first poll should return first output");
    assert!(first.running, "command should still be running: {first:?}");
    assert!(first.output.contains("first"));
    assert!(!first.output.contains("second"));
    assert!(!first.output.contains("shell_id:"));
    assert!(!first.output.contains("--- STDERR ---"));

    let second = wait_background_shell(&spawned.shell_id, 5_000, None)
        .await
        .expect("second poll should return final output");
    assert!(!second.running);
    assert_eq!(second.status, "completed");
    assert!(!second.output.contains("first"));
    assert!(second.output.contains("second"));
}
