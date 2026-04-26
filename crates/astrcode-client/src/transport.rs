//! Client transport abstraction.

use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};

use astrcode_protocol::commands::ClientCommand;
use astrcode_protocol::events::ServerEvent;
use astrcode_protocol::framing::{from_jsonl_line, to_jsonl_line};

/// Transport for client-server communication.
#[async_trait::async_trait]
pub trait ClientTransport: Send + Sync {
    /// Send a command without waiting for a response.
    /// Use subscribe() to receive events.
    async fn send(&self, command: &ClientCommand) -> Result<(), TransportError>;

    /// Send a command and wait for the first response event.
    /// Convenience wrapper around send + subscribe.
    async fn execute(&self, command: &ClientCommand) -> Result<ServerEvent, TransportError> {
        let mut rx = self.subscribe().await?;
        self.send(command).await?;
        loop {
            match rx.recv().await {
                Ok(event) => return Ok(event),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return Err(TransportError::StreamDisconnected);
                }
            }
        }
    }

    /// Subscribe to the server event stream.
    async fn subscribe(
        &self,
    ) -> Result<tokio::sync::broadcast::Receiver<ServerEvent>, TransportError>;
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Connection error: {0}")]
    Connection(String),
    #[error("Stream disconnected")]
    StreamDisconnected,
    #[error("Server error: {0}")]
    Server(String),
    #[error("Unexpected response")]
    UnexpectedResponse,
}

/// stdio transport that communicates with a server child process.
///
/// Spawns the server binary, writes JSON-RPC commands to its stdin,
/// reads events from its stdout.
pub struct StdioClientTransport {
    stdin: Arc<Mutex<Box<dyn Write + Send>>>,
    event_tx: tokio::sync::broadcast::Sender<ServerEvent>,
    _child: std::process::Child,
}

impl StdioClientTransport {
    /// Spawn the server binary as a child process.
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

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TransportError::Connection("No stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TransportError::Connection("No stdout".into()))?;

        let (event_tx, _) = tokio::sync::broadcast::channel(256);
        let tx = event_tx.clone();

        // Spawn reader thread for server stdout
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if line.is_empty() {
                    continue;
                }
                if let Ok(event) = from_jsonl_line::<ServerEvent>(&line) {
                    let _ = tx.send(event);
                }
            }
        });

        Ok(Self {
            stdin: Arc::new(Mutex::new(Box::new(stdin))),
            event_tx,
            _child: child,
        })
    }

    /// Write a command to server stdin.
    fn write_command(&self, cmd: &ClientCommand) -> Result<(), TransportError> {
        let line = to_jsonl_line(cmd)?;
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
    ) -> Result<tokio::sync::broadcast::Receiver<ServerEvent>, TransportError> {
        Ok(self.event_tx.subscribe())
    }
}
