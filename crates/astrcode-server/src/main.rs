//! astrcode-server binary — stdio JSON-RPC server.

use astrcode_protocol::events::ServerEvent;
use astrcode_server::handler::CommandHandler;
use astrcode_server::transport::{write_initialize_response, ServerTransport, StdioTransport};
use std::sync::Arc;

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
        }
    };

    let (cmd_tx, mut transport) = StdioTransport::new_channel();
    StdioTransport::spawn_stdin_reader(cmd_tx);
    write_initialize_response();

    let (event_tx, _) = tokio::sync::broadcast::channel(256);
    let mut handler = CommandHandler::new(runtime, event_tx.clone());

    // Background task: broadcast events → stdout
    let mut event_rx = event_tx.subscribe();
    tokio::spawn(async move {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    let line =
                        astrcode_protocol::framing::to_jsonl_line(&event).unwrap_or_default();
                    use std::io::Write;
                    let stdout = std::io::stdout();
                    let mut handle = stdout.lock();
                    let _ = handle.write_all(line.as_bytes());
                    let _ = handle.flush();
                }
                Err(_) => break,
            }
        }
    });

    tracing::info!("Server ready");
    while let Some(cmd) = transport.read_command().await {
        if let Err(e) = handler.handle(cmd).await {
            let _ = event_tx.send(ServerEvent::Error {
                code: -32603,
                message: e.to_string(),
            });
        }
    }
    tracing::info!("Server shutting down");
}
