//! EvalRunner — 编排器，管理 server 生命周期 + 并发执行 case。

use std::{
    collections::{HashMap, HashSet},
    fs::OpenOptions,
    io::{BufRead, BufReader, Write},
    panic::AssertUnwindSafe,
    sync::Arc,
    time::Instant,
};

use futures_util::{FutureExt, StreamExt, stream::FuturesUnordered};
use tokio::sync::Semaphore;

use crate::{
    EvalConfig, EvalError,
    case::EvalCase,
    client::EvalClient,
    judge::{self, JudgeContext, SWE_BENCH_PREDICTION_FILE, Verdict},
    metrics::Metrics,
    report::{EvalReport, EvalResult, SweBenchPrediction},
    setup, swebench_instance,
};

const MAX_RETAINED_PATCH_BYTES: usize = 256 * 1024 * 1024;

/// 评测编排器。
pub struct EvalRunner {
    config: EvalConfig,
    server: Option<ServerConnection>,
    resumed_results: Vec<EvalResult>,
}

struct ServerConnection {
    addr: String,
    auth_token: String,
}

impl EvalRunner {
    /// 启动 eval runner。
    ///
    /// 如果 config 指定了 server_addr，直接使用；否则需要外部确保 server 已启动。
    pub async fn start(config: &EvalConfig) -> Result<Self, EvalError> {
        let resumed_results = match config.checkpoint_path.as_deref() {
            Some(path) if config.resume_checkpoint => load_checkpoint(path.to_path_buf()).await?,
            Some(path) => {
                initialize_checkpoint(path.to_path_buf()).await?;
                Vec::new()
            },
            None => Vec::new(),
        };
        if let Some(instance_config) = config.swe_bench_instance.as_ref() {
            if config.server_addr.is_some() || config.auth_token.is_some() {
                return Err(EvalError::Setup(
                    "official instance images cannot be combined with --server-addr or \
                     --auth-token"
                        .into(),
                ));
            }
            swebench_instance::validate(instance_config).await?;
            return Ok(Self {
                config: config.clone(),
                server: None,
                resumed_results,
            });
        }

        let (server_addr, auth_token) = match (&config.server_addr, &config.auth_token) {
            (Some(addr), Some(token)) => (addr.clone(), token.clone()),
            (Some(_), None) => {
                return Err(EvalError::Setup(
                    "--server-addr requires --auth-token".into(),
                ));
            },
            (None, Some(_)) => {
                return Err(EvalError::Setup(
                    "--auth-token requires --server-addr".into(),
                ));
            },
            (None, None) => {
                // 尝试从 ~/.astrcode/run.json 读取
                let run_info = read_run_info()?;
                (
                    format!("http://127.0.0.1:{}", run_info.port),
                    run_info.auth_token,
                )
            },
        };

        // 健康检查
        EvalClient::new(&server_addr, &auth_token)?
            .health_check()
            .await
            .map_err(|error| {
                EvalError::Server(format!("cannot use server at {server_addr}: {error}"))
            })?;

        Ok(Self {
            config: config.clone(),
            server: Some(ServerConnection {
                addr: server_addr,
                auth_token,
            }),
            resumed_results,
        })
    }

    /// 并发执行所有 case，返回报告。
    pub async fn run_all(&self, cases: Vec<EvalCase>) -> Result<EvalReport, EvalError> {
        let semaphore = Arc::new(Semaphore::new(self.config.concurrency));
        let mut tasks = FuturesUnordered::new();
        let result_capacity = cases.len();
        let case_indices: HashMap<&str, usize> = cases
            .iter()
            .enumerate()
            .map(|(index, case)| (case.id.as_str(), index))
            .collect();
        let mut completed_case_ids = HashSet::with_capacity(self.resumed_results.len());
        let mut indexed_results = Vec::with_capacity(result_capacity);
        let mut retained_patch_bytes: usize = 0;

        for result in &self.resumed_results {
            let Some(case_index) = case_indices.get(result.case_id.as_str()).copied() else {
                return Err(EvalError::Setup(format!(
                    "checkpoint contains case not present in this eval source: {}",
                    result.case_id
                )));
            };
            if !completed_case_ids.insert(result.case_id.clone()) {
                return Err(EvalError::Setup(format!(
                    "checkpoint contains duplicate case: {}",
                    result.case_id
                )));
            }
            retained_patch_bytes = retained_patch_bytes.saturating_add(
                result
                    .swe_bench_prediction
                    .as_ref()
                    .map_or(0, |prediction| prediction.model_patch.len()),
            );
            indexed_results.push((case_index, result.clone()));
        }
        if retained_patch_bytes > MAX_RETAINED_PATCH_BYTES {
            return Err(EvalError::Setup(format!(
                "checkpoint patches exceed the {MAX_RETAINED_PATCH_BYTES}-byte audit limit"
            )));
        }

        for (case_index, case) in cases.into_iter().enumerate() {
            if completed_case_ids.contains(&case.id) {
                continue;
            }
            let permit = Arc::clone(&semaphore);
            let server = self
                .server
                .as_ref()
                .map(|server| (server.addr.clone(), server.auth_token.clone()));
            let instance_config = self.config.swe_bench_instance.clone();
            let cases_dir = self.config.cases_dir.clone();
            let keep_workdir = self.config.keep_workdir;
            let case_id = case.id.clone();
            let panic_case_id = case_id.clone();

            let task = async move {
                let _permit = match permit.acquire().await {
                    Ok(permit) => permit,
                    Err(error) => {
                        return failed_eval_result(
                            case.id.clone(),
                            String::new(),
                            "eval concurrency controller stopped".into(),
                            error.to_string(),
                            0,
                        );
                    },
                };
                if let Some(instance_config) = instance_config.as_ref() {
                    run_instance_case(&case, instance_config).await
                } else if let Some((server_addr, auth_token)) = server.as_ref() {
                    run_single_case(&case, server_addr, auth_token, &cases_dir, keep_workdir).await
                } else {
                    failed_eval_result(
                        case.id.clone(),
                        String::new(),
                        "eval runner has no execution environment".to_string(),
                        "missing server and SWE-bench instance configuration".to_string(),
                        0,
                    )
                }
            };
            tasks.push(AssertUnwindSafe(task).catch_unwind().map(move |outcome| {
                let result = outcome.unwrap_or_else(|_| {
                    failed_eval_result(
                        panic_case_id,
                        String::new(),
                        "task panicked".to_string(),
                        "panic while running eval case".to_string(),
                        0,
                    )
                });
                (case_index, result)
            }));
        }

        while let Some((case_index, mut result)) = tasks.next().await {
            enforce_patch_budget(&mut result, &mut retained_patch_bytes);
            if let Some(path) = self.config.checkpoint_path.as_deref() {
                if let Err(error) = append_checkpoint(path.to_path_buf(), result.clone()).await {
                    tracing::error!(path = %path.display(), %error, "failed to update eval checkpoint");
                }
            }
            indexed_results.push((case_index, result));
        }

        indexed_results.sort_unstable_by_key(|(case_index, _)| *case_index);
        let results = indexed_results
            .into_iter()
            .map(|(_, result)| result)
            .collect();
        Ok(EvalReport::from_results(results))
    }
}

fn enforce_patch_budget(result: &mut EvalResult, retained_bytes: &mut usize) {
    let Some(prediction) = result.swe_bench_prediction.as_mut() else {
        return;
    };
    let patch_bytes = prediction.model_patch.len();
    if retained_bytes.saturating_add(patch_bytes) <= MAX_RETAINED_PATCH_BYTES {
        *retained_bytes += patch_bytes;
        return;
    }

    prediction.model_patch.clear();
    result.passed = false;
    result.verdicts = vec![Verdict::Fail {
        reason: format!(
            "aggregate SWE-bench patches exceed the {MAX_RETAINED_PATCH_BYTES}-byte audit limit"
        ),
    }];
    result.error = Some("aggregate SWE-bench patch audit limit exceeded".to_string());
}

async fn run_instance_case(case: &EvalCase, config: &crate::SweBenchInstanceConfig) -> EvalResult {
    let started = Instant::now();
    match swebench_instance::run_case(case, config).await {
        Ok(outcome) => EvalResult {
            case_id: case.id.clone(),
            session_id: outcome.session_id,
            passed: outcome.has_patch,
            verdicts: if outcome.has_patch {
                vec![Verdict::Pass]
            } else {
                vec![Verdict::Fail {
                    reason: "SWE-bench model patch is empty".to_string(),
                }]
            },
            metrics: Metrics::default(),
            duration_ms: started.elapsed().as_millis() as u64,
            swe_bench_prediction: Some(outcome.prediction),
            error: None,
        },
        Err(error) => {
            let mut result = failed_eval_result(
                case.id.clone(),
                String::new(),
                format!("official instance solve failed: {error}"),
                error.to_string(),
                started.elapsed().as_millis() as u64,
            );
            result.swe_bench_prediction = Some(swebench_instance::prediction(
                case.id.clone(),
                String::new(),
            ));
            result
        },
    }
}

async fn initialize_checkpoint(path: std::path::PathBuf) -> Result<(), std::io::Error> {
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map(|_| ())
    })
    .await
    .map_err(std::io::Error::other)?
}

async fn load_checkpoint(path: std::path::PathBuf) -> Result<Vec<EvalResult>, std::io::Error> {
    tokio::task::spawn_blocking(move || {
        let file = OpenOptions::new().read(true).append(true).open(&path)?;
        let mut results = Vec::new();
        for (line_index, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let result = serde_json::from_str(&line).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "invalid checkpoint JSON at {}:{}: {error}",
                        path.display(),
                        line_index + 1
                    ),
                )
            })?;
            results.push(result);
        }
        Ok(results)
    })
    .await
    .map_err(std::io::Error::other)?
}

async fn append_checkpoint(
    path: std::path::PathBuf,
    result: EvalResult,
) -> Result<(), std::io::Error> {
    tokio::task::spawn_blocking(move || {
        let mut file = OpenOptions::new().append(true).open(path)?;
        serde_json::to_writer(&mut file, &result).map_err(std::io::Error::other)?;
        file.write_all(b"\n")?;
        file.flush()
    })
    .await
    .map_err(std::io::Error::other)?
}

async fn run_single_case(
    case: &EvalCase,
    server_addr: &str,
    auth_token: &str,
    cases_dir: &std::path::Path,
    keep_workdir: bool,
) -> EvalResult {
    let started = Instant::now();
    let case_id = case.id.clone();

    // Setup workspace
    let work_dir = match setup::setup_workspace(&case.setup, cases_dir).await {
        Ok(dir) => dir,
        Err(e) => {
            return failed_eval_result(
                case_id,
                String::new(),
                format!("setup failed: {e}"),
                e.to_string(),
                started.elapsed().as_millis() as u64,
            );
        },
    };

    let client = match EvalClient::new(server_addr, auth_token) {
        Ok(client) => client,
        Err(error) => {
            return failed_eval_result(
                case_id,
                String::new(),
                format!("create eval client: {error}"),
                error.to_string(),
                started.elapsed().as_millis() as u64,
            );
        },
    };

    // Create session
    let session_id = match client.create_session(&work_dir.display().to_string()).await {
        Ok(id) => id,
        Err(e) => {
            return failed_eval_result(
                case_id,
                String::new(),
                format!("create_session: {e}"),
                e.to_string(),
                started.elapsed().as_millis() as u64,
            );
        },
    };

    // Submit prompts
    for prompt in &case.prompts {
        if let Err(e) = client.submit_prompt(&session_id, prompt).await {
            return failed_eval_result(
                case_id,
                session_id,
                format!("submit_prompt: {e}"),
                e.to_string(),
                started.elapsed().as_millis() as u64,
            );
        }
        if let Err(e) = client.wait_completion(&session_id, case.timeout_secs).await {
            return failed_eval_result(
                case_id,
                session_id,
                format!("wait_completion: {e}"),
                e.to_string(),
                started.elapsed().as_millis() as u64,
            );
        }
    }

    // TODO: 从 EventStore 读取 events 计算 metrics。
    // 当前 eval crate 不直接访问 EventStore（通过 HTTP 操控），
    // 暂用空 metrics。后续可通过 server API 暴露 events 端点。
    let metrics = Metrics::default();

    // Run judges
    let ctx = JudgeContext {
        work_dir: &work_dir,
        events: &[],
        metrics: &metrics,
    };
    let verdicts = judge::evaluate_judges(&case.judges, &ctx).await;
    let passed = verdicts.iter().all(|v| v.is_pass());
    let swe_bench_prediction = read_swe_bench_prediction(&work_dir);

    // Cleanup
    if !keep_workdir {
        let _ = std::fs::remove_dir_all(&work_dir);
    } else {
        tracing::info!(case_id = %case_id, path = %work_dir.display(), "keeping workdir");
    }

    EvalResult {
        case_id,
        session_id,
        passed,
        verdicts,
        metrics,
        duration_ms: started.elapsed().as_millis() as u64,
        swe_bench_prediction,
        error: None,
    }
}

fn failed_eval_result(
    case_id: String,
    session_id: String,
    reason: String,
    error: String,
    duration_ms: u64,
) -> EvalResult {
    EvalResult {
        case_id,
        session_id,
        passed: false,
        verdicts: vec![Verdict::Fail { reason }],
        metrics: Metrics::default(),
        duration_ms,
        swe_bench_prediction: None,
        error: Some(error),
    }
}

fn read_swe_bench_prediction(work_dir: &std::path::Path) -> Option<SweBenchPrediction> {
    let path = work_dir.join(SWE_BENCH_PREDICTION_FILE);
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// 从 ~/.astrcode/run.json 读取 server 连接信息。
fn read_run_info() -> Result<RunInfo, EvalError> {
    let path = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(".astrcode")
        .join("run.json");
    let content = std::fs::read_to_string(&path)
        .map_err(|e| EvalError::Server(format!("cannot read {}: {e}", path.display())))?;
    serde_json::from_str(&content).map_err(|e| EvalError::Server(format!("invalid run.json: {e}")))
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunInfo {
    port: u16,
    auth_token: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::Setup;

    #[tokio::test]
    async fn checkpoint_appends_jsonl_results_and_refuses_to_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("checkpoint.json");
        let result = failed_eval_result(
            "case-1".to_string(),
            "session-1".to_string(),
            "no patch".to_string(),
            "model produced no changes".to_string(),
            42,
        );

        initialize_checkpoint(path.clone()).await.unwrap();
        append_checkpoint(path.clone(), result).await.unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = content.lines().collect();
        assert_eq!(lines.len(), 1);
        let result: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(result["case_id"], "case-1");
        assert_eq!(result["error"], "model produced no changes");

        let resumed = load_checkpoint(path.clone()).await.unwrap();
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].case_id, "case-1");
        assert_eq!(
            resumed[0].error.as_deref(),
            Some("model produced no changes")
        );

        let error = initialize_checkpoint(path).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    }

    #[tokio::test]
    async fn resumed_results_skip_completed_cases_and_reject_mismatched_checkpoints() {
        let case = EvalCase {
            id: "case-1".to_string(),
            description: String::new(),
            setup: Setup::Empty,
            prompts: Vec::new(),
            judges: Vec::new(),
            timeout_secs: 0,
            tags: Vec::new(),
        };
        let result = failed_eval_result(
            case.id.clone(),
            "session-1".to_string(),
            "preserved failure".to_string(),
            "preserved error".to_string(),
            42,
        );
        let runner = EvalRunner {
            config: EvalConfig::default(),
            server: None,
            resumed_results: vec![result.clone()],
        };

        let report = runner.run_all(vec![case.clone()]).await.unwrap();
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].session_id, "session-1");

        let duplicate_runner = EvalRunner {
            config: EvalConfig::default(),
            server: None,
            resumed_results: vec![result.clone(), result],
        };
        let duplicate_error = duplicate_runner
            .run_all(vec![case.clone()])
            .await
            .unwrap_err();
        assert!(duplicate_error.to_string().contains("duplicate case"));

        let unknown_runner = EvalRunner {
            config: EvalConfig::default(),
            server: None,
            resumed_results: vec![failed_eval_result(
                "case-2".to_string(),
                String::new(),
                String::new(),
                String::new(),
                0,
            )],
        };
        let unknown_error = unknown_runner.run_all(vec![case]).await.unwrap_err();
        assert!(unknown_error.to_string().contains("not present"));
    }

    #[test]
    fn aggregate_patch_budget_replaces_overflow_with_empty_prediction() {
        let mut retained = MAX_RETAINED_PATCH_BYTES - 2;
        let mut result = failed_eval_result(
            "case-1".to_string(),
            String::new(),
            "placeholder".to_string(),
            "placeholder".to_string(),
            0,
        );
        result.passed = true;
        result.swe_bench_prediction = Some(SweBenchPrediction {
            instance_id: "case-1".to_string(),
            model_name_or_path: "model".to_string(),
            model_patch: "abc".to_string(),
        });

        enforce_patch_budget(&mut result, &mut retained);

        assert!(!result.passed);
        assert_eq!(retained, MAX_RETAINED_PATCH_BYTES - 2);
        assert_eq!(
            result.swe_bench_prediction.unwrap().model_patch,
            String::new()
        );
        assert!(result.error.unwrap().contains("audit limit"));
    }
}
