//! Benchmark 适配器接口。
//!
//! 定义将外部 benchmark 数据源（如 SWE-bench）转换为 EvalCase 的 trait。
//! 本模块仅定义接口，具体适配器在外部仓库实现。

use std::{
    ffi::OsStr,
    fs,
    fs::File,
    path::{Path, PathBuf},
};

use parquet::{
    file::reader::{FileReader, SerializedFileReader},
    record::{Field, Row},
};
use serde::Deserialize;
use serde_json::{Map, Number, Value};
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
    hints_text: Option<String>,
    #[serde(rename = "FAIL_TO_PASS", default)]
    fail_to_pass: Option<String>,
    #[serde(rename = "PASS_TO_PASS", default)]
    pass_to_pass: Option<String>,
    #[serde(default)]
    test_patch: Option<String>,
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
            Some(ext) if ext == "json" || ext == "jsonl" || ext == "parquet" => {
                paths.push(path.to_path_buf())
            },
            _ => (),
        }
    }

    if paths.is_empty() {
        return Err(EvalError::CaseLoad(format!(
            "no .json/.jsonl/.parquet files under {}",
            source.display()
        )));
    }
    Ok(paths)
}

fn is_parquet_file(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|ext| ext.eq_ignore_ascii_case("parquet"))
}

fn load_swe_case_file(path: &Path) -> Result<Vec<SweBenchRecord>, EvalError> {
    if is_parquet_file(path) {
        return load_swe_parquet_file(path);
    }

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

fn load_swe_parquet_file(path: &Path) -> Result<Vec<SweBenchRecord>, EvalError> {
    let file = File::open(path).map_err(|e| {
        EvalError::CaseLoad(format!(
            "failed to open SWE parquet {}: {e}",
            path.display()
        ))
    })?;
    let reader = SerializedFileReader::new(file).map_err(|e| {
        EvalError::CaseLoad(format!(
            "failed to read SWE parquet {}: {e}",
            path.display()
        ))
    })?;
    let rows = reader.get_row_iter(None).map_err(|e| {
        EvalError::CaseLoad(format!(
            "failed to iterate SWE parquet rows {}: {e}",
            path.display()
        ))
    })?;

    let mut records = Vec::new();
    for (index, row) in rows.enumerate() {
        let row = row.map_err(|e| {
            EvalError::CaseLoad(format!(
                "failed to read SWE parquet row {}:{}: {e}",
                path.display(),
                index + 1
            ))
        })?;
        let value = parquet_row_to_json(row);
        let record = serde_json::from_value::<SweBenchRecord>(value).map_err(|e| {
            EvalError::CaseLoad(format!(
                "invalid SWE parquet row {}:{}: {e}",
                path.display(),
                index + 1
            ))
        })?;
        records.push(record);
    }
    Ok(records)
}

fn parquet_row_to_json(row: Row) -> Value {
    Value::Object(
        row.into_columns()
            .into_iter()
            .map(|(name, field)| (name, parquet_field_to_json(field)))
            .collect::<Map<_, _>>(),
    )
}

fn parquet_field_to_json(field: Field) -> Value {
    match field {
        Field::Null => Value::Null,
        Field::Bool(value) => Value::Bool(value),
        Field::Byte(value) => Number::from(value).into(),
        Field::Short(value) => Number::from(value).into(),
        Field::Int(value) => Number::from(value).into(),
        Field::Long(value) => Number::from(value).into(),
        Field::UByte(value) => Number::from(value).into(),
        Field::UShort(value) => Number::from(value).into(),
        Field::UInt(value) => Number::from(value).into(),
        Field::ULong(value) => Number::from(value).into(),
        Field::Float16(value) => Number::from_f64(value.to_f32() as f64)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Field::Float(value) => Number::from_f64(value as f64)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Field::Double(value) => Number::from_f64(value)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Field::Decimal(value) => Value::String(format!("{value:?}")),
        Field::Str(value) => Value::String(value),
        Field::Bytes(value) => Value::String(
            value
                .as_utf8()
                .map(str::to_string)
                .unwrap_or_else(|_| String::from_utf8_lossy(value.data()).into_owned()),
        ),
        Field::Date(value) => Number::from(value).into(),
        Field::TimeMillis(value) => Number::from(value).into(),
        Field::TimeMicros(value) => Number::from(value).into(),
        Field::TimestampMillis(value) => Number::from(value).into(),
        Field::TimestampMicros(value) => Number::from(value).into(),
        Field::Group(value) => parquet_row_to_json(value),
        Field::ListInternal(value) => Value::Array(
            value
                .elements()
                .iter()
                .cloned()
                .map(parquet_field_to_json)
                .collect(),
        ),
        Field::MapInternal(value) => Value::Object(
            value
                .entries()
                .iter()
                .map(|(key, value)| {
                    (
                        parquet_field_to_map_key(key),
                        parquet_field_to_json(value.clone()),
                    )
                })
                .collect::<Map<_, _>>(),
        ),
    }
}

fn parquet_field_to_map_key(field: &Field) -> String {
    match field {
        Field::Str(value) => value.clone(),
        Field::Bytes(value) => value
            .as_utf8()
            .map(str::to_string)
            .unwrap_or_else(|_| String::from_utf8_lossy(value.data()).into_owned()),
        _ => field.to_string(),
    }
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

fn non_empty_text(raw: Option<String>) -> Option<String> {
    raw.and_then(|text| {
        let trimmed = text.trim();
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
    let official = is_official_swe_bench_record(&record);
    let id = record.id;
    let problem = record
        .problem_statement
        .or(record.question)
        .unwrap_or_else(|| format!("SWE case [{}]", id));
    let hints_text = non_empty_text(record.hints_text);
    let mut prompts = vec![problem];
    if let Some(hints) = record.hints {
        prompts.extend(hints.into_iter().map(|hint| format!("Hint: {hint}")));
    }
    if let Some(hints_text) = hints_text {
        prompts.push(format!("Hints:\n{hints_text}"));
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
    } else if official {
        judges.push(JudgeConfig::SweBenchPatch {
            instance_id: id.clone(),
        });
    }

    let mut tags = record.tags.unwrap_or_default();
    tags.push("swe-bench".to_string());
    if official {
        tags.push("official-swe-bench".to_string());
    }

    Ok(EvalCase {
        id,
        description: if official {
            "Official SWE-bench case".to_string()
        } else {
            "SWE benchmark case".to_string()
        },
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

fn is_official_swe_bench_record(record: &SweBenchRecord) -> bool {
    record
        .test_patch
        .as_ref()
        .is_some_and(|patch| !patch.is_empty())
        || record
            .fail_to_pass
            .as_ref()
            .is_some_and(|tests| !tests.is_empty())
        || record
            .pass_to_pass
            .as_ref()
            .is_some_and(|tests| !tests.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_swe_bench_record_uses_prediction_judge() {
        let record: SweBenchRecord = serde_json::from_str(
            r#"{
                "instance_id": "django__django-12345",
                "repo": "django/django",
                "base_commit": "abc123",
                "problem_statement": "Fix the issue.",
                "hints_text": "Look at the parser.",
                "FAIL_TO_PASS": "[\"tests.test_parser\"]",
                "PASS_TO_PASS": "[]",
                "test_patch": "diff --git a/tests.py b/tests.py"
            }"#,
        )
        .unwrap();

        let case = map_swe_record_to_case(record).unwrap();

        assert_eq!(case.id, "django__django-12345");
        assert_eq!(case.description, "Official SWE-bench case");
        assert!(case.tags.contains(&"official-swe-bench".to_string()));
        assert!(
            case.prompts
                .iter()
                .any(|prompt| prompt.contains("Fix the issue."))
        );
        assert!(
            case.prompts
                .iter()
                .any(|prompt| prompt.contains("Look at the parser."))
        );
        assert!(matches!(
            case.judges.as_slice(),
            [JudgeConfig::SweBenchPatch { instance_id }] if instance_id == "django__django-12345"
        ));
    }

    #[test]
    fn explicit_test_command_takes_precedence_over_official_prediction_judge() {
        let record: SweBenchRecord = serde_json::from_str(
            r#"{
                "instance_id": "custom-1",
                "repo": "owner/repo",
                "base_commit": "abc123",
                "problem_statement": "Fix it.",
                "test_patch": "diff --git a/tests.py b/tests.py",
                "command": "pytest tests/test_issue.py"
            }"#,
        )
        .unwrap();

        let case = map_swe_record_to_case(record).unwrap();

        assert!(matches!(
            case.judges.as_slice(),
            [JudgeConfig::Command { command, expect_exit_code: Some(0) }]
                if command == "pytest tests/test_issue.py"
        ));
    }

    #[test]
    fn parquet_input_loads_official_swe_bench_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-00000-of-00001.parquet");
        write_single_swe_parquet_record(&path);

        let records = load_swe_case_file(&path).unwrap();
        let case = map_swe_record_to_case(records.into_iter().next().unwrap()).unwrap();

        assert_eq!(case.id, "django__django-12345");
        assert_eq!(case.description, "Official SWE-bench case");
        assert!(matches!(
            case.judges.as_slice(),
            [JudgeConfig::SweBenchPatch { instance_id }] if instance_id == "django__django-12345"
        ));
    }

    fn write_single_swe_parquet_record(path: &Path) {
        use std::sync::Arc;

        use parquet::{
            data_type::{ByteArray, ByteArrayType},
            file::{properties::WriterProperties, writer::SerializedFileWriter},
            schema::parser::parse_message_type,
        };

        let schema = Arc::new(
            parse_message_type(
                r#"
                message swe_bench {
                    REQUIRED BINARY instance_id (UTF8);
                    REQUIRED BINARY repo (UTF8);
                    REQUIRED BINARY base_commit (UTF8);
                    REQUIRED BINARY problem_statement (UTF8);
                    REQUIRED BINARY hints_text (UTF8);
                    REQUIRED BINARY FAIL_TO_PASS (UTF8);
                    REQUIRED BINARY PASS_TO_PASS (UTF8);
                    REQUIRED BINARY test_patch (UTF8);
                }
                "#,
            )
            .unwrap(),
        );
        let props = Arc::new(WriterProperties::builder().build());
        let file = std::fs::File::create(path).unwrap();
        let mut writer = SerializedFileWriter::new(file, schema, props).unwrap();
        let mut row_group = writer.next_row_group().unwrap();

        for value in [
            "django__django-12345",
            "django/django",
            "abc123",
            "Fix the issue.",
            "Look at the parser.",
            "[\"tests.test_parser\"]",
            "[]",
            "diff --git a/tests.py b/tests.py",
        ] {
            let mut column = row_group.next_column().unwrap().unwrap();
            column
                .typed::<ByteArrayType>()
                .write_batch(&[ByteArray::from(value)], None, None)
                .unwrap();
            column.close().unwrap();
        }

        row_group.close().unwrap();
        writer.close().unwrap();
    }
}
