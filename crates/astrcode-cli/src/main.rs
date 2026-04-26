//! astrcode CLI — multi-subcommand entry point.

mod exec;
mod transport;
mod tui;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "astrcode", version, about = "AI coding agent platform")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start interactive terminal UI
    Tui,
    /// Execute a single prompt (headless)
    Exec {
        /// The prompt text
        prompt: String,
        /// Output mode: jsonl
        #[arg(long)]
        jsonl: bool,
        /// Timeout in seconds
        #[arg(long, default_value = "300")]
        timeout: u64,
    },
    /// Start the server in standalone mode
    Server,
    /// Show version information
    Version,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Tui => {
            if let Err(e) = tui::run().await {
                eprintln!("TUI error: {}", e);
            }
        }
        Commands::Exec {
            prompt,
            jsonl,
            timeout,
        } => {
            if let Err(e) = exec::run(&prompt, jsonl, timeout).await {
                eprintln!("Exec error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Server => {
            // Server binary is astrcode-server, not this one.
            // This command is a convenience that re-execs the server binary.
            eprintln!(
                "Use 'astrcode-server' binary directly, or run 'cargo run -p astrcode-server'"
            );
        }
        Commands::Version => {
            println!("astrcode v{}", env!("CARGO_PKG_VERSION"));
            println!("protocol version: 1");
        }
    }
}
