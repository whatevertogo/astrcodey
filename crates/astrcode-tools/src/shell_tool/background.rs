use std::{collections::BTreeMap, path::Path, time::Instant};

use astrcode_core::tool::{ToolError, ToolExecutionContext, ToolResult};
use astrcode_support::{
    hostpaths::resolve_path,
    shell::{ShellInfo, resolve_shell},
};

use super::{
    AUTO_BACKGROUND_AFTER_MS, BackgroundTransfer, CapturedOutput, ShellArgs, ShellTool,
    detect_shell_output_diagnostic, preprocess_shell_command,
};
use crate::{
    background_shell::{
        AdoptBackgroundShellParams, BackgroundShellSpawnParams, DEFAULT_STATUS_OUTPUT_MAX_TOKENS,
        adopt_running_shell, prepare_background_shell_output, spawn_background_shell,
        wait_background_shell,
    },
    files::tool_call_id,
};

impl ShellTool {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn try_adopt_foreground_shell(
        &self,
        args: &ShellArgs,
        command: &str,
        shell: &ShellInfo,
        cwd: &Path,
        timeout_secs: u64,
        started_at: Instant,
        ctx: &ToolExecutionContext,
        transfer: &BackgroundTransfer,
        child: &mut Option<tokio::process::Child>,
        out_h: &mut Option<tokio::task::JoinHandle<CapturedOutput>>,
        err_h: &mut Option<tokio::task::JoinHandle<CapturedOutput>>,
        reason: &str,
    ) -> Result<ToolResult, ToolError> {
        let prepared = prepare_background_shell_output(
            ctx.capabilities.paths.store_dir.as_deref(),
            cwd,
            command,
            args.intent.as_deref(),
            shell,
        )
        .await?;
        let shell_id = prepared.shell_id;
        let output_path = prepared.output_path;

        let remaining_timeout = timeout_secs
            .saturating_sub(started_at.elapsed().as_secs())
            .max(1);

        let out_handle = out_h
            .take()
            .ok_or_else(|| ToolError::Execution("stdout capture task missing".into()))?;
        let err_handle = err_h
            .take()
            .ok_or_else(|| ToolError::Execution("stderr capture task missing".into()))?;

        let adopted = adopt_running_shell(
            AdoptBackgroundShellParams {
                session_id: ctx.session_id.to_string(),
                timeout_secs: remaining_timeout,
                shell_id: shell_id.clone(),
                output_path: output_path.clone(),
                child: child
                    .take()
                    .ok_or_else(|| ToolError::Execution("child already taken".into()))?,
            },
            tokio::spawn(async move {
                let _ = out_handle.await;
            }),
            tokio::spawn(async move {
                let _ = err_handle.await;
            }),
        )
        .await?;

        transfer.activate(output_path.clone());

        let path = adopted.output_path.display().to_string();
        let reason_note = match reason {
            "auto_background" => format!(
                "Command exceeded {AUTO_BACKGROUND_AFTER_MS}ms and was moved to the background."
            ),
            _ => format!(
                "Command reached the {timeout_secs}s foreground timeout and was moved to the \
                 background."
            ),
        };
        let content = format!(
            "{reason_note}\nBackground shell started (shell_id: {}).\nOutput file: {path}\nYou \
             can check it with `shellId` and optional `blockUntilMs`. Use `read` on this path \
             only when you need more output than the status result returns.",
            adopted.shell_id
        );
        let mut meta = BTreeMap::new();
        meta.insert("backgrounded".into(), serde_json::json!(true));
        meta.insert("autoBackgrounded".into(), serde_json::json!(true));
        meta.insert("shellId".into(), serde_json::json!(adopted.shell_id));
        meta.insert("outputPath".into(), serde_json::json!(path));
        meta.insert("command".into(), serde_json::json!(args.command));
        if let Some(intent) = args
            .intent
            .as_ref()
            .filter(|intent| !intent.trim().is_empty())
        {
            meta.insert("intent".into(), serde_json::json!(intent));
        }
        Ok(ToolResult {
            call_id: tool_call_id(ctx),
            content,
            is_error: false,
            error: None,
            metadata: meta,
            duration_ms: Some(started_at.elapsed().as_millis() as u64),
        })
    }
}
impl ShellTool {
    pub(super) async fn execute_background_shell_spawn(
        &self,
        args: ShellArgs,
        started_at: Instant,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError> {
        if args.command.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "command cannot be empty when run_in_background is true".into(),
            ));
        }
        let shell = resolve_shell();
        let cwd = args
            .cwd
            .as_deref()
            .map(|cwd| resolve_path(&self.working_dir, cwd))
            .unwrap_or_else(|| self.working_dir.clone());
        let timeout_secs = args.timeout.unwrap_or(self.timeout_secs).min(600);
        let command = preprocess_shell_command(&args.command, &shell);
        let spawned = spawn_background_shell(BackgroundShellSpawnParams {
            session_id: ctx.session_id.to_string(),
            command,
            intent: args.intent.clone(),
            cwd,
            shell,
            timeout_secs,
            store_dir: ctx.capabilities.paths.store_dir.clone(),
        })
        .await?;
        let path = spawned.output_path.display().to_string();
        let content = format!(
            "Background shell started (shell_id: {}).\nOutput is being written to: {path}\nYou \
             can check it with `shellId` and optional `blockUntilMs`. Use `read` on this path \
             only when you need more output than the status result returns.",
            spawned.shell_id
        );
        let mut meta = BTreeMap::new();
        meta.insert("backgrounded".into(), serde_json::json!(true));
        meta.insert("shellId".into(), serde_json::json!(spawned.shell_id));
        meta.insert("outputPath".into(), serde_json::json!(path));
        meta.insert("command".into(), serde_json::json!(args.command));
        if let Some(intent) = args.intent.filter(|intent| !intent.trim().is_empty()) {
            meta.insert("intent".into(), serde_json::json!(intent));
        }
        Ok(ToolResult {
            call_id: tool_call_id(ctx),
            content,
            is_error: false,
            error: None,
            metadata: meta,
            duration_ms: Some(started_at.elapsed().as_millis() as u64),
        })
    }
}

pub(super) async fn execute_background_shell_wait(
    shell_id: &str,
    block_until_ms: u64,
    max_output_tokens: Option<usize>,
    started_at: Instant,
    ctx: &ToolExecutionContext,
) -> Result<ToolResult, ToolError> {
    let status = match wait_background_shell(shell_id, block_until_ms, max_output_tokens).await {
        Ok(status) => status,
        Err(ToolError::InvalidArguments(message)) if is_unknown_background_shell(&message) => {
            return Ok(stale_background_shell_result(
                shell_id, message, started_at, ctx,
            ));
        },
        Err(error) => return Err(error),
    };
    let path = status.output_path.display().to_string();
    let output_label = if status.running {
        "New output"
    } else {
        "Final output"
    };
    let output = if status.output.trim().is_empty() {
        "(no new output)".to_string()
    } else {
        status.output
    };
    let has_new_output = output != "(no new output)";
    let rendered_output = if has_new_output {
        output
    } else if status.running {
        "No new output since the previous background-shell poll.".to_string()
    } else {
        "No new output remained when the shell finished. Read the output file only if you need to \
         inspect earlier output."
            .to_string()
    };
    let diagnostic = if status.running {
        None
    } else {
        detect_shell_output_diagnostic(&rendered_output)
    };
    let content = if status.running {
        format!(
            "Shell {shell_id} is still running.\nOutput file: \
             {path}\n\n{output_label}:\n{rendered_output}"
        )
    } else if let Some(diagnostic) = diagnostic {
        format!(
            "{}\n\nShell {shell_id} finished (status: {}, exit_code: {:?}).\nOutput file: \
             {path}\n\n{output_label}:\n{rendered_output}",
            diagnostic.message(),
            status.status,
            status.exit_code
        )
    } else {
        format!(
            "Shell {shell_id} finished (status: {}, exit_code: {:?}).\nOutput file: \
             {path}\n\n{output_label}:\n{rendered_output}",
            status.status, status.exit_code
        )
    };
    let is_error =
        matches!(status.status.as_str(), "failed" | "timed_out" | "killed") || diagnostic.is_some();
    let mut meta = BTreeMap::new();
    meta.insert("shellId".into(), serde_json::json!(shell_id));
    meta.insert("outputPath".into(), serde_json::json!(path));
    meta.insert("running".into(), serde_json::json!(status.running));
    meta.insert(
        "outputTruncated".into(),
        serde_json::json!(status.output_truncated),
    );
    meta.insert("hasNewOutput".into(), serde_json::json!(has_new_output));
    meta.insert(
        "outputTokens".into(),
        serde_json::json!(status.output_tokens),
    );
    meta.insert(
        "returnedOutputTokens".into(),
        serde_json::json!(status.returned_output_tokens),
    );
    meta.insert(
        "omittedOutputTokens".into(),
        serde_json::json!(status.omitted_output_tokens),
    );
    meta.insert(
        "maxOutputTokens".into(),
        serde_json::json!(status.max_output_tokens),
    );
    meta.insert("status".into(), serde_json::json!(status.status));
    if let Some(code) = status.exit_code {
        meta.insert("exitCode".into(), serde_json::json!(code));
    }
    if let Some(diagnostic) = diagnostic {
        meta.insert("semanticError".into(), serde_json::json!(diagnostic.code()));
        meta.insert(
            "semanticErrorMessage".into(),
            serde_json::json!(diagnostic.message()),
        );
    }
    Ok(ToolResult {
        call_id: tool_call_id(ctx),
        content,
        is_error,
        error: if let Some(diagnostic) = diagnostic {
            Some(diagnostic.message().to_string())
        } else {
            is_error.then(|| format!("background shell {}", status.status))
        },
        metadata: meta,
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
    })
}

fn is_unknown_background_shell(message: &str) -> bool {
    message.starts_with("unknown shell_id:")
}

fn stale_background_shell_result(
    shell_id: &str,
    message: String,
    started_at: Instant,
    ctx: &ToolExecutionContext,
) -> ToolResult {
    let content = format!(
        "Shell {shell_id} is not active in this server process.\nStatus: \
         unknown_stale_shell_id.\n\nThis usually means the shellId came from an older server \
         process, an older session, or a session cleanup. The shell is not running here, and \
         polling this shellId again will not produce output. Stop polling this shellId. If an \
         earlier shell result showed an output file path, use `read` on that path only if you \
         still need to inspect saved output."
    );
    let mut meta = BTreeMap::new();
    meta.insert("shellId".into(), serde_json::json!(shell_id));
    meta.insert("running".into(), serde_json::json!(false));
    meta.insert("status".into(), serde_json::json!("unknown_stale_shell_id"));
    meta.insert("hasNewOutput".into(), serde_json::json!(false));
    meta.insert("outputTruncated".into(), serde_json::json!(false));
    meta.insert("outputTokens".into(), serde_json::json!(0));
    meta.insert("returnedOutputTokens".into(), serde_json::json!(0));
    meta.insert("omittedOutputTokens".into(), serde_json::json!(0));
    meta.insert(
        "maxOutputTokens".into(),
        serde_json::json!(DEFAULT_STATUS_OUTPUT_MAX_TOKENS),
    );
    meta.insert("staleShellId".into(), serde_json::json!(true));
    meta.insert("diagnostic".into(), serde_json::json!(message));
    ToolResult {
        call_id: tool_call_id(ctx),
        content,
        is_error: false,
        error: None,
        metadata: meta,
        duration_ms: Some(started_at.elapsed().as_millis() as u64),
    }
}
