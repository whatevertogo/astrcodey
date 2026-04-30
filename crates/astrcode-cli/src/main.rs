//! astrcode CLI —— 多子命令入口点。
//!
//! 提供 `tui`（交互式终端）、`exec`（无头执行）、`server`（独立服务器）、`version` 四个子命令。

mod exec;
mod transport;
mod tui;

use clap::{Parser, Subcommand};

/// CLI 顶层参数结构。
#[derive(Parser)]
#[command(name = "astrcode", version, about = "AI coding agent platform")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// 支持的子命令枚举。
#[derive(Subcommand)]
enum Commands {
    /// 启动交互式终端 UI
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
    /// 以独立模式启动服务器
    Server,
    /// 显示版本信息
    Version,
}

/// 程序入口：解析命令行参数并分发到对应子命令处理函数。
#[tokio::main]
async fn main() {
    let _guard = astrcode_log::init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Tui => {
            if let Err(e) = tui::run().await {
                eprintln!("TUI error: {}", e);
            }
        },
        Commands::Exec {
            prompt,
            jsonl,
            timeout,
        } => {
            if let Err(e) = exec::run(&prompt, jsonl, timeout).await {
                eprintln!("Exec error: {}", e);
                std::process::exit(1);
            }
        },
        Commands::Server => {
            // 服务器二进制文件是 astrcode-server，不是当前这个。
            // 此命令仅为便利提示，引导用户使用正确的二进制文件。
            eprintln!(
                "Use 'astrcode-server' binary directly, or run 'cargo run -p astrcode-server'"
            );
        },
        Commands::Version => {
            println!("astrcode v{}", env!("CARGO_PKG_VERSION"));
            println!("protocol version: 1");
        },
    }
}
