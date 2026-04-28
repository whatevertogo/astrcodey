//! Server transport abstraction and stdio implementation.

use std::io::{BufRead, BufReader, Write};

use astrcode_protocol::{
    commands::ClientCommand,
    events::ClientNotification,
    framing::{PROTOCOL_VERSION, from_jsonl_line, to_jsonl_line},
    version::{InitializeRequest, InitializeResponse, ServerCapabilities, ServerInfo},
};
use tokio::sync::mpsc;

/// Transport trait for server communication.
///
/// Allows future expansion to WebSocket, TCP, etc.
#[async_trait::async_trait]
pub trait ServerTransport: Send + Sync {
    /// Read the next command from the transport.
    async fn read_command(&mut self) -> Option<ClientCommand>;

    /// Write an event to the transport.
    async fn write_event(&self, event: &ClientNotification) -> Result<(), TransportError>;

    /// Initialize the connection with version negotiation.
    async fn initialize(&mut self) -> Result<InitializeRequest, TransportError>;
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Connection closed")]
    Disconnected,
    #[error("Unsupported protocol version: {0}")]
    UnsupportedVersion(u32),
}

/// stdio transport: JSON-RPC 2.0 over stdin/stdout.
pub struct StdioTransport {
    rx: mpsc::UnboundedReceiver<ClientCommand>,
}

impl StdioTransport {
    /// Create a channel pair: the sender for stdin reader, and the transport.
    pub fn new_channel() -> (mpsc::UnboundedSender<ClientCommand>, Self) {
        let (tx, rx) = mpsc::unbounded_channel();
        (tx, Self { rx })
    }

    /// Spawn a background task that reads JSON-RPC lines from stdin.
    pub fn spawn_stdin_reader(tx: mpsc::UnboundedSender<ClientCommand>) {
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let reader = BufReader::new(stdin);
            for line in reader.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                if line.is_empty() {
                    continue;
                }
                if let Ok(cmd) = from_jsonl_line::<ClientCommand>(&line) {
                    if tx.send(cmd).is_err() {
                        break; // Receiver dropped
                    }
                }
            }
        });
    }
}

#[async_trait::async_trait]
impl ServerTransport for StdioTransport {
    async fn read_command(&mut self) -> Option<ClientCommand> {
        self.rx.recv().await
    }

    async fn write_event(&self, event: &ClientNotification) -> Result<(), TransportError> {
        let line = to_jsonl_line(event)?;
        // Write to stdout
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        handle.write_all(line.as_bytes())?;
        handle.flush()?;
        Ok(())
    }

    async fn initialize(&mut self) -> Result<InitializeRequest, TransportError> {
        // Read the first command — must be an initialize request
        // For now, assume the first command read is the init
        // TODO: Proper initialize message parsing
        Ok(InitializeRequest {
            protocol_version: PROTOCOL_VERSION,
            client_info: astrcode_protocol::version::ClientInfo {
                name: "unknown".into(),
                version: "0.1.0".into(),
            },
        })
    }
}

/// Write the server initialize response to stdout.
pub fn write_initialize_response() {
    let response = InitializeResponse {
        accepted_version: PROTOCOL_VERSION,
        server_info: ServerInfo {
            name: "astrcode-server".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            protocol_versions: vec![PROTOCOL_VERSION],
            capabilities: ServerCapabilities {
                streaming: true,
                session_fork: true,
                compaction: true,
                extensions: true,
            },
        },
    };
    if let Ok(line) = to_jsonl_line(&response) {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        let _ = handle.write_all(line.as_bytes());
        let _ = handle.flush();
    }
}
