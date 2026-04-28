//! astrcode-server binary — stdio JSON-RPC server.

use std::sync::Arc;

use astrcode_protocol::events::ClientNotification;
use astrcode_server::{
    handler::CommandHandler,
    transport::{ServerTransport, StdioTransport, write_initialize_response},
};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    tracing::info!("astrcode-server starting");

    let runtime = match astrcode_server::bootstrap::bootstrap().await {
        Ok(rt) => Arc::new(rt),
        Err(e) => {
            tracing::error!("Bootstrap failed: {e}");
            std::process::exit(1);
        },
    };

    let (cmd_tx, mut transport) = StdioTransport::new_channel();
    StdioTransport::spawn_stdin_reader(cmd_tx);
    write_initialize_response();

    let (event_tx, _) = tokio::sync::broadcast::channel(256);
    let mut handler = CommandHandler::new(runtime, event_tx.clone());

    // Background task: broadcast events → stdout
    let mut event_rx = event_tx.subscribe();
    tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            let line = astrcode_protocol::framing::to_jsonl_line(&event).unwrap_or_default();
            use std::io::Write;
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            let _ = handle.write_all(line.as_bytes());
            let _ = handle.flush();
        }
    });

    tracing::info!("Server ready");
    while let Some(cmd) = transport.read_command().await {
        if let Err(e) = handler.handle(cmd).await {
            let _ = event_tx.send(ClientNotification::Error {
                code: -32603,
                message: e.to_string(),
            });
        }
    }
    tracing::info!("Server shutting down");
}
