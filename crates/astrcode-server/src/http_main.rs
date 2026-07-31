//! HTTP/SSE server binary.
//!
//! stdio JSON-RPC remains the default `astrcode-server` binary; this entry
//! starts the additive HTTP surface.

#![windows_subsystem = "windows"]

use std::{net::SocketAddr, sync::Arc};

#[tokio::main]
async fn main() {
    let _guard = astrcode_log::init();
    tracing::info!("astrcode-http-server starting");

    let server_app = match astrcode_server::bootstrap::bootstrap().await {
        Ok(runtime) => astrcode_server::bootstrap::ServerApp::new(Arc::new(runtime)),
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

    if let Err(error) = astrcode_server::http::run_http_server(server_app, addr).await {
        tracing::error!("HTTP server failed: {error}");
        std::process::exit(1);
    }
}
