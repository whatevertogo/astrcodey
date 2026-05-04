//! astrcode-server 二进制入口 — 基于 stdio 的 JSON-RPC 服务器。
//!
//! 启动流程：
//! 1. 初始化日志（输出到 stderr，避免与 stdout 的 JSON-RPC 通信冲突）
//! 2. 引导服务器运行时（加载配置、初始化各组件）
//! 3. 启动 stdio 传输层（从 stdin 读取命令，向 stdout 写入事件）
//! 4. 写入初始化响应，声明服务器能力
//! 5. 进入主循环，持续处理客户端命令

use std::sync::Arc;

use astrcode_protocol::{
    events::ClientNotification,
    framing::{notification_to_jsonrpc_message, to_jsonl_line},
    framing::PROTOCOL_VERSION,
    version::negotiate_version,
};
use astrcode_server::{
    handler::CommandHandler,
    transport::{ServerTransport, StdioTransport, write_initialize_error, write_initialize_response},
};

#[tokio::main]
async fn main() {
    let _guard = astrcode_log::init();
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
    let initialize = match transport.initialize().await {
        Ok(initialize) => initialize,
        Err(e) => {
            tracing::error!("Initialize failed: {e}");
            std::process::exit(1);
        },
    };
    let request_id = transport.initialize_request_id();
    let accepted_version = negotiate_version(initialize.protocol_version, &[PROTOCOL_VERSION]);
    let Some(accepted_version) = accepted_version else {
        write_initialize_error(
            request_id,
            -32000,
            &format!("Unsupported protocol version {}", initialize.protocol_version),
        );
        std::process::exit(1);
    };
    let Some(request_id) = request_id else {
        write_initialize_error(None, -32600, "Initialize request must include an id");
        std::process::exit(1);
    };
    write_initialize_response(request_id, accepted_version);

    let (event_tx, _) = tokio::sync::broadcast::channel(256);
    let handler = CommandHandler::spawn_actor(runtime, event_tx.clone());

    // Background task: broadcast events → stdout
    let mut event_rx = event_tx.subscribe();
    tokio::spawn(async move {
        while let Ok(event) = event_rx.recv().await {
            let line = notification_to_jsonrpc_message(&event)
                .and_then(|message| to_jsonl_line(&message))
                .unwrap_or_default();
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
