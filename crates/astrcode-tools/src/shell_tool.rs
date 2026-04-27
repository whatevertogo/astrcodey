//! Shell execution tool with streaming stdout/stderr and timeout.

use std::{collections::BTreeMap, path::PathBuf, process::Stdio};

use astrcode_core::tool::*;
use astrcode_support::shell::{ShellFamily, ShellInfo, resolve_shell};
use serde::Deserialize;
use tokio::process::Command;

pub struct ShellTool {
    pub working_dir: PathBuf,
    pub timeout_secs: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShellArgs {
    command: String,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    timeout: Option<u64>,
}

#[async_trait::async_trait]
impl Tool for ShellTool {
    fn definition(&self) -> ToolDefinition {
        let shell = resolve_shell();
        ToolDefinition {
            name: "shell".into(),
            description: format!(
                "Execute a shell command with the default shell ({}). Returns stdout, stderr, and \
                 exit code. Timeout: 120s.",
                shell.name
            ),
            is_builtin: true,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory for this command. Prefer this over shell-level cd."
                    },
                    "timeout": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 600,
                        "description": "Timeout in seconds (default from config, max 600)."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let args: ShellArgs = serde_json::from_value(args)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid shell args: {e}")))?;
        if args.command.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "command cannot be empty".into(),
            ));
        }
        let shell = resolve_shell();
        let command_args = command_args(&shell, &args.command);
        let cwd = args
            .cwd
            .as_deref()
            .map(|cwd| resolve_path(&self.working_dir, cwd))
            .unwrap_or_else(|| self.working_dir.clone());
        let timeout_secs = args.timeout.unwrap_or(self.timeout_secs).min(600);

        let mut child = Command::new(&shell.path)
            .args(&command_args)
            .current_dir(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| ToolError::Execution(format!("spawn: {e}")))?;

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let out_h = tokio::spawn(read_all(stdout));
        let err_h = tokio::spawn(read_all(stderr));

        let status =
            match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), child.wait())
                .await
            {
                Ok(status) => status,
                Err(_) => {
                    let _ = child.start_kill();
                    return Err(ToolError::Timeout(timeout_secs * 1000));
                },
            };

        let exit = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
        let out_text = out_h.await.unwrap_or_default();
        let err_text = err_h.await.unwrap_or_default();

        let mut output = out_text;
        if !err_text.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str("STDERR:\n");
            output.push_str(&err_text);
        }

        let mut meta = BTreeMap::new();
        meta.insert("exit_code".into(), serde_json::json!(exit));
        meta.insert("shell".into(), serde_json::json!(shell.name));
        meta.insert("shell_path".into(), serde_json::json!(shell.path));
        meta.insert("cwd".into(), serde_json::json!(cwd.display().to_string()));
        if output.is_empty() {
            output = "(no output)".into();
        }

        Ok(ToolResult {
            call_id: String::new(),
            content: output,
            is_error: exit != 0,
            error: if exit == 0 {
                None
            } else {
                Some(format!("exit code {exit}"))
            },
            metadata: meta,
            duration_ms: None,
        })
    }
}

fn resolve_path(cwd: &std::path::Path, raw: &std::path::Path) -> PathBuf {
    if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        cwd.join(raw)
    }
}

fn command_args(shell: &ShellInfo, command: &str) -> Vec<String> {
    match shell.family {
        ShellFamily::PowerShell => vec![
            "-NoProfile".to_string(),
            "-Command".to_string(),
            command.to_string(),
        ],
        ShellFamily::Cmd => vec![
            "/d".to_string(),
            "/s".to_string(),
            "/c".to_string(),
            command.to_string(),
        ],
        ShellFamily::Posix => vec!["-lc".to_string(), command.to_string()],
        ShellFamily::Wsl => vec!["bash".to_string(), "-lc".to_string(), command.to_string()],
    }
}

async fn read_all(stream: impl tokio::io::AsyncRead + Unpin) -> String {
    let mut reader = tokio::io::BufReader::new(stream);
    let mut buf = Vec::new();
    use tokio::io::AsyncReadExt;
    let _ = reader.read_to_end(&mut buf).await;
    String::from_utf8_lossy(&buf).into_owned()
}

// TODO: sandbox support — execute commands in isolated environment
// TODO: execpolicy — command allow/deny rules (via extensions)

#[cfg(test)]
mod tests {
    use astrcode_support::shell::{ShellFamily, ShellInfo};

    use super::command_args;

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
}
