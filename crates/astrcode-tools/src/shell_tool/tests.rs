use std::time::Instant;

use astrcode_core::tool::{Tool, ToolCapabilities, ToolExecutionContext};
use astrcode_support::shell::{ShellFamily, ShellInfo, resolve_shell};

use super::{
    MAX_CAPTURE_BYTES_PER_STREAM, ShellTool, capture_stream, command_args, preprocess_shell_command,
};

fn empty_ctx() -> ToolExecutionContext {
    ToolExecutionContext::new(
        String::new().into(),
        String::new(),
        None,
        None,
        ToolCapabilities::default(),
    )
}

fn ctx_with_session(session_id: &str) -> ToolExecutionContext {
    ToolExecutionContext::new(
        session_id.to_string().into(),
        String::new(),
        None,
        None,
        ToolCapabilities::default(),
    )
}

fn command_with_stderr() -> String {
    match resolve_shell().family {
        ShellFamily::PowerShell => "Write-Output out; [Console]::Error.WriteLine('err')".into(),
        ShellFamily::Cmd => "echo out & echo err 1>&2".into(),
        ShellFamily::Posix | ShellFamily::Wsl => "echo out; echo err >&2".into(),
    }
}

fn command_with_delay() -> String {
    match resolve_shell().family {
        // Use `cmd /c echo` to bypass PowerShell's .NET output buffering.
        // `[Console]::Out.WriteLine` + `.Flush()` may not reach the OS pipe before
        // `taskkill /F` destroys the process, losing the buffered data.
        ShellFamily::PowerShell => {
            "cmd /c echo before; Start-Sleep -Seconds 10; cmd /c echo after".into()
        },
        // cmd.exe has no built-in sleep; delegate to powershell (always on PATH from cmd).
        ShellFamily::Cmd => "echo before & powershell -NoProfile -Command \"Start-Sleep -Seconds \
                             10\" & echo after"
            .into(),
        // POSIX shells (bash, zsh, Git Bash, WSL) all provide native `sleep`.
        ShellFamily::Posix | ShellFamily::Wsl => "echo before; sleep 10; echo after".into(),
    }
}

/// 读取 stdin 并原样输出，用于测试 stdin 参数。
fn command_echoing_stdin() -> String {
    match resolve_shell().family {
        ShellFamily::PowerShell => "[Console]::In.ReadToEnd()".into(),
        ShellFamily::Cmd => "findstr \"^\"".into(),
        ShellFamily::Posix | ShellFamily::Wsl => "cat".into(),
    }
}

fn command_with_sudo_auth_failure_output_and_zero_exit() -> String {
    match resolve_shell().family {
        ShellFamily::PowerShell => "Write-Output 'sudo: a terminal is required to read the \
                                    password'; Write-Output 'sudo: a password is required'; exit 0"
            .into(),
        ShellFamily::Cmd => "echo sudo: a terminal is required to read the password & echo sudo: \
                             a password is required & exit /b 0"
            .into(),
        ShellFamily::Posix | ShellFamily::Wsl => "printf '%s\\n' 'sudo: a terminal is required to \
                                                  read the password' 'sudo: a password is \
                                                  required'; exit 0"
            .into(),
    }
}

#[test]
fn command_args_match_resolved_shell_family() {
    let command = "echo ok";

    let powershell = ShellInfo {
        family: ShellFamily::PowerShell,
        name: "powershell".into(),
        path: "powershell.exe".into(),
    };
    assert_eq!(
        command_args(&powershell, command),
        vec!["-NoProfile", "-Command", command]
    );

    let cmd = ShellInfo {
        family: ShellFamily::Cmd,
        name: "cmd".into(),
        path: "cmd.exe".into(),
    };
    assert_eq!(command_args(&cmd, command), vec!["/d", "/s", "/c", command]);

    let posix = ShellInfo {
        family: ShellFamily::Posix,
        name: "bash".into(),
        path: "bash".into(),
    };
    assert_eq!(command_args(&posix, command), vec!["-lc", command]);
}

#[test]
fn preprocess_shell_command_rewrites_nul_redirect_on_windows_posix() {
    let bash = ShellInfo {
        family: ShellFamily::Posix,
        name: "bash".into(),
        path: "bash".into(),
    };
    if cfg!(windows) {
        assert_eq!(
            preprocess_shell_command("cmd 2>nul || echo ok", &bash),
            "cmd 2>/dev/null || echo ok"
        );
        assert_eq!(
            preprocess_shell_command("echo \">nul\"", &bash),
            "echo \">nul\""
        );
    } else {
        assert_eq!(
            preprocess_shell_command("cmd 2>nul || echo ok", &bash),
            "cmd 2>nul || echo ok"
        );
    }
}

#[test]
fn preprocess_shell_command_skips_powershell_on_windows() {
    let powershell = ShellInfo {
        family: ShellFamily::PowerShell,
        name: "powershell".into(),
        path: "powershell.exe".into(),
    };
    let command = "cmd 2>nul";
    assert_eq!(preprocess_shell_command(command, &powershell), command);
}

#[tokio::test]
async fn shell_captures_stdout_and_stderr_without_live_events() {
    let tool = ShellTool {
        working_dir: std::env::current_dir().expect("cwd should exist"),
        timeout_secs: 30,
    };
    let mut ctx = empty_ctx();
    ctx.tool_call_id = Some("shell-capture".into());

    let result = tool
        .execute(
            serde_json::json!({
                "command": command_with_stderr(),
                "intent": "Check stdout and stderr capture"
            }),
            &ctx,
        )
        .await
        .expect("shell should execute");

    assert_eq!(result.call_id, "shell-capture");
    assert!(!result.is_error, "{result:?}");
    assert!(result.content.contains("out"));
    assert!(result.content.contains("err"));
    assert_eq!(result.metadata["streamed"], serde_json::json!(false));
    assert_eq!(
        result.metadata["intent"],
        serde_json::json!("Check stdout and stderr capture")
    );
}

#[tokio::test]
async fn shell_timeout_returns_partial_output() {
    let tool = ShellTool {
        working_dir: std::env::current_dir().expect("cwd should exist"),
        timeout_secs: 30,
    };

    let result = tool
        .execute(
            serde_json::json!({
                "command": command_with_delay(),
                "timeout": 5
            }),
            &empty_ctx(),
        )
        .await
        .expect("shell should return a structured timeout result");

    assert!(result.is_error);
    assert!(result.content.contains("before"));
    assert!(
        !result.content.lines().any(|line| line.trim() == "after"),
        "command output after the sleep should not appear: {}",
        result.content
    );
    assert!(
        result.content.contains("timed out after 5s"),
        "timeout reason must be in content for the LLM: {}",
        result.content
    );
    assert_eq!(result.metadata["timedOut"], serde_json::json!(true));
    assert_eq!(result.metadata["streamed"], serde_json::json!(false));
}

#[tokio::test]
async fn shell_stdin_pipes_data_to_command() {
    let tool = ShellTool {
        working_dir: std::env::current_dir().expect("cwd should exist"),
        timeout_secs: 30,
    };

    let result = tool
        .execute(
            serde_json::json!({
                "command": command_echoing_stdin(),
                "stdin": "hello from stdin\n"
            }),
            &empty_ctx(),
        )
        .await
        .expect("shell with stdin should execute");

    assert!(!result.is_error, "unexpected error: {result:?}");
    assert!(
        result.content.contains("hello from stdin"),
        "stdout should contain stdin data, got: {}",
        result.content
    );
}

#[tokio::test]
async fn shell_rejects_empty_command() {
    let tool = ShellTool {
        working_dir: std::env::current_dir().expect("cwd should exist"),
        timeout_secs: 30,
    };
    let result = tool
        .execute(serde_json::json!({ "command": "   " }), &empty_ctx())
        .await;
    let err = result.expect_err("empty command should fail");
    assert!(
        err.to_string().contains("empty"),
        "error should mention empty: {err}"
    );
}

#[tokio::test]
async fn shell_nonzero_exit_code_is_error() {
    let tool = ShellTool {
        working_dir: std::env::current_dir().expect("cwd should exist"),
        timeout_secs: 30,
    };
    let exit_cmd = match resolve_shell().family {
        ShellFamily::PowerShell => "exit 42",
        ShellFamily::Cmd => "exit /b 42",
        // On MSYS2/bash, the exit code may be transformed; just verify non-zero.
        ShellFamily::Posix | ShellFamily::Wsl => "exit 42",
    };
    let result = tool
        .execute(serde_json::json!({ "command": exit_cmd }), &empty_ctx())
        .await
        .expect("shell should execute");
    assert!(result.is_error, "non-zero exit should be error");
    let exit_code = result.metadata["exitCode"].as_i64().expect("exitCode");
    assert_ne!(
        exit_code, 0,
        "exit code should be non-zero, got {exit_code}"
    );
}

#[tokio::test]
async fn shell_sudo_auth_failure_output_is_error_even_with_zero_exit() {
    let tool = ShellTool {
        working_dir: std::env::current_dir().expect("cwd should exist"),
        timeout_secs: 30,
    };

    let result = tool
        .execute(
            serde_json::json!({
                "command": command_with_sudo_auth_failure_output_and_zero_exit()
            }),
            &empty_ctx(),
        )
        .await
        .expect("shell should execute");

    assert!(
        result.is_error,
        "sudo auth failure output should be treated as a semantic error: {result:?}"
    );
    assert_eq!(result.metadata["exitCode"], serde_json::json!(0));
    assert_eq!(
        result.metadata["semanticError"],
        serde_json::json!("sudo_authentication_required")
    );
    assert!(
        result.content.contains("pipeline may have hidden"),
        "diagnostic should explain the hidden failure: {}",
        result.content
    );
}

#[test]
fn auto_background_disallows_sleep_commands() {
    assert!(!super::is_auto_background_allowed("sleep 60"));
    assert!(!super::is_auto_background_allowed("Start-Sleep -Seconds 5"));
    assert!(super::is_auto_background_allowed("cargo build --release"));
}

#[test]
fn render_shell_output_formats_stdout_and_stderr() {
    use super::render_shell_output;
    assert_eq!(render_shell_output("", ""), "");
    assert_eq!(render_shell_output("hello", ""), "hello");
    assert_eq!(render_shell_output("", "oops"), "STDERR:\noops");
    assert_eq!(render_shell_output("hi", "err"), "hi\nSTDERR:\nerr");
}

#[test]
fn command_args_wsl_prepends_bash() {
    let wsl = ShellInfo {
        family: ShellFamily::Wsl,
        name: "wsl".into(),
        path: "wsl.exe".into(),
    };
    assert_eq!(command_args(&wsl, "ls -la"), vec!["bash", "-lc", "ls -la"]);
}

#[tokio::test]
async fn shell_metadata_includes_shell_info_and_cwd() {
    let tool = ShellTool {
        working_dir: std::env::current_dir().expect("cwd should exist"),
        timeout_secs: 30,
    };
    let result = tool
        .execute(
            serde_json::json!({
                "command": match resolve_shell().family {
                    ShellFamily::PowerShell => "Write-Output ok",
                    ShellFamily::Cmd => "echo ok",
                    ShellFamily::Posix | ShellFamily::Wsl => "echo ok",
                },
            }),
            &empty_ctx(),
        )
        .await
        .expect("shell should execute");

    assert!(!result.is_error, "{result:?}");
    assert!(result.metadata.contains_key("shell"));
    assert!(result.metadata.contains_key("shellPath"));
    assert!(result.metadata.contains_key("cwd"));
    assert!(result.metadata.contains_key("exitCode"));
    assert_eq!(result.metadata["streamed"], serde_json::json!(false));
    assert_eq!(result.metadata["timedOut"], serde_json::json!(false));
}

#[tokio::test]
async fn shell_run_in_background_returns_shell_id() {
    let tool = ShellTool {
        working_dir: std::env::current_dir().expect("cwd should exist"),
        timeout_secs: 30,
    };
    let sleep_cmd: String = match resolve_shell().family {
        ShellFamily::PowerShell => "Start-Sleep -Seconds 5".into(),
        ShellFamily::Cmd => "timeout /t 5 /nobreak >nul".into(),
        ShellFamily::Posix | ShellFamily::Wsl => "sleep 5".into(),
    };
    let result = tool
        .execute(
            serde_json::json!({
                "command": sleep_cmd,
                "runInBackground": true,
                "intent": "Long sleep in background"
            }),
            &empty_ctx(),
        )
        .await
        .expect("background shell should start");

    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.metadata["backgrounded"], serde_json::json!(true));
    let shell_id = result.metadata["shellId"]
        .as_str()
        .expect("shellId metadata");
    assert!(!shell_id.is_empty());
    assert!(result.content.contains(shell_id));

    let status =
        super::execute_background_shell_wait(shell_id, 0, None, Instant::now(), &empty_ctx())
            .await
            .expect("status query");
    assert_eq!(status.metadata["running"], serde_json::json!(true));
}

#[tokio::test]
async fn completed_background_shell_can_be_polled_repeatedly_without_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let session_id = format!("sess-repeat-terminal-{}", uuid::Uuid::new_v4());
    let ctx = ctx_with_session(&session_id);
    let tool = ShellTool {
        working_dir: temp.path().to_path_buf(),
        timeout_secs: 30,
    };
    let command: String = match resolve_shell().family {
        ShellFamily::PowerShell => "Write-Output done".into(),
        ShellFamily::Cmd => "echo done".into(),
        ShellFamily::Posix | ShellFamily::Wsl => "echo done".into(),
    };

    let started = tool
        .execute(
            serde_json::json!({
                "command": command,
                "runInBackground": true,
                "intent": "Finish quickly in background"
            }),
            &ctx,
        )
        .await
        .expect("background shell should start");
    let shell_id = started.metadata["shellId"]
        .as_str()
        .expect("shellId metadata");

    let final_poll = tool
        .execute(
            serde_json::json!({
                "shellId": shell_id,
                "blockUntilMs": 5_000
            }),
            &ctx,
        )
        .await
        .expect("first terminal poll should succeed");
    assert!(!final_poll.is_error, "{final_poll:?}");
    assert_eq!(final_poll.metadata["running"], serde_json::json!(false));
    assert_eq!(
        final_poll.metadata["status"],
        serde_json::json!("completed")
    );
    assert!(
        final_poll.content.contains("done"),
        "first terminal poll should include final output: {}",
        final_poll.content
    );

    let repeated_poll = tool
        .execute(
            serde_json::json!({
                "shellId": shell_id,
                "blockUntilMs": 0
            }),
            &ctx,
        )
        .await
        .expect("repeated terminal poll should not become unknown shell_id");
    assert!(!repeated_poll.is_error, "{repeated_poll:?}");
    assert_eq!(repeated_poll.metadata["running"], serde_json::json!(false));
    assert_eq!(
        repeated_poll.metadata["hasNewOutput"],
        serde_json::json!(false)
    );
    assert!(
        repeated_poll
            .content
            .contains("No new output remained when the shell finished"),
        "repeated poll should tell the model to stop polling: {}",
        repeated_poll.content
    );

    crate::background_shell::cleanup_background_shells_for_session(&session_id);
}

#[tokio::test]
async fn stale_background_shell_id_returns_terminal_status_instead_of_tool_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let tool = ShellTool {
        working_dir: temp.path().to_path_buf(),
        timeout_secs: 30,
    };
    let stale_shell_id = format!("stale-{}", uuid::Uuid::new_v4());

    let result = tool
        .execute(
            serde_json::json!({
                "shellId": stale_shell_id,
                "blockUntilMs": 0
            }),
            &ctx_with_session("sess-stale-shell-id"),
        )
        .await
        .expect("stale shellId should be a terminal status result");

    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.metadata["running"], serde_json::json!(false));
    assert_eq!(
        result.metadata["status"],
        serde_json::json!("unknown_stale_shell_id")
    );
    assert_eq!(result.metadata["staleShellId"], serde_json::json!(true));
    assert!(
        result.content.contains("Stop polling this shellId"),
        "stale shellId response must tell the model to stop: {}",
        result.content
    );
    assert!(
        !result.content.contains("Invalid arguments"),
        "stale shellId should not be framed as a parameter error: {}",
        result.content
    );
}

#[tokio::test]
async fn capture_stream_truncates_but_keeps_draining() {
    use tokio::io::AsyncWriteExt;

    let total = MAX_CAPTURE_BYTES_PER_STREAM + 4096;
    let (mut writer, reader) = tokio::io::duplex(total);
    tokio::spawn(async move {
        writer
            .write_all(&vec![b'x'; total])
            .await
            .expect("write test payload");
    });

    let captured = capture_stream(reader).await;
    assert!(captured.truncated);
    assert_eq!(captured.bytes_read, total);
    assert_eq!(captured.text.len(), MAX_CAPTURE_BYTES_PER_STREAM);
}

#[tokio::test]
async fn shell_id_and_command_are_mutually_exclusive() {
    let tool = ShellTool {
        working_dir: std::env::current_dir().expect("cwd should exist"),
        timeout_secs: 30,
    };
    let result = tool
        .execute(
            serde_json::json!({
                "command": "echo hi",
                "shellId": "fake-id",
            }),
            &empty_ctx(),
        )
        .await;
    let err = result.expect_err("should reject shellId + command");
    let msg = format!("{err}");
    assert!(
        msg.contains("cannot specify both shellId and command"),
        "unexpected error: {msg}"
    );
}

#[tokio::test]
async fn shell_id_and_run_in_background_are_mutually_exclusive() {
    let tool = ShellTool {
        working_dir: std::env::current_dir().expect("cwd should exist"),
        timeout_secs: 30,
    };
    let result = tool
        .execute(
            serde_json::json!({
                "shellId": "fake-id",
                "runInBackground": true,
            }),
            &empty_ctx(),
        )
        .await;
    let err = result.expect_err("should reject shellId + runInBackground");
    let msg = format!("{err}");
    assert!(
        msg.contains("cannot specify both shellId and runInBackground"),
        "unexpected error: {msg}"
    );
}

#[tokio::test]
async fn max_output_tokens_requires_shell_id() {
    let tool = ShellTool {
        working_dir: std::env::current_dir().expect("cwd should exist"),
        timeout_secs: 30,
    };
    let result = tool
        .execute(
            serde_json::json!({
                "command": "echo hi",
                "maxOutputTokens": 256,
            }),
            &empty_ctx(),
        )
        .await;
    let err = result.expect_err("maxOutputTokens without shellId should fail");
    assert!(
        err.to_string().contains("maxOutputTokens can only be used"),
        "unexpected error: {err}"
    );
}
