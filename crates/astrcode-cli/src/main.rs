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

#[cfg(feature = "dev-mode")]
use std::path::PathBuf;
use std::{net::SocketAddr, process::ExitCode, sync::Arc};

use astrcode_core::permission::ApprovalMode;
use astrcode_protocol::framing::PROTOCOL_VERSION;
use astrcode_server::bootstrap::{BootstrapOptions, ServerRuntime};
use clap::{Parser, Subcommand};

fn cli_approval_bootstrap_opts(yolo: bool, manual: bool) -> BootstrapOptions {
    let approval_mode_override = if yolo {
        Some(ApprovalMode::Yolo)
    } else if manual {
        Some(ApprovalMode::Manual)
    } else {
        None
    };
    BootstrapOptions {
        default_approval_mode_if_unset: Some(ApprovalMode::Yolo),
        approval_mode_override,
        ..Default::default()
    }
}

#[cfg(feature = "dev-mode")]
fn swe_to_source(raw: String) -> astrcode_eval::EvalSource {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        astrcode_eval::EvalSource::SweBenchUrl(raw)
    } else {
        astrcode_eval::EvalSource::SweBench(PathBuf::from(raw))
    }
}

#[cfg(feature = "dev-mode")]
fn default_swe_predictions_output() -> PathBuf {
    PathBuf::from("target")
        .join("astrcode-eval")
        .join("swebench-predictions.jsonl")
}

#[cfg(feature = "dev-mode")]
fn write_swe_predictions(
    report: &astrcode_eval::EvalReport,
    path: &std::path::Path,
) -> Result<(), String> {
    let predictions = report
        .swe_bench_predictions_jsonl()
        .map_err(|e| format!("serialize SWE-bench predictions: {e}"))?;
    if predictions.is_empty() {
        return Err("no SWE-bench predictions were generated".to_string());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create predictions dir {}: {e}", parent.display()))?;
    }
    std::fs::write(path, predictions)
        .map_err(|e| format!("write SWE-bench predictions {}: {e}", path.display()))
}

#[cfg(feature = "dev-mode")]
struct SweHarnessConfig {
    python: String,
    dataset: String,
    split: String,
    run_id: String,
    max_workers: usize,
}

#[cfg(feature = "dev-mode")]
async fn run_swe_harness(
    predictions_path: &std::path::Path,
    config: SweHarnessConfig,
) -> Result<(), String> {
    let status = tokio::process::Command::new(&config.python)
        .args([
            "-m",
            "swebench.harness.run_evaluation",
            "--dataset_name",
            &config.dataset,
            "--predictions_path",
        ])
        .arg(predictions_path)
        .args([
            "--split",
            &config.split,
            "--max_workers",
            &config.max_workers.to_string(),
            "--run_id",
            &config.run_id,
        ])
        .status()
        .await
        .map_err(|e| format!("start SWE-bench harness: {e}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("SWE-bench harness exited with {status}"))
    }
}

/// CLI 顶层参数结构。
#[derive(Parser)]
#[command(name = "astrcode", version, about = "AI coding agent platform")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

/// 支持的子命令枚举。
#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Commands {
    /// 启动交互式终端 UI（默认）
    Tui {
        /// 工具审批：跳过 Ask，自动放行（覆盖 config 中的 approvalMode）
        #[arg(long)]
        yolo: bool,
        /// 工具审批：敏感操作需确认（覆盖 config）
        #[arg(long)]
        manual: bool,
    },
    /// 执行单次提示（无头模式）
    Exec {
        /// 提示文本
        prompt: String,
        /// 输出模式：jsonl
        #[arg(long)]
        jsonl: bool,
        /// 超时时间（秒）
        #[arg(long, default_value = "600")]
        timeout: u64,
        #[arg(long)]
        yolo: bool,
        #[arg(long)]
        manual: bool,
    },
    /// 启动 HTTP/SSE 后端服务器
    Server {
        /// 监听地址
        #[arg(long, default_value = "127.0.0.1:3847")]
        addr: SocketAddr,
    },
    /// 启动 ACP (Agent Client Protocol) stdio 服务器
    Acp,
    /// 执行自动化评测（仅 dev-mode feature 启用时可用）
    #[cfg(feature = "dev-mode")]
    Eval {
        /// eval case 目录路径
        #[arg(long, default_value = "eval-tasks")]
        cases: std::path::PathBuf,
        /// SWE-bench 数据文件、目录或 URL（json/jsonl/parquet）。设置后覆盖 --cases。
        #[arg(long)]
        swe: Option<String>,
        // TODO: Add --limit/--sample before encouraging full SWE-bench runs. Full datasets are
        // expensive and should have a first-class bounded-run path.
        /// 报告输出路径（默认 stdout）
        #[arg(long)]
        output: Option<std::path::PathBuf>,
        /// 输出格式
        #[arg(long, default_value = "json")]
        format: EvalOutputFormat,
        /// 最大并发 case 数
        #[arg(long, default_value = "4")]
        concurrency: usize,
        /// 按标签过滤
        #[arg(long, value_delimiter = ',')]
        tags: Option<Vec<String>>,
        /// 保留临时工作目录
        #[arg(long)]
        keep_workdir: bool,
        /// 存储根目录（eval 数据隔离，默认 tempdir）
        #[arg(long)]
        storage: Option<std::path::PathBuf>,
        /// 服务器地址（若已有运行中的 server）
        #[arg(long)]
        server_addr: Option<String>,
        /// Auth token
        #[arg(long)]
        auth_token: Option<String>,
        /// 将 SWE-bench predictions 聚合输出为官方 harness 可消费的 JSONL。
        #[arg(long)]
        swe_predictions_output: Option<std::path::PathBuf>,
        /// 输出 predictions 后调用官方 SWE-bench harness 进行 Docker 判分。
        #[arg(long)]
        swe_harness: bool,
        /// 官方 harness 的 dataset_name 参数。
        #[arg(long, default_value = "princeton-nlp/SWE-bench_Lite")]
        swe_harness_dataset: String,
        /// 官方 harness 的 split 参数。
        #[arg(long, default_value = "test")]
        swe_harness_split: String,
        /// 官方 harness 的 run_id 参数。
        #[arg(long, default_value = "astrcode-eval")]
        swe_harness_run_id: String,
        /// 官方 harness 的 max_workers 参数。
        #[arg(long, default_value = "1")]
        swe_harness_max_workers: usize,
        /// 用于执行 `-m swebench.harness.run_evaluation` 的 Python 命令。
        #[arg(long, default_value = "python")]
        swe_harness_python: String,
    },
    /// 显示版本信息
    Version,
}

/// 程序入口：解析命令行参数并分发到对应子命令处理函数。
async fn bootstrap_runtime() -> Arc<ServerRuntime> {
    match astrcode_server::bootstrap::bootstrap().await {
        Ok(rt) => Arc::new(rt),
        Err(e) => {
            tracing::error!("Bootstrap failed: {e}");
            std::process::exit(1);
        },
    }
}

#[cfg(feature = "dev-mode")]
#[derive(Clone, Debug, clap::ValueEnum)]
enum EvalOutputFormat {
    Json,
    Markdown,
    Md,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    // TUI 模式禁用 stderr 日志，避免破坏终端 UI
    let _guard = match &cli.command {
        None | Some(Commands::Tui { .. }) => astrcode_log::init_with(astrcode_log::LogOptions {
            stderr_enabled: false,
            ..astrcode_log::LogOptions::default()
        }),
        _ => astrcode_log::init(),
    };

    let command = cli.command.unwrap_or(Commands::Tui {
        yolo: false,
        manual: false,
    });

    match command {
        Commands::Tui { yolo, manual } => {
            if yolo && manual {
                eprintln!("error: --yolo and --manual are mutually exclusive");
                return ExitCode::from(2);
            }
            if let Err(e) = tui::run(cli_approval_bootstrap_opts(yolo, manual)).await {
                eprintln!("TUI error: {}", e);
                return ExitCode::from(1);
            }
        },
        Commands::Exec {
            prompt,
            jsonl,
            timeout,
            yolo,
            manual,
        } => {
            if yolo && manual {
                eprintln!("error: --yolo and --manual are mutually exclusive");
                return ExitCode::from(2);
            }
            if let Err(e) = exec::run(
                &prompt,
                jsonl,
                timeout,
                cli_approval_bootstrap_opts(yolo, manual),
            )
            .await
            {
                eprintln!("Exec error: {e}");
                return ExitCode::from(1);
            }
        },
        Commands::Server { addr } => {
            let runtime = bootstrap_runtime().await;
            if let Err(e) = astrcode_server::http::run_http_server(runtime, addr).await {
                tracing::error!("Server failed: {e}");
                return ExitCode::from(1);
            }
        },
        Commands::Acp => {
            let runtime = bootstrap_runtime().await;
            if let Err(e) = astrcode_server::acp::run_acp_server(runtime).await {
                tracing::error!("ACP server failed: {e}");
                return ExitCode::from(1);
            }
        },
        Commands::Version => {
            println!("astrcode v{}", env!("CARGO_PKG_VERSION"));
            println!("protocol version: {PROTOCOL_VERSION}");
        },
        #[cfg(feature = "dev-mode")]
        Commands::Eval {
            cases,
            swe,
            output,
            format,
            concurrency,
            tags,
            keep_workdir,
            storage,
            server_addr,
            auth_token,
            swe_predictions_output,
            swe_harness,
            swe_harness_dataset,
            swe_harness_split,
            swe_harness_run_id,
            swe_harness_max_workers,
            swe_harness_python,
        } => {
            let config = astrcode_eval::EvalConfig {
                cases_dir: cases,
                source: swe.map_or(astrcode_eval::EvalSource::TomlDir, swe_to_source),
                concurrency,
                tags_filter: tags,
                keep_workdir,
                storage_root: storage,
                server_addr,
                auth_token,
            };
            match astrcode_eval::run_eval(config).await {
                Ok(report) => {
                    let text = match format {
                        EvalOutputFormat::Markdown | EvalOutputFormat::Md => report.to_markdown(),
                        EvalOutputFormat::Json => report.to_json(),
                    };
                    if let Some(path) = output {
                        if let Err(e) = std::fs::write(&path, &text) {
                            eprintln!("Failed to write report: {e}");
                            return ExitCode::from(1);
                        }
                    } else {
                        println!("{text}");
                    }
                    let predictions_output = match (swe_predictions_output, swe_harness) {
                        (Some(path), _) => Some(path),
                        (None, true) => Some(default_swe_predictions_output()),
                        (None, false) => None,
                    };
                    if let Some(path) = predictions_output.as_deref() {
                        if let Err(e) = write_swe_predictions(&report, path) {
                            eprintln!("Failed to write SWE-bench predictions: {e}");
                            return ExitCode::from(1);
                        }
                        eprintln!(
                            "Wrote {} SWE-bench predictions to {}",
                            report.swe_bench_prediction_count(),
                            path.display()
                        );
                    }
                    if !report.all_passed() {
                        return ExitCode::from(1);
                    }
                    if swe_harness {
                        let Some(path) = predictions_output.as_deref() else {
                            eprintln!("SWE-bench harness requires a predictions output path");
                            return ExitCode::from(1);
                        };
                        let harness_config = SweHarnessConfig {
                            python: swe_harness_python,
                            dataset: swe_harness_dataset,
                            split: swe_harness_split,
                            run_id: swe_harness_run_id,
                            max_workers: swe_harness_max_workers,
                        };
                        if let Err(e) = run_swe_harness(path, harness_config).await {
                            eprintln!("SWE-bench harness failed: {e}");
                            return ExitCode::from(1);
                        }
                    }
                },
                Err(e) => {
                    eprintln!("Eval error: {e}");
                    return ExitCode::from(1);
                },
            }
        },
    }

    ExitCode::SUCCESS
}
