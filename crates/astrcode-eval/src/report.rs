//! 报告生成 — JSON / Markdown 输出。

use serde::Serialize;

use crate::{judge::Verdict, metrics::Metrics};

/// 单个 case 的评测结果。
#[derive(Debug, Clone, Serialize)]
pub struct EvalResult {
    pub case_id: String,
    pub session_id: String,
    pub passed: bool,
    pub verdicts: Vec<Verdict>,
    pub metrics: Metrics,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swe_bench_prediction: Option<SweBenchPrediction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// SWE-bench 官方 harness 可消费的单条 prediction。
#[derive(Debug, Clone, serde::Deserialize, Serialize)]
pub struct SweBenchPrediction {
    pub instance_id: String,
    pub model_name_or_path: String,
    pub model_patch: String,
}

/// 评测报告（所有 case 的汇总）。
#[derive(Debug, Clone, Serialize)]
pub struct EvalReport {
    pub results: Vec<EvalResult>,
    pub summary: EvalSummary,
}

/// 汇总统计。
#[derive(Debug, Clone, Serialize)]
pub struct EvalSummary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub pass_rate: f64,
    pub total_duration_ms: u64,
}

impl EvalReport {
    pub fn from_results(results: Vec<EvalResult>) -> Self {
        let total = results.len();
        let passed = results.iter().filter(|r| r.passed).count();
        let failed = total - passed;
        let pass_rate = if total > 0 {
            passed as f64 / total as f64
        } else {
            0.0
        };
        let total_duration_ms = results.iter().map(|r| r.duration_ms).sum();
        Self {
            results,
            summary: EvalSummary {
                total,
                passed,
                failed,
                pass_rate,
                total_duration_ms,
            },
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    pub fn swe_bench_predictions_jsonl(&self) -> Result<String, serde_json::Error> {
        let mut jsonl = String::new();
        for prediction in self
            .results
            .iter()
            .filter_map(|result| result.swe_bench_prediction.as_ref())
        {
            jsonl.push_str(&serde_json::to_string(prediction)?);
            jsonl.push('\n');
        }
        Ok(jsonl)
    }

    pub fn swe_bench_prediction_count(&self) -> usize {
        self.results
            .iter()
            .filter(|result| result.swe_bench_prediction.is_some())
            .count()
    }

    pub fn to_markdown(&self) -> String {
        let mut md = String::new();
        md.push_str("# Eval Report\n\n");
        md.push_str(&format!(
            "**Summary:** {}/{} passed ({:.0}%) in {:.1}s\n\n",
            self.summary.passed,
            self.summary.total,
            self.summary.pass_rate * 100.0,
            self.summary.total_duration_ms as f64 / 1000.0,
        ));
        let prediction_count = self.swe_bench_prediction_count();
        if prediction_count > 0 {
            md.push_str(&format!(
                "**SWE-bench predictions:** {prediction_count} generated; use JSON output to \
                 collect `swe_bench_prediction` entries for the official harness.\n\n"
            ));
        }
        md.push_str("| Case | Result | Duration | Tools | Errors |\n");
        md.push_str("|------|--------|----------|-------|--------|\n");
        for r in &self.results {
            let result_str = if r.passed { "✅ Pass" } else { "❌ Fail" };
            let tools: usize = r.metrics.tool_calls.values().sum();
            md.push_str(&format!(
                "| {} | {} | {:.1}s | {} | {} |\n",
                r.case_id,
                result_str,
                r.duration_ms as f64 / 1000.0,
                tools,
                r.metrics.errors,
            ));
        }
        // Failed case details
        let failures: Vec<_> = self.results.iter().filter(|r| !r.passed).collect();
        if !failures.is_empty() {
            md.push_str("\n## Failures\n\n");
            for r in failures {
                md.push_str(&format!("### {}\n\n", r.case_id));
                if let Some(ref err) = r.error {
                    md.push_str(&format!("Error: {err}\n\n"));
                }
                for (i, v) in r.verdicts.iter().enumerate() {
                    if let Verdict::Fail { reason } = v {
                        md.push_str(&format!("- Judge {}: {reason}\n", i + 1));
                    }
                }
                md.push('\n');
            }
        }
        md
    }

    /// 全部通过返回 true。
    pub fn all_passed(&self) -> bool {
        self.summary.failed == 0
    }
}
