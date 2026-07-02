//! Benchmark 适配器接口。
//!
//! 定义将外部 benchmark 数据源（如 SWE-bench）转换为 EvalCase 的 trait。
//! 本模块仅定义接口，具体适配器在外部仓库实现。

use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use walkdir::WalkDir;

use crate::{
    EvalError,
    case::{DEFAULT_TIMEOUT_SECS, EvalCase, JudgeConfig, Setup},
};

/// 外部 benchmark 适配器。
///
/// 实现此 trait 可将任意格式的 benchmark 数据转换为 astrcode eval case。
pub trait BenchmarkAdapter: Send + Sync {
    /// 适配器名称。
    fn name(&self) -> &str;

    /// 从数据源目录加载并转换为 eval cases。
    fn load_cases(&self, source: &Path) -> Result<Vec<EvalCase>, EvalError>;
}

/// SWE-bench 数据适配器：将 SWE 风格实例转换为 EvalCase。
pub struct SweBenchAdapter;

impl BenchmarkAdapter for SweBenchAdapter {
    fn name(&self) -> &str {
        "swe-bench"
    }

    fn load_cases(&self, source: &Path) -> Result<Vec<EvalCase>, EvalError> {
        let mut cases = Vec::new();

        let paths = collect_case_files(source)?;
        for path in paths {
            let loaded = load_swe_case_file(&path)?;
            let mut file_cases = map_records_to_cases(&path, loaded);
            cases.append(&mut file_cases);
        }

        if cases.is_empty() {
            return Err(EvalError::CaseLoad(format!(
                "no SWE cases loaded from {}",
                source.display()
            )));
        }
        Ok(cases)
    }
}

impl SweBenchAdapter {
    /// 从 SWE 数据源加载 case，支持本地路径和 http/https URL。
    pub async fn load_cases_from_source(&self, source: &str) -> Result<Vec<EvalCase>, EvalError> {
        if is_remote_source(source) {
            let path = download_source_file(source).await?;
            self.load_cases(&path)
        } else {
            self.load_cases(Path::new(source))
        }
    }
}

#[derive(Deserialize)]
struct SweBenchRecord {
    #[serde(alias = "instance_id")]
    id: String,
    #[serde(default)]
    problem_statement: Option<String>,
    #[serde(alias = "question", default)]
    question: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(alias = "base_commit", default)]
    base_commit: Option<String>,
    #[serde(alias = "commit", default)]
    commit: Option<String>,
    #[serde(default)]
    hints: Option<Vec<String>>,
    #[serde(alias = "command", default)]
    test_command: Option<String>,
    #[serde(alias = "test", default)]
    test: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    tags: Option<Vec<String>>,
}

fn is_remote_source(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://")
}

async fn download_source_file(source: &str) -> Result<PathBuf, EvalError> {
    let response = reqwest::get(source)
        .await
        .map_err(|e| EvalError::Setup(format!("download SWE source failed: {e}")))?;
    if !response.status().is_success() {
        return Err(EvalError::Setup(format!(
            "download SWE source failed: {} (HTTP {})",
            source,
            response.status()
        )));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| EvalError::Setup(format!("read SWE source failed: {e}")))?;

    let dir = tempfile::tempdir()
        .map_err(|e| EvalError::Setup(format!("create tempdir for SWE source: {e}")))?;
    let file_name = derive_file_name(source);
    let path = dir.path().join(file_name);

    fs::write(&path, bytes)
        .map_err(|e| EvalError::Setup(format!("write temporary SWE source: {e}")))?;
    std::mem::forget(dir); // keep temp dir for process lifetime

    Ok(path)
}

fn derive_file_name(source: &str) -> String {
    let base = source.split('?').next().unwrap_or(source);
    let file_name = Path::new(base)
        .file_name()
        .and_then(OsStr::to_str)
        .map_or("", |s| s)
        .trim();
    if file_name.is_empty() {
        "swe-bench.jsonl".to_string()
    } else {
        file_name.to_string()
    }
}

fn collect_case_files(source: &Path) -> Result<Vec<PathBuf>, EvalError> {
    if source.is_file() {
        return Ok(vec![source.to_path_buf()]);
    }
    if !source.is_dir() {
        return Err(EvalError::CaseLoad(format!(
            "SWE source not found: {}",
            source.display()
        )));
    }

    let mut paths = Vec::new();
    for entry in WalkDir::new(source)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        let path = entry.path();
        if !entry.file_type().is_file() {
            continue;
        }
        match path
            .extension()
            .and_then(OsStr::to_str)
            .map(|ext| ext.to_ascii_lowercase())
        {
            Some(ext) if ext == "json" || ext == "jsonl" => paths.push(path.to_path_buf()),
            _ => (),
        }
    }

    if paths.is_empty() {
        return Err(EvalError::CaseLoad(format!(
            "no .json/.jsonl files under {}",
            source.display()
        )));
    }
    Ok(paths)
}

fn load_swe_case_file(path: &Path) -> Result<Vec<SweBenchRecord>, EvalError> {
    let text = fs::read_to_string(path).map_err(|e| {
        EvalError::CaseLoad(format!("failed to read SWE file {}: {e}", path.display()))
    })?;

    let trimmed = text.trim();
    if trimmed.starts_with('[') {
        return serde_json::from_str::<Vec<SweBenchRecord>>(trimmed).map_err(|e| {
            EvalError::CaseLoad(format!("invalid SWE JSON array {}: {e}", path.display()))
        });
    }

    let mut records = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record = serde_json::from_str::<SweBenchRecord>(line).map_err(|e| {
            EvalError::CaseLoad(format!(
                "invalid SWE jsonl {}:{}: {e}",
                path.display(),
                line_no
            ))
        })?;
        records.push(record);
    }
    Ok(records)
}

fn map_records_to_cases(path: &Path, records: Vec<SweBenchRecord>) -> Vec<EvalCase> {
    let mut cases = Vec::new();
    for record in records {
        match map_swe_record_to_case(record) {
            Ok(case) => cases.push(case),
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "skip invalid SWE record"
                );
            },
        }
    }
    cases
}

fn pick_test_command(test_command: Option<String>, test: Option<String>) -> Option<String> {
    test_command.or(test).and_then(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn normalize_repo_url(repo: &str) -> String {
    let repo = repo.trim();
    if repo.starts_with("http://") || repo.starts_with("https://") {
        repo.to_string()
    } else {
        format!("https://github.com/{repo}.git")
    }
}

fn map_swe_record_to_case(record: SweBenchRecord) -> Result<EvalCase, EvalError> {
    let id = record.id;
    let prompt = record
        .problem_statement
        .or(record.question)
        .unwrap_or_else(|| format!("SWE case [{}]", id));
    let mut prompts = vec![prompt];
    if let Some(hints) = record.hints {
        prompts.extend(hints.into_iter().map(|hint| format!("Hint: {hint}")));
    }

    let repo = record
        .repo
        .ok_or_else(|| EvalError::CaseLoad(format!("{id}: missing repo")))?;
    let commit = record
        .base_commit
        .or(record.commit)
        .ok_or_else(|| EvalError::CaseLoad(format!("{id}: missing base commit")))?;

    let mut judges = Vec::new();
    if let Some(test_command) = pick_test_command(record.test_command, record.test) {
        judges.push(JudgeConfig::Command {
            command: test_command,
            expect_exit_code: Some(0),
        });
    }

    let mut tags = record.tags.unwrap_or_default();
    tags.push("swe-bench".to_string());

    Ok(EvalCase {
        id,
        description: "SWE benchmark case".to_string(),
        setup: Setup::Git {
            repo: normalize_repo_url(&repo),
            commit,
        },
        prompts,
        judges,
        timeout_secs: record.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS),
        tags,
    })
}
