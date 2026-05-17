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
    // TODO: 更好的capacity？
    let (event_tx, _) = tokio::sync::broadcast::channel(512);
    let shutdown_token = runtime.shutdown_token.clone();
    let (app, auth_token) =
        astrcode_server::http::router(runtime, event_tx).unwrap_or_else(|error| {
            tracing::error!("Failed to initialize HTTP router: {error}");
            std::process::exit(1);
        });
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|error| {
            tracing::error!("Failed to bind {addr}: {error}");
            std::process::exit(1);
        });

    tracing::info!("HTTP server ready at http://{addr}");
    tracing::info!(
        "Auth token: {}...{}",
        &auth_token[..4],
        &auth_token[auth_token.len() - 4..]
    );
    if let Err(error) = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_token.cancelled().await;
            tracing::info!("graceful shutdown triggered");
        })
        .await
    {
        tracing::error!("HTTP server failed: {error}");
        std::process::exit(1);
    }
}
