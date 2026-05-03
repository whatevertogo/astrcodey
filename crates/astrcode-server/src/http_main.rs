//! HTTP/SSE server binary.
//!
//! stdio JSON-RPC remains the default `astrcode-server` binary; this entry
//! starts the additive HTTP surface.

use std::{net::SocketAddr, sync::Arc};

#[tokio::main]
async fn main() {
    let _guard = astrcode_log::init();
    tracing::info!("astrcode-http-server starting");

    let runtime = match astrcode_server::bootstrap::bootstrap().await {
        Ok(rt) => Arc::new(rt),
        Err(error) => {
            tracing::error!("Bootstrap failed: {error}");
            std::process::exit(1);
        },
    };

    let addr: SocketAddr = std::env::var("ASTRCODE_HTTP_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3847".into())
        .parse()
        .unwrap_or_else(|error| {
            tracing::error!("Invalid ASTRCODE_HTTP_ADDR: {error}");
            std::process::exit(1);
        });
    let (event_tx, _) = tokio::sync::broadcast::channel(256);
    let app = astrcode_server::http::router(runtime, event_tx);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|error| {
            tracing::error!("Failed to bind {addr}: {error}");
            std::process::exit(1);
        });

    tracing::info!("HTTP server ready at http://{addr}");
    if let Err(error) = axum::serve(listener, app).await {
        tracing::error!("HTTP server failed: {error}");
        std::process::exit(1);
    }
}
