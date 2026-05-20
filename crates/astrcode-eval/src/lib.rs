//! astrcode-eval — 自动化评测框架。
//!
//! 通过 HTTP 操控内嵌 server 执行 eval case，从 event log 提取 metrics，
//! 运行 judge 判定，输出结构化报告。

pub mod adapter;
pub mod case;
pub mod client;
pub mod judge;
pub mod metrics;
pub mod report;
pub mod runner;
pub mod setup;

use std::path::PathBuf;

pub use report::EvalReport;
pub use runner::EvalRunner;

/// Eval 全局配置。
#[derive(Debug, Clone)]
pub struct EvalConfig {
    /// eval case 目录路径。
    pub cases_dir: PathBuf,
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
    /// 存储根目录（eval session 数据隔离）。
    ///
    /// 指定后通过 `ASTRCODE_TEST_HOME` 环境变量注入，server 使用此目录
    /// 代替 `~/.astrcode/` 存放 session 数据。
    /// - `None`：使用自动创建的 tempdir（默认，最安全的隔离）
    /// - `Some(path)`：使用指定路径（可累积历史结果）
    pub storage_root: Option<PathBuf>,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self {
            cases_dir: PathBuf::from("eval-tasks"),
            concurrency: 4,
            tags_filter: None,
            keep_workdir: false,
            server_addr: None,
            auth_token: None,
            storage_root: None,
        }
    }
}

/// 执行评测并返回报告。
pub async fn run_eval(config: EvalConfig) -> Result<EvalReport, EvalError> {
    // 设置存储隔离：通过 ASTRCODE_TEST_HOME 注入 eval 专用存储目录
    let _storage_dir = setup_storage_isolation(&config)?;

    let mut cases = case::load_case_set(&config.cases_dir)?;

    // 按 tag 过滤
    if let Some(ref tags) = config.tags_filter {
        cases.retain(|c| c.tags.iter().any(|t| tags.contains(t)));
    }

    if cases.is_empty() {
        return Err(EvalError::NoCases);
    }

    let runner = EvalRunner::start(&config).await?;
    let report = runner.run_all(cases).await;
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
