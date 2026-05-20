//! Judge 判定系统 — trait 定义与内置 judges。

use std::path::Path;

use astrcode_core::event::Event;

use crate::{case::JudgeConfig, metrics::Metrics};

/// 判定上下文。
pub struct JudgeContext<'a> {
    pub work_dir: &'a Path,
    pub events: &'a [Event],
    pub metrics: &'a Metrics,
}

/// 判定结果。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Verdict {
    Pass,
    Fail { reason: String },
    Partial { score: f64, reason: String },
}

impl Verdict {
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }
}

/// 执行所有 judges，返回判定结果列表。
pub async fn evaluate_judges(configs: &[JudgeConfig], ctx: &JudgeContext<'_>) -> Vec<Verdict> {
    let mut verdicts = Vec::with_capacity(configs.len());
    for config in configs {
        let verdict = evaluate_single(config, ctx).await;
        verdicts.push(verdict);
    }
    verdicts
}

async fn evaluate_single(config: &JudgeConfig, ctx: &JudgeContext<'_>) -> Verdict {
    match config {
        JudgeConfig::Command {
            command,
            expect_exit_code,
        } => evaluate_command(command, expect_exit_code.unwrap_or(0), ctx.work_dir).await,
        JudgeConfig::FileContains {
            path,
            contains,
            not_contains,
        } => evaluate_file_contains(
            ctx.work_dir,
            path,
            contains.as_deref(),
            not_contains.as_deref(),
        ),
        JudgeConfig::FileExists { path, exists } => {
            evaluate_file_exists(ctx.work_dir, path, *exists)
        },
        JudgeConfig::EventLog { condition } => evaluate_event_log(condition, ctx.metrics),
    }
}

// ─── CommandJudge ────────────────────────────────────────────────────────

async fn evaluate_command(command: &str, expect_exit_code: i32, work_dir: &Path) -> Verdict {
    let result = tokio::process::Command::new("sh")
        .args(["-c", command])
        .current_dir(work_dir)
        .output()
        .await;
    match result {
        Ok(output) => {
            let code = output.status.code().unwrap_or(-1);
            if code == expect_exit_code {
                Verdict::Pass
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Verdict::Fail {
                    reason: format!(
                        "command exited with {code} (expected {expect_exit_code}): {stderr}"
                    ),
                }
            }
        },
        Err(e) => Verdict::Fail {
            reason: format!("command execution failed: {e}"),
        },
    }
}

// ─── FileContainsJudge ───────────────────────────────────────────────────

fn evaluate_file_contains(
    work_dir: &Path,
    file_path: &str,
    contains: Option<&str>,
    not_contains: Option<&str>,
) -> Verdict {
    let full_path = work_dir.join(file_path);
    let content = match std::fs::read_to_string(&full_path) {
        Ok(c) => c,
        Err(e) => {
            return Verdict::Fail {
                reason: format!("cannot read {file_path}: {e}"),
            };
        },
    };

    if let Some(expected) = contains {
        if !content.contains(expected) {
            return Verdict::Fail {
                reason: format!("{file_path} does not contain '{expected}'"),
            };
        }
    }
    if let Some(unexpected) = not_contains {
        if content.contains(unexpected) {
            return Verdict::Fail {
                reason: format!("{file_path} contains forbidden text '{unexpected}'"),
            };
        }
    }
    Verdict::Pass
}

// ─── FileExistsJudge ─────────────────────────────────────────────────────

fn evaluate_file_exists(work_dir: &Path, file_path: &str, should_exist: bool) -> Verdict {
    let full_path = work_dir.join(file_path);
    let exists = full_path.exists();
    if exists == should_exist {
        Verdict::Pass
    } else if should_exist {
        Verdict::Fail {
            reason: format!("{file_path} does not exist"),
        }
    } else {
        Verdict::Fail {
            reason: format!("{file_path} exists but should not"),
        }
    }
}

// ─── EventLogJudge ───────────────────────────────────────────────────────

/// 简单条件表达式：
/// - "errors < N" / "errors <= N" / "errors == 0"
/// - "no_tool <name>"
/// - "tool_count <name> <= N"
fn evaluate_event_log(condition: &str, metrics: &Metrics) -> Verdict {
    // 当前 eval 通过 HTTP 操控 server，不直接访问 EventStore，
    // 因此 metrics 可能为空。如果 metrics 未填充（全为默认值），
    // 则跳过判定而非给出误导性结果。
    if metrics.total_turns == 0 && metrics.errors == 0 && metrics.tool_calls.is_empty() {
        return Verdict::Partial {
            score: 0.0,
            reason: format!(
                "event_log judge skipped: metrics not populated (condition: {condition})"
            ),
        };
    }

    let parts: Vec<&str> = condition.split_whitespace().collect();

    match parts.as_slice() {
        ["errors", op, n] => {
            let n: usize = match n.parse() {
                Ok(v) => v,
                Err(_) => {
                    return Verdict::Fail {
                        reason: format!("invalid condition: {condition}"),
                    };
                },
            };
            let pass = match *op {
                "<" => metrics.errors < n,
                "<=" => metrics.errors <= n,
                "==" => metrics.errors == n,
                ">" => metrics.errors > n,
                ">=" => metrics.errors >= n,
                _ => {
                    return Verdict::Fail {
                        reason: format!("unknown operator: {op}"),
                    };
                },
            };
            if pass {
                Verdict::Pass
            } else {
                Verdict::Fail {
                    reason: format!(
                        "errors condition failed: {} errors, expected {op} {n}",
                        metrics.errors
                    ),
                }
            }
        },
        ["no_tool", name] => {
            if metrics.tool_calls.contains_key(*name) {
                Verdict::Fail {
                    reason: format!("tool '{name}' was called but should not have been"),
                }
            } else {
                Verdict::Pass
            }
        },
        ["tool_count", name, op, n] => {
            let n: usize = match n.parse() {
                Ok(v) => v,
                Err(_) => {
                    return Verdict::Fail {
                        reason: format!("invalid condition: {condition}"),
                    };
                },
            };
            let count = metrics.tool_calls.get(*name).copied().unwrap_or(0);
            let pass = match *op {
                "<" => count < n,
                "<=" => count <= n,
                "==" => count == n,
                ">" => count > n,
                ">=" => count >= n,
                _ => {
                    return Verdict::Fail {
                        reason: format!("unknown operator: {op}"),
                    };
                },
            };
            if pass {
                Verdict::Pass
            } else {
                Verdict::Fail {
                    reason: format!(
                        "tool_count condition failed: {name} called {count} times, expected {op} \
                         {n}"
                    ),
                }
            }
        },
        _ => Verdict::Fail {
            reason: format!("unrecognized event_log condition: {condition}"),
        },
    }
}
