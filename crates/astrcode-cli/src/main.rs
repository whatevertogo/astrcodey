//! astrcode CLI —— multitool 入口点。
//!
//! 单个 `astrcode` 二进制包含所有运行模式：
//! - `tui`：交互式终端（默认行为）
//! - `exec`：无头单次执行
//! - `server`：HTTP/SSE 后端服务器
//! - `version`：版本信息

mod exec;
mod transport;
mod tui;
mod tui_v2; // Phase 6: will replace tui once complete

use std::{net::SocketAddr, sync::Arc};

use clap::{Parser, Subcommand};

/// CLI 顶层参数结构。
#[derive(Parser)]
#[command(name = "astrcode", version, about = "AI coding agent platform")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

/// 支持的子命令枚举。
#[derive(Subcommand)]
enum Commands {
    /// 启动交互式终端 UI（默认）
    Tui,
    /// 执行单次提示（无头模式）
    Exec {
        /// 提示文本
        prompt: String,
        /// 输出模式：jsonl
        #[arg(long)]
        jsonl: bool,
        /// 超时时间（秒）
        #[arg(long, default_value = "300")]
        timeout: u64,
    },
    /// 启动 HTTP/SSE 后端服务器
    Server {
        /// 监听地址
        #[arg(long, default_value = "127.0.0.1:3847")]
        addr: SocketAddr,
    },
    /// 启动 ACP (Agent Client Protocol) stdio 服务器
    Acp,
    /// 显示版本信息
    Version,
}

/// 程序入口：解析命令行参数并分发到对应子命令处理函数。
#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // TUI 模式禁用 stderr 日志，避免破坏终端 UI
    let _guard = match &cli.command {
        Some(Commands::Tui) | None => astrcode_log::init_with(astrcode_log::LogOptions {
            stderr_enabled: false,
            ..astrcode_log::LogOptions::default()
        }),
        _ => astrcode_log::init(),
    };

    match cli.command {
        None | Some(Commands::Tui) => {
            if let Err(e) = tui::run().await {
                eprintln!("TUI error: {}", e);
            }
        },
        Some(Commands::Exec {
            prompt,
            jsonl,
            timeout,
        }) => {
            if let Err(e) = exec::run(&prompt, jsonl, timeout).await {
                eprintln!("Exec error: {}", e);
                std::process::exit(1);
            }
        },
        Some(Commands::Server { addr }) => {
            let runtime = match astrcode_server::bootstrap::bootstrap().await {
                Ok(rt) => Arc::new(rt),
                Err(e) => {
                    tracing::error!("Bootstrap failed: {e}");
                    std::process::exit(1);
                },
            };
            if let Err(e) = astrcode_server::http::run_http_server(runtime, addr).await {
                tracing::error!("Server failed: {e}");
                std::process::exit(1);
            }
        },
        Some(Commands::Acp) => {
            let runtime = match astrcode_server::bootstrap::bootstrap().await {
                Ok(rt) => Arc::new(rt),
                Err(e) => {
                    tracing::error!("Bootstrap failed: {e}");
                    std::process::exit(1);
                },
            };
            if let Err(e) = astrcode_server::acp::run_acp_server(runtime).await {
                tracing::error!("ACP server failed: {e}");
                std::process::exit(1);
            }
        },
        Some(Commands::Version) => {
            println!("astrcode v{}", env!("CARGO_PKG_VERSION"));
            println!("protocol version: 1");
        },
    }
}
