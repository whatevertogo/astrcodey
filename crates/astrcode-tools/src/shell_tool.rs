//! Shell execution tool with streaming stdout/stderr and timeout.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;

use astrcode_core::tool::*;
use tokio::process::Command;

pub struct ShellTool { pub working_dir: PathBuf, pub timeout_secs: u64 }

#[async_trait::async_trait]
impl Tool for ShellTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "shell".into(),
            description: "Execute a shell command. Returns stdout, stderr, and exit code. Timeout: 120s.".into(),
            is_builtin: true,
            parameters: serde_json::json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}),
        }
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let cmd_str = args["command"].as_str().ok_or_else(|| ToolError::InvalidArguments("missing 'command'".into()))?;
        let (shell, arg) = if cfg!(windows) { ("cmd.exe", "/C") } else { ("/bin/sh", "-c") };

        let mut child = Command::new(shell).arg(arg).arg(cmd_str).current_dir(&self.working_dir)
            .stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null())
            .spawn().map_err(|e| ToolError::Execution(format!("spawn: {e}")))?;

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let out_h = tokio::spawn(read_all(stdout));
        let err_h = tokio::spawn(read_all(stderr));

        let status = tokio::time::timeout(
            std::time::Duration::from_secs(self.timeout_secs), child.wait(),
        ).await.map_err(|_| ToolError::Timeout(self.timeout_secs * 1000))?;

        let exit = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
        let out_text = out_h.await.unwrap_or_default();
        let err_text = err_h.await.unwrap_or_default();

        let mut output = out_text;
        if !err_text.is_empty() {
            if !output.is_empty() { output.push('\n'); }
            output.push_str("STDERR:\n"); output.push_str(&err_text);
        }

        let mut meta = BTreeMap::new();
        meta.insert("exit_code".into(), serde_json::json!(exit));
        if output.is_empty() { output = "(no output)".into(); }

        Ok(ToolResult { call_id: String::new(), content: output, is_error: exit != 0, metadata: meta })
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
