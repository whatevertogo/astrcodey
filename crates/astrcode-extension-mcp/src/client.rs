use std::{process::Stdio, sync::Arc, time::Duration};

use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, mpsc},
    task::JoinHandle,
};

use crate::{
    config::McpServerConfig,
    protocol::{self, CallToolResult, JsonRpcResponse, McpTool},
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);
const STDERR_TAIL_BYTES: usize = 8192;

pub(crate) struct StdioMcpClient {
    server: McpServerConfig,
    timeout: Duration,
}

impl StdioMcpClient {
    pub(crate) fn new(server: McpServerConfig) -> Self {
        Self {
            server,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub(crate) async fn list_tools(&self) -> Result<Vec<McpTool>, McpClientError> {
        let mut transport = StdioTransport::spawn(&self.server).await?;
        let result = async {
            initialize(&mut transport, self.timeout).await?;
            let result = request_result(
                &mut transport,
                protocol::list_tools_request(2),
                2,
                self.timeout,
            )
            .await?;
            protocol::parse_list_tools(result).map_err(McpClientError::Result)
        }
        .await;
        transport.shutdown().await;
        result
    }

    pub(crate) async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<CallToolResult, McpClientError> {
        let mut transport = StdioTransport::spawn(&self.server).await?;
        let result = async {
            initialize(&mut transport, self.timeout).await?;
            let result = request_result(
                &mut transport,
                protocol::call_tool_request(2, tool_name, arguments),
                2,
                self.timeout,
            )
            .await?;
            protocol::parse_call_tool(result).map_err(McpClientError::Result)
        }
        .await;
        transport.shutdown().await;
        result
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum McpClientError {
    #[error("spawn MCP server '{server}': {source}")]
    Spawn {
        server: String,
        source: std::io::Error,
    },
    #[error("MCP server '{server}' did not expose {stream}")]
    MissingPipe {
        server: String,
        stream: &'static str,
    },
    #[error("write MCP request: {0}")]
    Write(std::io::Error),
    #[error("read MCP stdout: {0}")]
    Stdout(std::io::Error),
    #[error("parse MCP response from stdout: {source}; line: {line}")]
    ParseResponse {
        line: String,
        source: serde_json::Error,
    },
    #[error("MCP request {id} timed out after {timeout_ms}ms; stderr tail: {stderr}")]
    Timeout {
        id: u64,
        timeout_ms: u128,
        stderr: String,
    },
    #[error("MCP stdout closed before response {id}; stderr tail: {stderr}")]
    Closed { id: u64, stderr: String },
    #[error("MCP response id mismatch: expected {expected}, got {actual:?}; stderr tail: {stderr}")]
    MismatchedResponse {
        expected: u64,
        actual: Option<u64>,
        stderr: String,
    },
    #[error("MCP server returned JSON-RPC error {code}: {message}; stderr tail: {stderr}")]
    Rpc {
        code: i64,
        message: String,
        stderr: String,
    },
    #[error("parse MCP result: {0}")]
    Result(serde_json::Error),
}

struct StdioTransport {
    child: Child,
    stdin: tokio::process::ChildStdin,
    responses: mpsc::UnboundedReceiver<Result<JsonRpcResponse, McpClientError>>,
    stderr_tail: Arc<Mutex<TailBuffer>>,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
}

impl StdioTransport {
    async fn spawn(server: &McpServerConfig) -> Result<Self, McpClientError> {
        let mut command = Command::new(&server.command);
        command
            .args(&server.args)
            .envs(&server.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &server.cwd {
            command.current_dir(cwd);
        }

        let mut child = command.spawn().map_err(|source| McpClientError::Spawn {
            server: server.name.clone(),
            source,
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpClientError::MissingPipe {
                server: server.name.clone(),
                stream: "stdin",
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpClientError::MissingPipe {
                server: server.name.clone(),
                stream: "stdout",
            })?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| McpClientError::MissingPipe {
                server: server.name.clone(),
                stream: "stderr",
            })?;

        let (tx, responses) = mpsc::unbounded_channel();
        let stdout_task = tokio::spawn(read_stdout(stdout, tx));
        let stderr_tail = Arc::new(Mutex::new(TailBuffer::new(STDERR_TAIL_BYTES)));
        let stderr_task = tokio::spawn(drain_stderr(stderr, Arc::clone(&stderr_tail)));

        Ok(Self {
            child,
            stdin,
            responses,
            stderr_tail,
            stdout_task,
            stderr_task,
        })
    }

    async fn send(&mut self, message: Value) -> Result<(), McpClientError> {
        self.stdin
            .write_all(protocol::serialize_line(&message).as_bytes())
            .await
            .map_err(McpClientError::Write)?;
        self.stdin.flush().await.map_err(McpClientError::Write)
    }

    async fn read_result(&mut self, id: u64, timeout: Duration) -> Result<Value, McpClientError> {
        loop {
            let received = match tokio::time::timeout(timeout, self.responses.recv()).await {
                Ok(received) => received,
                Err(_) => {
                    return Err(McpClientError::Timeout {
                        id,
                        timeout_ms: timeout.as_millis(),
                        stderr: self.stderr_tail().await,
                    });
                },
            };
            let Some(next) = received else {
                return Err(self.closed_error(id).await);
            };
            let response = next?;

            let Some(actual_id) = response.id else {
                continue;
            };
            if actual_id != id {
                return Err(McpClientError::MismatchedResponse {
                    expected: id,
                    actual: Some(actual_id),
                    stderr: self.stderr_tail().await,
                });
            }
            if let Some(error) = response.error {
                return Err(McpClientError::Rpc {
                    code: error.code,
                    message: error.message,
                    stderr: self.stderr_tail().await,
                });
            }
            return Ok(response.result.unwrap_or(Value::Null));
        }
    }

    async fn closed_error(&self, id: u64) -> McpClientError {
        McpClientError::Closed {
            id,
            stderr: self.stderr_tail().await,
        }
    }

    async fn stderr_tail(&self) -> String {
        self.stderr_tail.lock().await.as_string()
    }

    async fn shutdown(mut self) {
        let _ = self.stdin.shutdown().await;
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        }
        let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut self.stdout_task).await;
        let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut self.stderr_task).await;
    }
}

async fn initialize(
    transport: &mut StdioTransport,
    timeout: Duration,
) -> Result<(), McpClientError> {
    request_result(transport, protocol::initialize_request(1), 1, timeout).await?;
    transport.send(protocol::initialized_notification()).await
}

async fn request_result(
    transport: &mut StdioTransport,
    message: Value,
    id: u64,
    timeout: Duration,
) -> Result<Value, McpClientError> {
    transport.send(message).await?;
    transport.read_result(id, timeout).await
}

async fn read_stdout(
    stdout: tokio::process::ChildStdout,
    tx: mpsc::UnboundedSender<Result<JsonRpcResponse, McpClientError>>,
) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let parsed = serde_json::from_str::<JsonRpcResponse>(&line).map_err(|source| {
                    McpClientError::ParseResponse {
                        line: line.clone(),
                        source,
                    }
                });
                if tx.send(parsed).is_err() {
                    return;
                }
            },
            Ok(None) => return,
            Err(error) => {
                let _ = tx.send(Err(McpClientError::Stdout(error)));
                return;
            },
        }
    }
}

async fn drain_stderr(stderr: tokio::process::ChildStderr, tail: Arc<Mutex<TailBuffer>>) {
    let mut stderr = stderr;
    let mut buf = [0u8; 4096];
    loop {
        match stderr.read(&mut buf).await {
            Ok(0) => return,
            Ok(n) => tail.lock().await.push(&buf[..n]),
            Err(error) => {
                tail.lock()
                    .await
                    .push(format!("\n[stderr read error: {error}]").as_bytes());
                return;
            },
        }
    }
}

struct TailBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

impl TailBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
        if self.bytes.len() > self.limit {
            let overflow = self.bytes.len() - self.limit;
            self.bytes.drain(..overflow);
        }
    }

    fn as_string(&self) -> String {
        String::from_utf8_lossy(&self.bytes).trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
        process::Command as StdCommand,
    };

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn lists_tools_from_stdio_server() {
        let server = fake_server(BTreeMap::new());
        let tools = StdioMcpClient::new(server.config.clone())
            .with_timeout(Duration::from_secs(5))
            .list_tools()
            .await
            .unwrap();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
    }

    #[tokio::test]
    async fn calls_tool_on_stdio_server() {
        let server = fake_server(BTreeMap::new());
        let result = StdioMcpClient::new(server.config.clone())
            .with_timeout(Duration::from_secs(5))
            .call_tool("echo", json!({"text": "hello"}))
            .await
            .unwrap();

        assert!(!result.is_error);
        assert_eq!(protocol::render_call_content(&result), "called");
    }

    #[tokio::test]
    async fn drains_stderr_while_waiting_for_stdout() {
        let mut env = BTreeMap::new();
        env.insert("ASTRCODE_FAKE_MCP_NOISY_STDERR".into(), "1".into());
        let server = fake_server(env);
        let tools = StdioMcpClient::new(server.config.clone())
            .with_timeout(Duration::from_secs(5))
            .list_tools()
            .await
            .unwrap();

        assert_eq!(tools[0].name, "echo");
    }

    struct FakeServer {
        _temp: TempDir,
        config: McpServerConfig,
    }

    fn fake_server(env: BTreeMap<String, String>) -> FakeServer {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("fake_mcp_server.rs");
        fs::write(&source, FAKE_SERVER_SOURCE).unwrap();
        let exe = temp.path().join(exe_name("fake_mcp_server"));
        compile_fake_server(&source, &exe);
        FakeServer {
            config: McpServerConfig {
                name: "fake".into(),
                command: exe.to_string_lossy().to_string(),
                args: Vec::new(),
                env,
                cwd: None,
            },
            _temp: temp,
        }
    }

    fn compile_fake_server(source: &Path, exe: &Path) {
        let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
        let output = StdCommand::new(rustc)
            .arg(source)
            .arg("-o")
            .arg(exe)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "compile fake MCP server\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn exe_name(stem: &str) -> PathBuf {
        let mut name = PathBuf::from(stem);
        if cfg!(windows) {
            name.set_extension("exe");
        }
        name
    }

    const FAKE_SERVER_SOURCE: &str = r#"
use std::io::{self, BufRead, Write};

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = line.unwrap();
        let id = request_id(&line);
        if line.contains("\"method\":\"initialize\"") {
            writeln!(
                stdout,
                "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"protocolVersion\":\"2024-11-05\",\"capabilities\":{{}},\"serverInfo\":{{\"name\":\"fake\",\"version\":\"1\"}}}}}}",
                id
            ).unwrap();
            stdout.flush().unwrap();
        } else if line.contains("\"method\":\"tools/list\"") {
            if std::env::var("ASTRCODE_FAKE_MCP_NOISY_STDERR").ok().as_deref() == Some("1") {
                let mut stderr = io::stderr();
                for _ in 0..2048 {
                    stderr.write_all(&[b'e'; 1024]).unwrap();
                }
                stderr.flush().unwrap();
            }
            writeln!(
                stdout,
                "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"tools\":[{{\"name\":\"echo\",\"description\":\"Echo text\",\"inputSchema\":{{\"type\":\"object\",\"properties\":{{\"text\":{{\"type\":\"string\"}}}}}}}}]}}}}",
                id
            ).unwrap();
            stdout.flush().unwrap();
        } else if line.contains("\"method\":\"tools/call\"") {
            writeln!(
                stdout,
                "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"called\"}}],\"isError\":false}}}}",
                id
            ).unwrap();
            stdout.flush().unwrap();
        }
    }
}

fn request_id(line: &str) -> u64 {
    let Some(start) = line.find("\"id\":") else {
        return 0;
    };
    let rest = &line[start + 5..];
    let digits = rest
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    digits.parse().unwrap_or(0)
}
"#;
}
