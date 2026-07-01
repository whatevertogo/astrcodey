//! Persistent MCP process pool.
//!
//! MCP server processes are spawned once during pre-warm and kept alive for the
//! extension lifecycle. All `list_tools` and `call_tool` requests reuse
//! the same long-lived connections, eliminating per-request spawn/initialize/kill
//! overhead from turn and session paths.

use std::{
    collections::{HashMap, HashSet},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex as AsyncMutex, mpsc},
    task::JoinHandle,
};

// Re-export for use in lib.rs
pub(crate) use self::error::McpPoolError;
use crate::{
    config::McpServerConfig,
    http_client::HttpPooledClient,
    protocol::{self, CallToolResult, JsonRpcResponse, McpTool},
};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(500);
const STDERR_TAIL_BYTES: usize = 8192;

mod error {
    #[derive(Debug, thiserror::Error)]
    pub(crate) enum McpPoolError {
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
        #[error(
            "MCP response id mismatch: expected {expected}, got {actual:?}; stderr tail: {stderr}"
        )]
        MismatchedResponse {
            expected: u64,
            actual: Option<u64>,
            stderr: String,
        },
        #[error("MCP server returned JSON-RPC error {code}: {message}; stderr tail: {stderr}")]
        Rpc {
            code: i32,
            message: String,
            stderr: String,
        },
        #[error("parse MCP result: {0}")]
        Result(serde_json::Error),
        #[error("HTTP request failed: {message}")]
        Http { message: String },
        #[error("HTTP request to {url} timed out")]
        HttpTimeout { url: String },
        #[error("HTTP MCP session expired with status {status} from {url}; body: {body}")]
        HttpSessionExpired {
            status: u16,
            url: String,
            body: String,
        },
        #[error("MCP server '{server}' not found in pool")]
        ServerNotFound { server: String },
        #[error("a pooled MCP stdio server process is no longer running")]
        UnhealthyProcess,
    }
}

/// Persistent pool of MCP server processes/connections.
///
/// Each pooled client serializes its requests while independent servers may
/// run in parallel.
pub(crate) struct McpProcessPool {
    pool: AsyncMutex<PoolInner>,
    timeout: Duration,
}

struct PoolInner {
    entries: HashMap<ServerId, Arc<PooledClient>>,
}

/// Unique identifier for a server config (transport + connection params).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ServerId(String);

impl ServerId {
    fn from_config(server: &McpServerConfig) -> Self {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        match server.transport {
            crate::config::McpTransport::Stdio => {
                "stdio".hash(&mut hasher);
                server.command.hash(&mut hasher);
                server.args.hash(&mut hasher);
                for (k, v) in &server.env {
                    k.hash(&mut hasher);
                    v.hash(&mut hasher);
                }
                server.cwd.hash(&mut hasher);
            },
            crate::config::McpTransport::Http => {
                "http".hash(&mut hasher);
                server.url.hash(&mut hasher);
                for (k, v) in &server.headers {
                    k.hash(&mut hasher);
                    v.hash(&mut hasher);
                }
            },
        }
        Self(format!("{:x}", hasher.finish()))
    }
}

enum PooledClient {
    Stdio(Box<StdioPooledClient>),
    Http(Box<HttpPooledClient>),
}

/// Long-lived stdio MCP process.
struct StdioPooledClient {
    /// Child process handle. Wrapped in Option so shutdown can take it.
    child: std::sync::Mutex<Option<Child>>,
    stdin: AsyncMutex<tokio::process::ChildStdin>,
    responses: AsyncMutex<mpsc::UnboundedReceiver<Result<JsonRpcResponse, McpPoolError>>>,
    request_lock: AsyncMutex<()>,
    next_id: AtomicU64,
    timeout: Duration,
    stderr_buffer: Arc<AsyncMutex<TailBuffer>>,
    _stdout_task: JoinHandle<()>,
    _stderr_task: JoinHandle<()>,
}

// ─── McpProcessPool ──────────────────────────────────────────────────────

impl McpProcessPool {
    pub(crate) fn new(timeout: Duration) -> Self {
        Self {
            pool: AsyncMutex::new(PoolInner {
                entries: HashMap::new(),
            }),
            timeout,
        }
    }

    /// Spawn processes for all given servers, run initialize handshake, and
    /// keep them alive. Skips servers that are already in the pool.
    pub(crate) async fn pre_warm(
        &self,
        servers: &[McpServerConfig],
    ) -> Vec<(String, Result<(), McpPoolError>)> {
        let results = futures_util::future::join_all(servers.iter().map(|server| async {
            let name = server.name.clone();
            let result = self.ensure_pooled(server).await;
            (name, result)
        }))
        .await;
        results
    }

    /// Execute `tools/list`, restoring a dead pooled process if needed.
    pub(crate) async fn list_tools(
        &self,
        server: &McpServerConfig,
    ) -> Result<Vec<McpTool>, McpPoolError> {
        let (id, entry) = self.pooled_entry(server).await?;
        match list_tools_from_entry(&entry).await {
            Ok(tools) => Ok(tools),
            Err(error) if should_retry_after_reconnect(&error) => {
                self.evict_entry_if_current(&id, &entry).await;
                let (retry_id, retry_entry) = self.pooled_entry(server).await?;
                match list_tools_from_entry(&retry_entry).await {
                    Ok(tools) => Ok(tools),
                    Err(retry_error) => {
                        if error_invalidates_client(&retry_error) {
                            self.evict_entry_if_current(&retry_id, &retry_entry).await;
                        }
                        Err(retry_error)
                    },
                }
            },
            Err(error) => {
                if error_invalidates_client(&error) {
                    self.evict_entry_if_current(&id, &entry).await;
                }
                Err(error)
            },
        }
    }

    /// Execute `tools/call`, restoring a dead pooled process if needed.
    pub(crate) async fn call_tool(
        &self,
        server: &McpServerConfig,
        tool_name: &str,
        arguments: Value,
    ) -> Result<CallToolResult, McpPoolError> {
        let (id, entry) = self.pooled_entry(server).await?;
        match call_tool_from_entry(&entry, tool_name, arguments.clone()).await {
            Ok(result) => Ok(result),
            Err(error) if should_retry_call_after_reconnect(&error) => {
                self.evict_entry_if_current(&id, &entry).await;
                let (retry_id, retry_entry) = self.pooled_entry(server).await?;
                match call_tool_from_entry(&retry_entry, tool_name, arguments).await {
                    Ok(result) => Ok(result),
                    Err(retry_error) => {
                        if error_invalidates_client(&retry_error) {
                            self.evict_entry_if_current(&retry_id, &retry_entry).await;
                        }
                        Err(retry_error)
                    },
                }
            },
            Err(error) => {
                if error_invalidates_client(&error) {
                    self.evict_entry_if_current(&id, &entry).await;
                }
                Err(error)
            },
        }
    }

    async fn pooled_entry(
        &self,
        server: &McpServerConfig,
    ) -> Result<(ServerId, Arc<PooledClient>), McpPoolError> {
        self.ensure_pooled(server).await?;
        let id = ServerId::from_config(server);
        let entry = {
            let pool = self.pool.lock().await;
            pool.entries.get(&id).cloned()
        };
        let entry = entry.ok_or_else(|| McpPoolError::ServerNotFound {
            server: server.name.clone(),
        })?;
        Ok((id, entry))
    }

    /// Probe initialized MCP connections without silently respawning them.
    pub(crate) async fn health(&self) -> Result<(), McpPoolError> {
        let entries: Vec<_> = {
            let pool = self.pool.lock().await;
            pool.entries.values().cloned().collect()
        };
        for entry in entries {
            match entry.as_ref() {
                PooledClient::Stdio(client) => {
                    if !client_healthy(client) {
                        return Err(McpPoolError::UnhealthyProcess);
                    }
                    let _request = client.request_lock.lock().await;
                    let next_id = client.next_id.fetch_add(1, Ordering::SeqCst);
                    let result = request_result_stdio(
                        client,
                        protocol::list_tools_request(next_id),
                        next_id,
                    )
                    .await?;
                    protocol::parse_list_tools(result).map_err(McpPoolError::Result)?;
                },
                PooledClient::Http(client) => {
                    client.list_tools().await?;
                },
            }
        }
        Ok(())
    }

    /// Shut down all pooled processes gracefully (stdin close → wait → kill).
    pub(crate) async fn shutdown(&self) {
        let entries = {
            let mut pool = self.pool.lock().await;
            pool.entries
                .drain()
                .map(|(_, entry)| entry)
                .collect::<Vec<_>>()
        };
        for entry in entries {
            shutdown_pooled(entry).await;
        }
    }

    /// Drop pooled clients that are no longer referenced by any warm cache.
    pub(crate) async fn retain_servers(&self, servers: &[McpServerConfig]) {
        let active = servers
            .iter()
            .map(ServerId::from_config)
            .collect::<HashSet<_>>();
        let stale = {
            let mut pool = self.pool.lock().await;
            let stale_ids = pool
                .entries
                .keys()
                .filter(|id| !active.contains(*id))
                .cloned()
                .collect::<Vec<_>>();
            stale_ids
                .into_iter()
                .filter_map(|id| pool.entries.remove(&id))
                .collect::<Vec<_>>()
        };
        for client in stale {
            shutdown_pooled(client).await;
        }
    }

    async fn evict_entry_if_current(&self, id: &ServerId, expected: &Arc<PooledClient>) {
        let removed = {
            let mut pool = self.pool.lock().await;
            let is_current = pool
                .entries
                .get(id)
                .is_some_and(|entry| Arc::ptr_eq(entry, expected));
            is_current.then(|| pool.entries.remove(id)).flatten()
        };
        if let Some(entry) = removed {
            shutdown_pooled(entry).await;
        }
    }

    /// Ensure a server has a live pooled client. If the current one is dead,
    /// respawn and reinitialize.
    async fn ensure_pooled(&self, server: &McpServerConfig) -> Result<(), McpPoolError> {
        let id = ServerId::from_config(server);
        let removed = {
            let mut pool = self.pool.lock().await;
            match pool.entries.get(&id) {
                Some(entry) => match entry.as_ref() {
                    PooledClient::Stdio(client) if client_healthy(client) => return Ok(()),
                    PooledClient::Stdio(_) => pool.entries.remove(&id),
                    PooledClient::Http(_) => {
                        // HTTP health is validated by requests; session-expired responses
                        // evict the entry and force a fresh initialize on retry.
                        return Ok(());
                    },
                },
                None => None,
            }
        };
        if let Some(removed) = removed {
            shutdown_pooled(removed).await;
        }

        let candidate = match server.transport {
            crate::config::McpTransport::Stdio => {
                let client = spawn_stdio(server, self.timeout).await?;
                Arc::new(PooledClient::Stdio(Box::new(client)))
            },
            crate::config::McpTransport::Http => {
                let client = HttpPooledClient::initialize(server, self.timeout).await?;
                Arc::new(PooledClient::Http(Box::new(client)))
            },
        };
        let discarded = {
            let mut pool = self.pool.lock().await;
            match pool.entries.entry(id) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(candidate);
                    None
                },
                std::collections::hash_map::Entry::Occupied(_) => Some(candidate),
            }
        };
        if let Some(discarded) = discarded {
            shutdown_pooled(discarded).await;
        }
        Ok(())
    }
}

async fn list_tools_from_entry(entry: &PooledClient) -> Result<Vec<McpTool>, McpPoolError> {
    match entry {
        PooledClient::Stdio(client) => {
            let _request = client.request_lock.lock().await;
            let next_id = client.next_id.fetch_add(1, Ordering::SeqCst);
            let result =
                request_result_stdio(client, protocol::list_tools_request(next_id), next_id)
                    .await?;
            protocol::parse_list_tools(result).map_err(McpPoolError::Result)
        },
        PooledClient::Http(client) => client.list_tools().await,
    }
}

async fn call_tool_from_entry(
    entry: &PooledClient,
    tool_name: &str,
    arguments: Value,
) -> Result<CallToolResult, McpPoolError> {
    match entry {
        PooledClient::Stdio(client) => {
            let _request = client.request_lock.lock().await;
            let next_id = client.next_id.fetch_add(1, Ordering::SeqCst);
            let result = request_result_stdio(
                client,
                protocol::call_tool_request(next_id, tool_name, arguments),
                next_id,
            )
            .await?;
            protocol::parse_call_tool(result).map_err(McpPoolError::Result)
        },
        PooledClient::Http(client) => client.call_tool(tool_name, arguments).await,
    }
}

fn should_retry_after_reconnect(error: &McpPoolError) -> bool {
    error_invalidates_client(error)
}

fn should_retry_call_after_reconnect(error: &McpPoolError) -> bool {
    matches!(error, McpPoolError::HttpSessionExpired { .. })
}

fn error_invalidates_client(error: &McpPoolError) -> bool {
    matches!(
        error,
        McpPoolError::Write(_)
            | McpPoolError::Stdout(_)
            | McpPoolError::ParseResponse { .. }
            | McpPoolError::Timeout { .. }
            | McpPoolError::Closed { .. }
            | McpPoolError::MismatchedResponse { .. }
            | McpPoolError::HttpSessionExpired { .. }
    )
}

// ─── Stdio helpers ─────────────────────────────────────────────────────

async fn shutdown_stdio_ref(client: &StdioPooledClient) {
    let child_opt = {
        let Ok(mut guard) = client.child.lock() else {
            return;
        };
        guard.take()
    };
    if let Some(mut child) = child_opt {
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

async fn shutdown_pooled(client: Arc<PooledClient>) {
    match Arc::try_unwrap(client) {
        Ok(PooledClient::Stdio(client)) => shutdown_stdio(*client).await,
        Ok(PooledClient::Http(_)) => {},
        Err(client) => {
            if let PooledClient::Stdio(client) = client.as_ref() {
                shutdown_stdio_ref(client).await;
            }
        },
    }
}

// ─── Stdio client ────────────────────────────────────────────────────────

async fn spawn_stdio(
    server: &McpServerConfig,
    timeout: Duration,
) -> Result<StdioPooledClient, McpPoolError> {
    let mut command = Command::new(&server.command);
    command
        .args(&server.args)
        .envs(&server.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide_command_window(&mut command);
    if let Some(cwd) = &server.cwd {
        command.current_dir(cwd);
    }

    let mut child = command.spawn().map_err(|source| McpPoolError::Spawn {
        server: server.name.clone(),
        source,
    })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| McpPoolError::MissingPipe {
            server: server.name.clone(),
            stream: "stdin",
        })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| McpPoolError::MissingPipe {
            server: server.name.clone(),
            stream: "stdout",
        })?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| McpPoolError::MissingPipe {
            server: server.name.clone(),
            stream: "stderr",
        })?;

    let (tx, responses) = mpsc::unbounded_channel();
    let stdout_task = tokio::spawn(read_stdout(stdout, tx));
    let stderr_buffer = Arc::new(AsyncMutex::new(TailBuffer::new(STDERR_TAIL_BYTES)));
    let stderr_task = tokio::spawn(drain_stderr(stderr, Arc::clone(&stderr_buffer)));

    let client = StdioPooledClient {
        child: std::sync::Mutex::new(Some(child)),
        stdin: AsyncMutex::new(stdin),
        responses: AsyncMutex::new(responses),
        request_lock: AsyncMutex::new(()),
        next_id: AtomicU64::new(2),
        timeout,
        stderr_buffer,
        _stdout_task: stdout_task,
        _stderr_task: stderr_task,
    };

    initialize_stdio(&client).await?;

    Ok(client)
}

fn client_healthy(client: &StdioPooledClient) -> bool {
    match client.child.lock() {
        Ok(mut guard) => match guard.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        },
        Err(_) => false,
    }
}

async fn initialize_stdio(client: &StdioPooledClient) -> Result<(), McpPoolError> {
    request_result_stdio(client, protocol::initialize_request(1), 1).await?;
    send_stdio(client, protocol::initialized_notification()).await
}

async fn request_result_stdio(
    client: &StdioPooledClient,
    message: Value,
    id: u64,
) -> Result<Value, McpPoolError> {
    send_stdio(client, message).await?;
    read_result_stdio(client, id).await
}

async fn send_stdio(client: &StdioPooledClient, message: Value) -> Result<(), McpPoolError> {
    let mut stdin = client.stdin.lock().await;
    stdin
        .write_all(protocol::serialize_line(&message).as_bytes())
        .await
        .map_err(McpPoolError::Write)?;
    stdin.flush().await.map_err(McpPoolError::Write)
}

async fn read_result_stdio(client: &StdioPooledClient, id: u64) -> Result<Value, McpPoolError> {
    let mut responses = client.responses.lock().await;
    loop {
        let received = tokio::time::timeout(client.timeout, responses.recv()).await;
        let received = match received {
            Ok(r) => r,
            Err(_) => {
                return Err(McpPoolError::Timeout {
                    id,
                    timeout_ms: client.timeout.as_millis(),
                    stderr: stderr_tail_stdio(client).await,
                });
            },
        };
        let Some(next) = received else {
            return Err(McpPoolError::Closed {
                id,
                stderr: stderr_tail_stdio(client).await,
            });
        };
        let response = next?;

        let Some(actual_id) = response.id else {
            continue; // notification — skip
        };
        if actual_id != id {
            return Err(McpPoolError::MismatchedResponse {
                expected: id,
                actual: Some(actual_id),
                stderr: stderr_tail_stdio(client).await,
            });
        }
        if let Some(error) = response.error {
            return Err(McpPoolError::Rpc {
                code: error.code,
                message: error.message,
                stderr: stderr_tail_stdio(client).await,
            });
        }
        return Ok(response.result.unwrap_or(Value::Null));
    }
}

async fn stderr_tail_stdio(client: &StdioPooledClient) -> String {
    client.stderr_buffer.lock().await.as_string()
}

async fn shutdown_stdio(client: StdioPooledClient) {
    // Take the child out of the Mutex and drop the guard before any await
    let child_opt = {
        let Ok(mut guard) = client.child.lock() else {
            return;
        };
        guard.take()
    };
    if let Some(mut child) = child_opt {
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

// ─── Stdio I/O helpers ───────────────────────────────────────────────────

async fn read_stdout(
    stdout: tokio::process::ChildStdout,
    tx: mpsc::UnboundedSender<Result<JsonRpcResponse, McpPoolError>>,
) {
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let parsed = serde_json::from_str::<JsonRpcResponse>(&line).map_err(|source| {
                    McpPoolError::ParseResponse {
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
                let _ = tx.send(Err(McpPoolError::Stdout(error)));
                return;
            },
        }
    }
}

async fn drain_stderr(stderr: tokio::process::ChildStderr, tail: Arc<AsyncMutex<TailBuffer>>) {
    use tokio::io::AsyncReadExt;
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

#[cfg(windows)]
fn hide_command_window(command: &mut Command) {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_command_window(_: &mut Command) {}

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
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Mutex,
    };

    use super::*;
    use crate::config::McpTransport;

    #[tokio::test]
    async fn reuses_prewarmed_stdio_process_and_recovers_after_exit() {
        let server = fake_stdio_server();
        let pool = McpProcessPool::new(Duration::from_secs(5));

        let warmed = pool.pre_warm(std::slice::from_ref(&server.config)).await;
        assert!(warmed[0].1.is_ok());
        let tools = pool.list_tools(&server.config).await.unwrap();
        let result = pool
            .call_tool(&server.config, "echo", json!({"text": "hello"}))
            .await
            .unwrap();

        assert_eq!(tools[0].name, "echo");
        assert_eq!(protocol::render_call_content(&result), "called");
        assert_eq!(
            fs::read_to_string(&server.marker).unwrap().lines().count(),
            1
        );
        assert!(pool.health().await.is_ok());

        let entry = {
            let pool = pool.pool.lock().await;
            pool.entries
                .get(&ServerId::from_config(&server.config))
                .cloned()
                .unwrap()
        };
        let PooledClient::Stdio(client) = entry.as_ref() else {
            panic!("expected stdio MCP client");
        };
        let child = client.child.lock().unwrap().take().unwrap();
        drop(entry);
        let mut child = child;
        child.kill().await.unwrap();
        child.wait().await.unwrap();

        assert!(pool.health().await.is_err());
        assert_eq!(
            pool.list_tools(&server.config).await.unwrap()[0].name,
            "echo"
        );
        assert!(pool.health().await.is_ok());
        assert_eq!(
            fs::read_to_string(&server.marker).unwrap().lines().count(),
            2
        );
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn reuses_http_session_after_prewarm() {
        let server = TestHttpServer::start(vec![
            TestResponse::json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"protocolVersion": "2025-06-18"}
            }))
            .header("Mcp-Session-Id", "session-1"),
            TestResponse::accepted(),
            TestResponse::json(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {"tools": [{"name": "echo"}]}
            })),
            TestResponse::json(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {"tools": [{"name": "echo"}]}
            })),
            TestResponse::json(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {
                    "content": [{"type": "text", "text": "called"}],
                    "isError": false
                }
            })),
        ])
        .await;
        let config = McpServerConfig {
            name: "http-fake".into(),
            transport: McpTransport::Http,
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            url: Some(server.url()),
            headers: BTreeMap::new(),
        };
        let pool = McpProcessPool::new(Duration::from_secs(5));

        assert!(
            pool.pre_warm(std::slice::from_ref(&config)).await[0]
                .1
                .is_ok()
        );
        assert!(pool.health().await.is_ok());
        assert_eq!(pool.list_tools(&config).await.unwrap()[0].name, "echo");
        assert_eq!(
            protocol::render_call_content(
                &pool.call_tool(&config, "echo", json!({})).await.unwrap()
            ),
            "called"
        );

        let requests = server.requests().await;
        assert_eq!(requests.len(), 5);
        assert!(requests[0].body.contains("\"method\":\"initialize\""));
        assert!(requests[1].body.contains("\"notifications/initialized\""));
        assert!(requests[2].body.contains("\"method\":\"tools/list\""));
        assert!(requests[3].body.contains("\"method\":\"tools/list\""));
        assert!(requests[4].body.contains("\"method\":\"tools/call\""));
        for request in &requests[1..] {
            assert_eq!(
                request.headers.get("mcp-session-id").map(String::as_str),
                Some("session-1")
            );
        }
    }

    #[tokio::test]
    async fn reconnects_http_session_after_expired_response() {
        let server = TestHttpServer::start(vec![
            TestResponse::json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"protocolVersion": "2025-06-18"}
            }))
            .header("Mcp-Session-Id", "session-1"),
            TestResponse::accepted(),
            TestResponse::status(reqwest::StatusCode::NOT_FOUND, "expired"),
            TestResponse::json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"protocolVersion": "2025-06-18"}
            }))
            .header("Mcp-Session-Id", "session-2"),
            TestResponse::accepted(),
            TestResponse::json(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {"tools": [{"name": "echo"}]}
            })),
        ])
        .await;
        let config = McpServerConfig {
            name: "http-fake".into(),
            transport: McpTransport::Http,
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            url: Some(server.url()),
            headers: BTreeMap::new(),
        };
        let pool = McpProcessPool::new(Duration::from_secs(5));

        assert!(
            pool.pre_warm(std::slice::from_ref(&config)).await[0]
                .1
                .is_ok()
        );
        assert_eq!(pool.list_tools(&config).await.unwrap()[0].name, "echo");

        let requests = server.requests().await;
        assert_eq!(requests.len(), 6);
        assert!(requests[2].body.contains("\"method\":\"tools/list\""));
        assert_eq!(
            requests[2]
                .headers
                .get("mcp-session-id")
                .map(String::as_str),
            Some("session-1")
        );
        assert!(requests[3].body.contains("\"method\":\"initialize\""));
        assert!(requests[5].body.contains("\"method\":\"tools/list\""));
        assert_eq!(
            requests[5]
                .headers
                .get("mcp-session-id")
                .map(String::as_str),
            Some("session-2")
        );
    }

    #[tokio::test]
    async fn recovers_after_stdio_process_exits_during_call() {
        let server = fake_stdio_server_with_env(BTreeMap::from([(
            "ASTRCODE_FAKE_MCP_EXIT_ON_CALL".into(),
            "1".into(),
        )]));
        let pool = McpProcessPool::new(Duration::from_secs(5));

        assert!(
            pool.pre_warm(std::slice::from_ref(&server.config)).await[0]
                .1
                .is_ok()
        );
        let error = pool
            .call_tool(&server.config, "echo", json!({"text": "hello"}))
            .await
            .expect_err("server exits before call response");
        assert!(matches!(error, McpPoolError::Closed { .. }));

        assert_eq!(
            pool.list_tools(&server.config).await.unwrap()[0].name,
            "echo"
        );
        assert_eq!(
            fs::read_to_string(&server.marker).unwrap().lines().count(),
            2
        );
        pool.shutdown().await;
    }

    struct FakeStdioServer {
        _temp: TempDir,
        marker: PathBuf,
        config: McpServerConfig,
    }

    struct TestHttpServer {
        addr: std::net::SocketAddr,
        requests: Arc<Mutex<Vec<TestRequest>>>,
    }

    impl TestHttpServer {
        async fn start(responses: Vec<TestResponse>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let server_requests = Arc::clone(&requests);
            tokio::spawn(async move {
                for response in responses {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    server_requests
                        .lock()
                        .await
                        .push(read_request(&mut socket).await);
                    socket
                        .write_all(response.as_http().as_bytes())
                        .await
                        .unwrap();
                }
            });
            Self { addr, requests }
        }

        fn url(&self) -> String {
            format!("http://{}/mcp", self.addr)
        }

        async fn requests(&self) -> Vec<TestRequest> {
            self.requests.lock().await.clone()
        }
    }

    #[derive(Clone)]
    struct TestRequest {
        headers: BTreeMap<String, String>,
        body: String,
    }

    struct TestResponse {
        status: reqwest::StatusCode,
        headers: BTreeMap<String, String>,
        body: String,
    }

    impl TestResponse {
        fn json(body: Value) -> Self {
            Self {
                status: reqwest::StatusCode::OK,
                headers: BTreeMap::from([("Content-Type".into(), "application/json".into())]),
                body: body.to_string(),
            }
        }

        fn accepted() -> Self {
            Self {
                status: reqwest::StatusCode::ACCEPTED,
                headers: BTreeMap::new(),
                body: String::new(),
            }
        }

        fn status(status: reqwest::StatusCode, body: &str) -> Self {
            Self {
                status,
                headers: BTreeMap::new(),
                body: body.into(),
            }
        }

        fn header(mut self, key: &str, value: &str) -> Self {
            self.headers.insert(key.into(), value.into());
            self
        }

        fn as_http(&self) -> String {
            let reason = self.status.canonical_reason().unwrap_or("");
            let mut output = format!(
                "HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
                self.status.as_u16(),
                self.body.len()
            );
            for (key, value) in &self.headers {
                output.push_str(&format!("{key}: {value}\r\n"));
            }
            output.push_str("\r\n");
            output.push_str(&self.body);
            output
        }
    }

    async fn read_request(socket: &mut tokio::net::TcpStream) -> TestRequest {
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 1024];
        loop {
            let count = socket.read(&mut buffer).await.unwrap();
            bytes.extend_from_slice(&buffer[..count]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let header_end = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        let headers = String::from_utf8_lossy(&bytes[..header_end])
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_string()))
            .collect::<BTreeMap<_, _>>();
        let content_length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        while bytes.len() - header_end < content_length {
            let count = socket.read(&mut buffer).await.unwrap();
            bytes.extend_from_slice(&buffer[..count]);
        }

        TestRequest {
            headers,
            body: String::from_utf8_lossy(&bytes[header_end..header_end + content_length])
                .to_string(),
        }
    }

    fn fake_stdio_server() -> FakeStdioServer {
        fake_stdio_server_with_env(BTreeMap::new())
    }

    fn fake_stdio_server_with_env(extra_env: BTreeMap<String, String>) -> FakeStdioServer {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("fake_mcp_server.rs");
        fs::write(&source, FAKE_SERVER_SOURCE).unwrap();
        let exe = temp.path().join(exe_name("fake_mcp_server"));
        compile_fake_server(&source, &exe);
        let marker = temp.path().join("initializes.txt");
        let mut env = BTreeMap::from([(
            "ASTRCODE_FAKE_MCP_MARKER".into(),
            marker.to_string_lossy().to_string(),
        )]);
        env.extend(extra_env);
        FakeStdioServer {
            config: McpServerConfig {
                name: "fake".into(),
                transport: McpTransport::Stdio,
                command: exe.to_string_lossy().to_string(),
                args: Vec::new(),
                env,
                cwd: None,
                url: None,
                headers: BTreeMap::new(),
            },
            marker,
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
use std::{
    fs::OpenOptions,
    io::{self, BufRead, Write},
    process,
};

fn main() {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = line.unwrap();
        let id = request_id(&line);
        if line.contains("\"method\":\"initialize\"") {
            let marker = std::env::var("ASTRCODE_FAKE_MCP_MARKER").unwrap();
            writeln!(
                OpenOptions::new().create(true).append(true).open(marker).unwrap(),
                "initialized"
            ).unwrap();
            writeln!(
                stdout,
                "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"protocolVersion\":\"2025-06-18\"}}}}",
                id
            ).unwrap();
        } else if line.contains("\"method\":\"tools/list\"") {
            writeln!(
                stdout,
                "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"tools\":[{{\"name\":\"echo\"}}]}}}}",
                id
            ).unwrap();
        } else if line.contains("\"method\":\"tools/call\"") {
            if std::env::var("ASTRCODE_FAKE_MCP_EXIT_ON_CALL").ok().as_deref() == Some("1") {
                process::exit(0);
            }
            writeln!(
                stdout,
                "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"called\"}}],\"isError\":false}}}}",
                id
            ).unwrap();
        }
        stdout.flush().unwrap();
    }
}

fn request_id(line: &str) -> u64 {
    let Some(start) = line.find("\"id\":") else {
        return 0;
    };
    line[start + 5..]
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}
"#;
}
