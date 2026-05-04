//! 客户端传输层抽象。
//!
//! 定义了 `ClientTransport` trait 作为传输层接口，并提供了基于 stdio 的实现
//! `StdioClientTransport`，通过子进程的 stdin/stdout 进行 JSON-RPC 通信。

use std::{
    io::{BufRead, BufReader, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use astrcode_protocol::{
    commands::ClientCommand,
    events::ClientNotification,
    framing::{
        JsonRpcMessage, PROTOCOL_VERSION, command_to_jsonrpc_request, from_jsonl_line,
        notification_from_jsonrpc_message, to_jsonl_line,
    },
    version::{ClientInfo, InitializeRequest, InitializeResponse},
};

/// 客户端与服务端之间的传输层接口。
///
/// 提供命令发送和事件订阅两个核心能力，所有传输层实现（如 stdio、TCP 等）
/// 均需实现此 trait。
#[async_trait::async_trait]
pub trait ClientTransport: Send + Sync {
    /// 发送命令但不等待响应。
    ///
    /// 需配合 `subscribe()` 使用以接收服务端事件。
    async fn send(&self, command: &ClientCommand) -> Result<(), TransportError>;

    /// 发送命令并等待第一个响应事件。
    ///
    /// 这是 `send` + `subscribe` 的便捷封装：先订阅事件流，再发送命令，
    /// 然后循环接收直到拿到第一条有效事件。跳过因消费滞后导致的 `Lagged` 错误。
    async fn execute(&self, command: &ClientCommand) -> Result<ClientNotification, TransportError> {
        let mut rx = self.subscribe().await?;
        self.send(command).await?;
        loop {
            match rx.recv().await {
                Ok(event) => return Ok(event),
                // 消费滞后时跳过，继续等待下一条有效事件。
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return Err(TransportError::StreamDisconnected);
                },
            }
        }
    }

    /// 订阅服务端事件流，返回一个广播接收端。
    async fn subscribe(
        &self,
    ) -> Result<tokio::sync::broadcast::Receiver<ClientNotification>, TransportError>;
}

/// 传输层错误类型。
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// I/O 错误（读写失败等）。
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON 序列化/反序列化错误。
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    /// 连接建立失败。
    #[error("Connection error: {0}")]
    Connection(String),
    /// 事件流已断开。
    #[error("Stream disconnected")]
    StreamDisconnected,
    /// 服务端返回的业务错误。
    #[error("Server error: {0}")]
    Server(String),
    /// 服务端返回了不符合预期的响应。
    #[error("Unexpected response")]
    UnexpectedResponse,
}

/// 基于 stdio 的传输层实现，通过子进程与 astrcode 服务端通信。
///
/// 启动服务端二进制文件作为子进程，向其 stdin 写入 JSON-RPC 命令，
/// 从其 stdout 读取事件通知。
pub struct StdioClientTransport {
    /// 子进程的标准输入，使用 `Mutex` 保证单线程写入。
    stdin: Arc<Mutex<Box<dyn Write + Send>>>,
    /// 下一条 JSON-RPC request id。
    next_id: AtomicU64,
    /// 事件广播发送端，读取线程通过它将事件分发给所有订阅者。
    event_tx: tokio::sync::broadcast::Sender<ClientNotification>,
    /// 子进程句柄，持有以确保子进程生命周期与传输层一致。
    _child: std::process::Child,
}

impl StdioClientTransport {
    /// 启动服务端二进制文件作为子进程。
    ///
    /// - `server_binary`: 服务端可执行文件路径。
    /// - `args`: 传递给服务端的命令行参数。
    ///
    /// 会自动创建后台线程从子进程 stdout 逐行读取事件并广播。
    pub fn spawn(server_binary: &str, args: &[&str]) -> Result<Self, TransportError> {
        let mut child = std::process::Command::new(server_binary)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|e| {
                TransportError::Connection(format!("Failed to spawn {}: {}", server_binary, e))
            })?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| TransportError::Connection("No stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TransportError::Connection("No stdout".into()))?;
        let mut reader = BufReader::new(stdout);

        let initialize_id = 1;
        let initialize = InitializeRequest {
            protocol_version: PROTOCOL_VERSION,
            client_info: ClientInfo {
                name: "astrcode-client".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
        };
        let initialize_message = JsonRpcMessage {
            jsonrpc: "2.0".into(),
            id: Some(initialize_id),
            method: Some("initialize".into()),
            params: Some(serde_json::to_value(initialize)?),
            result: None,
            error: None,
        };
        let initialize_line = to_jsonl_line(&initialize_message)?;
        stdin.write_all(initialize_line.as_bytes())?;
        stdin.flush()?;

        let mut response_line = String::new();
        reader.read_line(&mut response_line)?;
        let response = from_jsonl_line::<JsonRpcMessage>(&response_line)?;
        if response.id != Some(initialize_id) {
            return Err(TransportError::Connection(
                "initialize response id mismatch".into(),
            ));
        }
        if let Some(error) = response.error {
            return Err(TransportError::Server(error.message));
        }
        let result = response
            .result
            .ok_or(TransportError::UnexpectedResponse)?;
        let response = serde_json::from_value::<InitializeResponse>(result)?;
        if response.accepted_version != PROTOCOL_VERSION {
            return Err(TransportError::Connection(format!(
                "unsupported protocol version {}",
                response.accepted_version
            )));
        }

        // 创建广播通道，容量 256 足以应对短时间的事件突发。
        let (event_tx, _) = tokio::sync::broadcast::channel(256);
        let tx = event_tx.clone();

        // 启动后台读取线程，从子进程 stdout 逐行解析事件并广播。
        std::thread::spawn(move || {
            for line in reader.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if line.is_empty() {
                    continue;
                }
                let Ok(message) = from_jsonl_line::<JsonRpcMessage>(&line) else {
                    continue;
                };
                if let Ok(event) = notification_from_jsonrpc_message(&message) {
                    let _ = tx.send(event);
                    continue;
                }
                if let Some(result) = message.result {
                    if let Ok(event) = serde_json::from_value::<ClientNotification>(result) {
                        let _ = tx.send(event);
                    }
                }
            }
        });

        Ok(Self {
            stdin: Arc::new(Mutex::new(Box::new(stdin))),
            next_id: AtomicU64::new(initialize_id + 1),
            event_tx,
            _child: child,
        })
    }

    /// 将命令序列化为 JSONL 格式并写入子进程 stdin。
    fn write_command(&self, cmd: &ClientCommand) -> Result<(), TransportError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = command_to_jsonrpc_request(cmd, id)?;
        let line = to_jsonl_line(&message)?;
        let mut stdin = self.stdin.lock().unwrap();
        stdin.write_all(line.as_bytes())?;
        stdin.flush()?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl ClientTransport for StdioClientTransport {
    async fn send(&self, command: &ClientCommand) -> Result<(), TransportError> {
        self.write_command(command)
    }

    async fn subscribe(
        &self,
    ) -> Result<tokio::sync::broadcast::Receiver<ClientNotification>, TransportError> {
        Ok(self.event_tx.subscribe())
    }
}
