//! astrcode-eval — 自动化评测框架。
//!
//! 通过 HTTP 操控内嵌 server 执行 eval case，从 event log 提取 metrics，
//! 运行 judge 判定，输出结构化报告。

pub mod adapter;
pub mod case;
pub mod client;
mod git;
pub mod judge;
pub mod metrics;
pub mod report;
pub mod runner;
pub mod setup;
mod swebench_instance;

use std::path::PathBuf;

pub use adapter::{BenchmarkAdapter, SweBenchAdapter};
pub use report::EvalReport;
pub use runner::EvalRunner;

/// Eval 全局配置。
#[derive(Debug, Clone)]
pub struct EvalConfig {
    /// eval case 目录路径。
    pub cases_dir: PathBuf,
    /// 用例来源。
    pub source: EvalSource,
    /// 最大并发 case 数。
    pub concurrency: usize,
    /// 按 tag 过滤 case。
    pub tags_filter: Option<Vec<String>>,
    /// 是否保留临时工作目录供调试。
    pub keep_workdir: bool,
    /// 服务地址（若已有运行中的 server 则指定，否则从 run.json 读取）。
    pub server_addr: Option<String>,
    /// Auth token（与 server_addr 配合使用）。
    pub auth_token: Option<String>,
    /// 每完成一个 case 后追加一行的 JSONL checkpoint 路径。
    pub checkpoint_path: Option<PathBuf>,
    /// 从已有 checkpoint 恢复，跳过已经完成的 case。
    pub resume_checkpoint: bool,
    /// 存储根目录（eval session 数据隔离）。
    ///
    /// 指定后通过 `ASTRCODE_TEST_HOME` 环境变量注入，server 使用此目录
    /// 代替 `~/.astrcode/` 存放 session 数据。
    /// - `None`：使用自动创建的 tempdir（默认，最安全的隔离）
    /// - `Some(path)`：使用指定路径（可累积历史结果）
    pub storage_root: Option<PathBuf>,
    /// 在官方 SWE-bench instance image 中逐例启动求解服务。
    pub swe_bench_instance: Option<SweBenchInstanceConfig>,
}

/// 官方 SWE-bench instance image 求解环境配置。
#[derive(Debug, Clone)]
pub struct SweBenchInstanceConfig {
    /// 注入 instance image 的 Linux amd64 Astrcode 二进制。
    pub solver_binary: PathBuf,
    /// 只包含环境变量密钥引用的 Astrcode 配置文件。
    pub server_config: PathBuf,
    /// 官方预构建 instance image 的 Docker namespace。
    pub image_namespace: String,
    /// 可由宿主机访问、但禁用 IP masquerade 的控制网络。
    pub control_network: String,
    /// 按例接入隔离网络的可信 provider gateway 容器。
    pub provider_gateway_container: String,
    /// instance 中 HTTP(S) 请求使用的白名单代理地址。
    pub proxy_url: String,
    /// 可信控制 relay 使用的镜像；求解容器本身不接控制网络。
    pub control_relay_image: String,
    /// 每例 session、server log 与元数据的审计目录。
    pub audit_dir: PathBuf,
    /// Docker 容器名前缀；应包含本次运行 ID，避免不同运行混用。
    pub container_prefix: String,
    /// 求解完成后、删除 instance image 前执行的官方逐例判分。
    pub streaming_harness: Option<SweBenchStreamingHarnessConfig>,
}

/// 官方 SWE-bench harness 的逐例判分配置。
#[derive(Debug, Clone)]
pub struct SweBenchStreamingHarnessConfig {
    /// 已安装 swebench harness 的 Python 可执行文件。
    pub python: PathBuf,
    /// 官方 harness dataset_name。
    pub dataset_name: String,
    /// 官方 harness split。
    pub split: String,
    /// 隔离逐例判分日志的 run ID。
    pub run_id: String,
    /// 官方测试脚本的单例超时秒数。
    pub timeout_secs: u64,
}

/// eval 用例来源。
#[derive(Debug, Clone)]
pub enum EvalSource {
    /// 原有格式：目录下读取 .toml case 文件。
    TomlDir,
    /// SWE 格式实例文件（json 或 jsonl）。
    SweBench(PathBuf),
    /// SWE 格式实例 URL（json 或 jsonl）。
    SweBenchUrl(String),
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self {
            cases_dir: PathBuf::from("eval-tasks"),
            source: EvalSource::TomlDir,
            concurrency: 4,
            tags_filter: None,
            keep_workdir: false,
            server_addr: None,
            auth_token: None,
            checkpoint_path: None,
            resume_checkpoint: false,
            storage_root: None,
            swe_bench_instance: None,
        }
    }
}

/// 执行评测并返回报告。
pub async fn run_eval(config: EvalConfig) -> Result<EvalReport, EvalError> {
    // 设置存储隔离：通过 ASTRCODE_TEST_HOME 注入 eval 专用存储目录
    let _storage_dir = setup_storage_isolation(&config)?;

    let mut cases = match &config.source {
        EvalSource::TomlDir => case::load_case_set(&config.cases_dir)?,
        EvalSource::SweBench(path) => SweBenchAdapter.load_cases(path)?,
        EvalSource::SweBenchUrl(url) => SweBenchAdapter.load_cases_from_source(url).await?,
    };

    // 按 tag 过滤
    if let Some(ref tags) = config.tags_filter {
        cases.retain(|c| c.tags.iter().any(|t| tags.contains(t)));
    }

    if cases.is_empty() {
        return Err(EvalError::NoCases);
    }

    let runner = EvalRunner::start(&config).await?;
    let report = runner.run_all(cases).await?;
    Ok(report)
}

/// 设置存储隔离，返回实际使用的目录路径（需要保持存活防止 tempdir 被清理）。
fn setup_storage_isolation(config: &EvalConfig) -> Result<PathBuf, EvalError> {
    let storage_path = match &config.storage_root {
        Some(path) => {
            std::fs::create_dir_all(path)
                .map_err(|e| EvalError::Setup(format!("create storage dir: {e}")))?;
            path.clone()
        },
        None => {
            // 默认使用 tempdir 完全隔离
            let dir = tempfile::tempdir()
                .map_err(|e| EvalError::Setup(format!("create storage tempdir: {e}")))?;
            let path = dir.path().to_path_buf();
            std::mem::forget(dir); // 保持存活，eval 完成后由 OS 在进程退出时清理
            path
        },
    };
    // 注入环境变量，server 启动时 hostpaths::resolve_home_dir() 会读取
    // FIXME：是进程级副作用，多线程并发调用不安全（但当前 eval 是单进程入口，暂时可接受）
    std::env::set_var("ASTRCODE_TEST_HOME", &storage_path);
    tracing::info!(storage = %storage_path.display(), "eval storage isolated");
    Ok(storage_path)
}

/// Eval 框架错误类型。
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("No eval cases found")]
    NoCases,
    #[error("Case load error: {0}")]
    CaseLoad(String),
    #[error("Setup error: {0}")]
    Setup(String),
    #[error("Client error: {0}")]
    Client(String),
    #[error("Server error: {0}")]
    Server(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}
