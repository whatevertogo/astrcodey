//! stdio 传输实现 — JSON-RPC 2.0 over stdin/stdout。

use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Write},
};

use astrcode_protocol::{
    commands::ClientCommand,
    events::ClientNotification,
    framing::{
        JsonRpcMessage, PROTOCOL_VERSION, command_from_jsonrpc_request, error_message,
        from_jsonl_line, notification_to_jsonrpc_message, to_jsonl_line,
    },
    version::{InitializeRequest, InitializeResponse, ServerCapabilities, ServerInfo},
};
use tokio::sync::mpsc;

use super::{ServerTransport, TransportError};

/// stdio transport: JSON-RPC 2.0 over stdin/stdout.
pub struct StdioTransport {
    rx: mpsc::UnboundedReceiver<StdioMessage>,
    pending_commands: VecDeque<ClientCommand>,
    initialize_request_id: Option<u64>,
}

pub enum StdioMessage {
    Initialize {
        id: Option<u64>,
        request: InitializeRequest,
    },
    Command(ClientCommand),
}

impl StdioTransport {
    /// Create a channel pair: the sender for stdin reader, and the transport.
    pub fn new_channel() -> (mpsc::UnboundedSender<StdioMessage>, Self) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            tx,
            Self {
                rx,
                pending_commands: VecDeque::new(),
                initialize_request_id: None,
            },
        )
    }

    /// Spawn a background task that reads JSON-RPC lines from stdin.
    pub fn spawn_stdin_reader(tx: mpsc::UnboundedSender<StdioMessage>) {
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
                let Ok(message) = from_jsonl_line::<JsonRpcMessage>(&line) else {
                    continue;
                };
                if message.method.as_deref() == Some("initialize") {
                    let request = message.params.clone().and_then(|params| {
                        serde_json::from_value::<InitializeRequest>(params).ok()
                    });
                    if let Some(request) = request {
                        if tx
                            .send(StdioMessage::Initialize {
                                id: message.id,
                                request,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    continue;
                }
                if let Ok(cmd) = command_from_jsonrpc_request(&message) {
                    if tx.send(StdioMessage::Command(cmd)).is_err() {
                        break; // Receiver dropped
                    }
                }
            }
        });
    }

    pub fn initialize_request_id(&self) -> Option<u64> {
        self.initialize_request_id
    }
}

#[async_trait::async_trait]
impl ServerTransport for StdioTransport {
    async fn read_command(&mut self) -> Option<ClientCommand> {
        if let Some(command) = self.pending_commands.pop_front() {
            return Some(command);
        }
        while let Some(message) = self.rx.recv().await {
            match message {
                StdioMessage::Command(command) => return Some(command),
                StdioMessage::Initialize { .. } => {},
            }
        }
        None
    }

    async fn write_event(&self, event: &ClientNotification) -> Result<(), TransportError> {
        let message = notification_to_jsonrpc_message(event)?;
        let line = to_jsonl_line(&message)?;
        // Write to stdout
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        handle.write_all(line.as_bytes())?;
        handle.flush()?;
        Ok(())
    }

    async fn initialize(&mut self) -> Result<InitializeRequest, TransportError> {
        while let Some(message) = self.rx.recv().await {
            match message {
                StdioMessage::Initialize { id, request } => {
                    self.initialize_request_id = id;
                    return Ok(request);
                },
                StdioMessage::Command(command) => self.pending_commands.push_back(command),
            }
        }
        Err(TransportError::Disconnected)
    }
}

/// Write the server initialize response to stdout.
pub fn write_initialize_response(id: u64, accepted_version: u32) {
    let response = InitializeResponse {
        accepted_version,
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
    let message = JsonRpcMessage {
        jsonrpc: "2.0".into(),
        id: Some(id),
        method: None,
        params: None,
        result: serde_json::to_value(response).ok(),
        error: None,
    };
    if let Ok(line) = to_jsonl_line(&message) {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        let _ = handle.write_all(line.as_bytes());
        let _ = handle.flush();
    }
}

/// Write an initialize error response to stdout.
pub fn write_initialize_error(id: Option<u64>, code: i32, message: &str) {
    if let Ok(line) = to_jsonl_line(&error_message(id, code, message)) {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        let _ = handle.write_all(line.as_bytes());
        let _ = handle.flush();
    }
}
