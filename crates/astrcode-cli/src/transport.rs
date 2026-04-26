//! In-process transport — server runs in a tokio task, no child process.

use std::sync::Arc;

use astrcode_client::transport::{ClientTransport, TransportError};
use astrcode_protocol::commands::ClientCommand;
use astrcode_protocol::events::ServerEvent;
use astrcode_server::bootstrap;
use astrcode_server::handler::CommandHandler;
use tokio::sync::{broadcast, mpsc};

/// Transport that runs the server in a background tokio task.
///
/// Commands go through an mpsc channel, events come back through a broadcast.
pub struct InProcessTransport {
    cmd_tx: mpsc::UnboundedSender<ClientCommand>,
    event_tx: broadcast::Sender<ServerEvent>,
}

impl InProcessTransport {
    /// Start the server in a background task and return a connected transport.
    pub fn start() -> Self {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<ClientCommand>();
        let (event_tx, _) = broadcast::channel::<ServerEvent>(256);
        let tx = event_tx.clone();

        tokio::spawn(async move {
            let runtime = match bootstrap::bootstrap().await {
                Ok(rt) => Arc::new(rt),
                Err(e) => {
                    let _ = tx.send(ServerEvent::Error {
                        code: -32603,
                        message: e.to_string(),
                    });
                    return;
                }
            };

            let mut handler = CommandHandler::new(runtime, tx);
            while let Some(cmd) = cmd_rx.recv().await {
                if let Err(_e) = handler.handle(cmd).await {}
            }
        });

        Self { cmd_tx, event_tx }
    }
}

#[async_trait::async_trait]
impl ClientTransport for InProcessTransport {
    async fn send(&self, command: &ClientCommand) -> Result<(), TransportError> {
        self.cmd_tx
            .send(command.clone())
            .map_err(|_| TransportError::Connection("server task ended".into()))
    }

    async fn subscribe(&self) -> Result<broadcast::Receiver<ServerEvent>, TransportError> {
        Ok(self.event_tx.subscribe())
    }
}
