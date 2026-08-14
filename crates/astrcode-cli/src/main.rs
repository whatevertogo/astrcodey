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
use astrcode_extension_sdk::transport::{TransportFeature, TransportProfile};
use astrcode_protocol::framing::PROTOCOL_VERSION;
use astrcode_server::bootstrap::{BootstrapOptions, ServerApp};
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
        transport_profile: TransportProfile::default(),
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
        /// 每完成一个 case 后追加一行的 JSONL checkpoint 路径。
        #[arg(long)]
        checkpoint_output: Option<std::path::PathBuf>,
        /// 从已有 checkpoint 恢复，严格校验并跳过已经完成的 case。
        #[arg(long, requires = "checkpoint_output")]
        resume_checkpoint: bool,
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
        /// 已运行 server 的 Astrcode 数据目录（用于定位 run.json）
        #[arg(
            long,
            conflicts_with = "server_addr",
            conflicts_with = "swe_instance_solver_binary"
        )]
        storage: Option<std::path::PathBuf>,
        /// 服务器地址（若已有运行中的 server）
        #[arg(long)]
        server_addr: Option<String>,
        /// Auth token
        #[arg(long)]
        auth_token: Option<String>,
        /// 在官方 SWE-bench x86_64 instance image 中逐例求解。
        #[arg(long)]
        swe_instance_solver_binary: Option<std::path::PathBuf>,
        /// instance server 配置；provider 必须指向可信 gateway，且不能包含秘密。
        #[arg(long, requires = "swe_instance_solver_binary")]
        swe_instance_server_config: Option<std::path::PathBuf>,
        /// 官方预构建 SWE-bench image namespace。
        #[arg(long, default_value = "swebench")]
        swe_instance_image_namespace: String,
        /// 无 NAT、仅供宿主机控制 instance server 的 Docker 网络。
        #[arg(long, default_value = "astrcode-swebench-control")]
        swe_instance_control_network: String,
        /// 按例接入隔离网络的可信 provider gateway 容器。
        #[arg(long, default_value = "astrcode-swebench-egress")]
        swe_instance_provider_gateway_container: String,
        /// instance server 使用的 HTTP(S) 白名单代理。
        #[arg(long, default_value = "http://astrcode-swebench-egress:8888")]
        swe_instance_proxy: String,
        /// 可信控制 relay 镜像；该容器不执行模型生成的命令。
        #[arg(long, default_value = "astrcode-swebench-egress:gateway")]
        swe_instance_control_relay_image: String,
        /// instance session、server log 与元数据审计目录。
        #[arg(long, default_value = "target/astrcode-eval/swebench-instance-audit")]
        swe_instance_audit_dir: std::path::PathBuf,
        /// instance 容器名前缀；正式运行应包含唯一 run ID。
        #[arg(long, default_value = "astrcode-swebench-instance")]
        swe_instance_container_prefix: String,
        /// 每例求解后立即调用官方 harness 判分，再删除 instance image。
        #[arg(long, requires = "swe_instance_solver_binary")]
        swe_instance_streaming_harness: bool,
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
async fn bootstrap_server_app(transport_profile: TransportProfile) -> Arc<ServerApp> {
    match astrcode_server::bootstrap::bootstrap_with(BootstrapOptions {
        transport_profile,
        ..BootstrapOptions::default()
    })
    .await
    {
        Ok(runtime) => {
            let app = ServerApp::new(Arc::new(runtime));
            app.initialize().await;
            app
        },
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
            let server_app =
                bootstrap_server_app(TransportProfile::new([TransportFeature::AuthenticatedHttp]))
                    .await;
            if let Err(e) = astrcode_server::http::run_http_server(server_app, addr).await {
                tracing::error!("Server failed: {e}");
                return ExitCode::from(1);
            }
        },
        Commands::Acp => {
            let server_app = bootstrap_server_app(TransportProfile::default()).await;
            if let Err(e) = astrcode_server::acp::run_acp_server(server_app).await {
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
            checkpoint_output,
            resume_checkpoint,
            format,
            concurrency,
            tags,
            keep_workdir,
            storage,
            server_addr,
            auth_token,
            swe_instance_solver_binary,
            swe_instance_server_config,
            swe_instance_image_namespace,
            swe_instance_control_network,
            swe_instance_provider_gateway_container,
            swe_instance_proxy,
            swe_instance_control_relay_image,
            swe_instance_audit_dir,
            swe_instance_container_prefix,
            swe_instance_streaming_harness,
            swe_predictions_output,
            swe_harness,
            swe_harness_dataset,
            swe_harness_split,
            swe_harness_run_id,
            swe_harness_max_workers,
            swe_harness_python,
        } => {
            let streaming_harness = swe_instance_streaming_harness.then(|| {
                astrcode_eval::SweBenchStreamingHarnessConfig {
                    python: std::path::PathBuf::from(&swe_harness_python),
                    dataset_name: swe_harness_dataset.clone(),
                    split: swe_harness_split.clone(),
                    run_id: format!("{swe_harness_run_id}-streaming"),
                    timeout_secs: 1800,
                }
            });
            let swe_bench_instance = match (swe_instance_solver_binary, swe_instance_server_config)
            {
                (Some(solver_binary), Some(server_config)) => {
                    Some(astrcode_eval::SweBenchInstanceConfig {
                        solver_binary,
                        server_config,
                        image_namespace: swe_instance_image_namespace,
                        control_network: swe_instance_control_network,
                        provider_gateway_container: swe_instance_provider_gateway_container,
                        proxy_url: swe_instance_proxy,
                        control_relay_image: swe_instance_control_relay_image,
                        audit_dir: swe_instance_audit_dir,
                        container_prefix: swe_instance_container_prefix,
                        streaming_harness,
                    })
                },
                (Some(_), None) => {
                    eprintln!("--swe-instance-solver-binary requires --swe-instance-server-config");
                    return ExitCode::from(2);
                },
                (None, Some(_)) => {
                    eprintln!("--swe-instance-server-config requires --swe-instance-solver-binary");
                    return ExitCode::from(2);
                },
                (None, None) => None,
            };
            let config = astrcode_eval::EvalConfig {
                cases_dir: cases,
                source: swe.map_or(astrcode_eval::EvalSource::TomlDir, swe_to_source),
                concurrency,
                tags_filter: tags,
                keep_workdir,
                storage_root: storage,
                server_addr,
                auth_token,
                checkpoint_path: checkpoint_output,
                resume_checkpoint,
                swe_bench_instance,
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
                    if !report.all_passed() {
                        return ExitCode::from(1);
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
