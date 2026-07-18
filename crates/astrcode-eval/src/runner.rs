//! EvalRunner — 编排器，管理 server 生命周期 + 并发执行 case。

use std::{sync::Arc, time::Instant};

use tokio::sync::Semaphore;

use crate::{
    EvalConfig, EvalError,
    case::EvalCase,
    client::EvalClient,
    judge::{self, JudgeContext, Verdict},
    metrics::Metrics,
    report::{EvalReport, EvalResult, SweBenchPrediction},
    setup,
};

/// 评测编排器。
pub struct EvalRunner {
    config: EvalConfig,
    server_addr: String,
    auth_token: String,
}

impl EvalRunner {
    /// 启动 eval runner。
    ///
    /// 如果 config 指定了 server_addr，直接使用；否则需要外部确保 server 已启动。
    pub async fn start(config: &EvalConfig) -> Result<Self, EvalError> {
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
        let client = reqwest::Client::new();
        let health_url = format!("{}/api/config", server_addr);
        match client
            .get(&health_url)
            .bearer_auth(&auth_token)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {},
            Ok(resp) => {
                return Err(EvalError::Server(format!(
                    "server health check failed: {}",
                    resp.status()
                )));
            },
            Err(e) => {
                return Err(EvalError::Server(format!(
                    "cannot connect to server at {server_addr}: {e}"
                )));
            },
        }

        Ok(Self {
            config: config.clone(),
            server_addr,
            auth_token,
        })
    }

    /// 并发执行所有 case，返回报告。
    pub async fn run_all(&self, cases: Vec<EvalCase>) -> EvalReport {
        let semaphore = Arc::new(Semaphore::new(self.config.concurrency));
        let mut handles = Vec::with_capacity(cases.len());
        let mut case_ids = Vec::with_capacity(cases.len());

        for case in cases {
            let permit = Arc::clone(&semaphore);
            let server_addr = self.server_addr.clone();
            let auth_token = self.auth_token.clone();
            let cases_dir = self.config.cases_dir.clone();
            let keep_workdir = self.config.keep_workdir;
            let case_id = case.id.clone();

            handles.push(tokio::spawn(async move {
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
                run_single_case(&case, &server_addr, &auth_token, &cases_dir, keep_workdir).await
            }));
            case_ids.push(case_id);
        }

        let mut results = Vec::with_capacity(handles.len());
        for (handle, case_id) in handles.into_iter().zip(case_ids) {
            match handle.await {
                Ok(result) => results.push(result),
                Err(e) => {
                    results.push(failed_eval_result(
                        case_id,
                        String::new(),
                        format!("task panicked: {e}"),
                        format!("panic: {e}"),
                        0,
                    ));
                },
            }
        }

        EvalReport::from_results(results)
    }
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

    let client = EvalClient::new(server_addr, auth_token);

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
    let path = work_dir.join(".astrcode-swebench-prediction.json");
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
