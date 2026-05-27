//! astrcode-server — HTTP/SSE 后端入口。
//!
//! 外部客户端（Desktop sidecar、浏览器、脚本）通过 HTTP API 连接。
//! TUI / exec 使用进程内 InProcessTransport，不经由此二进制。
#![windows_subsystem = "windows"]

use std::{net::SocketAddr, sync::Arc};

#[tokio::main]
async fn main() {
    let _guard = astrcode_log::init();
    tracing::info!("astrcode-server (HTTP) starting");

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

    if let Err(error) = astrcode_server::http::run_http_server(runtime, addr).await {
        tracing::error!("HTTP server failed: {error}");
        std::process::exit(1);
    }
}
