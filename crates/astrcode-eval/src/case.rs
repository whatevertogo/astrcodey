//! Eval case 定义与加载。

use std::path::Path;

use serde::Deserialize;
use walkdir::WalkDir;

use crate::EvalError;

/// 单个评测用例。
#[derive(Debug, Clone, Deserialize)]
pub struct EvalCase {
    /// 用例唯一标识。
    pub id: String,
    /// 用例描述。
    #[serde(default)]
    pub description: String,
    /// 工作目录准备策略。
    #[serde(default)]
    pub setup: Setup,
    /// 用户 prompt 列表（按顺序提交）。
    pub prompts: Vec<String>,
    /// 判定条件列表。
    pub judges: Vec<JudgeConfig>,
    /// 超时秒数。
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// 标签（用于过滤）。
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_timeout() -> u64 {
    300
}

/// 工作目录准备策略。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Setup {
    /// 从模板目录复制。
    Template { path: String },
    /// Git clone + checkout。
    Git { repo: String, commit: String },
    /// 空临时目录。
    #[default]
    Empty,
}

/// Judge 配置（从 TOML 声明式定义）。
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JudgeConfig {
    /// 执行命令检查 exit code。
    Command {
        command: String,
        #[serde(default)]
        expect_exit_code: Option<i32>,
    },
    /// 检查文件内容。
    FileContains {
        path: String,
        #[serde(default)]
        contains: Option<String>,
        #[serde(default)]
        not_contains: Option<String>,
    },
    /// 检查文件是否存在。
    FileExists {
        path: String,
        #[serde(default = "default_true")]
        exists: bool,
    },
    /// 检查 event log 条件。
    EventLog { condition: String },
}

fn default_true() -> bool {
    true
}

/// 从目录递归加载所有 .toml case 文件。
pub fn load_case_set(dir: &Path) -> Result<Vec<EvalCase>, EvalError> {
    if !dir.exists() {
        return Err(EvalError::CaseLoad(format!(
            "cases directory not found: {}",
            dir.display()
        )));
    }

    let mut cases = Vec::new();
    for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "toml") {
            match load_single_case(path) {
                Ok(case) => cases.push(case),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping invalid case file");
                },
            }
        }
    }
    Ok(cases)
}

fn load_single_case(path: &Path) -> Result<EvalCase, EvalError> {
    let content = std::fs::read_to_string(path).map_err(|e| EvalError::CaseLoad(e.to_string()))?;
    toml::from_str(&content).map_err(|e| EvalError::CaseLoad(format!("{path:?}: {e}")))
}
