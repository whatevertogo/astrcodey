//! 工具调用去重：同 step 复用结果，跨 step 检测死循环并注入提醒。

use std::collections::HashMap;

use astrcode_core::tool::ToolResult;
use tokio::sync::watch;

/// 同 step 内 `check_same_step` 的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SameStepCheck {
    /// 首次出现，正常执行。
    Primary,
    /// 同 step 内重复，等待 Primary 的最终结果。
    Duplicate,
}

struct SameStepInFlight {
    primary_call_id: String,
    result_tx: watch::Sender<Option<ToolResult>>,
    /// 保持至少一个 receiver 存活，确保 `send` 能写入最新结果。
    _result_rx: watch::Receiver<Option<ToolResult>>,
}

/// 防止模型对相同 `(toolName, args)` 陷入死循环。
pub(crate) struct ToolCallDeduplicator {
    same_step_in_flight: HashMap<String, SameStepInFlight>,
    call_key_by_call_id: HashMap<String, String>,
    step_call_keys: Vec<String>,
    consecutive_key: Option<String>,
    consecutive_count: u32,
}

impl Default for ToolCallDeduplicator {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolCallDeduplicator {
    pub(crate) fn new() -> Self {
        Self {
            same_step_in_flight: HashMap::new(),
            call_key_by_call_id: HashMap::new(),
            step_call_keys: Vec::new(),
            consecutive_key: None,
            consecutive_count: 0,
        }
    }

    /// 每个 agent step 开始时调用：清空同 step 状态。
    pub(crate) fn begin_step(&mut self) {
        self.same_step_in_flight.clear();
        self.call_key_by_call_id.clear();
        self.step_call_keys.clear();
    }

    /// 在 `prepare_tool_calls` 阶段同步调用；锁定注册时的 call key。
    pub(crate) fn check_same_step(
        &mut self,
        call_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> SameStepCheck {
        let key = make_call_key(tool_name, args);
        self.call_key_by_call_id
            .insert(call_id.to_string(), key.clone());

        if let std::collections::hash_map::Entry::Vacant(entry) =
            self.same_step_in_flight.entry(key)
        {
            let (result_tx, result_rx) = watch::channel(None);
            entry.insert(SameStepInFlight {
                primary_call_id: call_id.to_string(),
                result_tx,
                _result_rx: result_rx,
            });
            SameStepCheck::Primary
        } else {
            tracing::debug!(call_id, tool_name, "same-step tool call deduplicated");
            SameStepCheck::Duplicate
        }
    }

    /// 在 `commit_tool_results` 完成后调用；释放同 step 等待者并记录跨 step 统计。
    pub(crate) fn finalize_result(&mut self, call_id: &str, result: &ToolResult) {
        let Some(key) = self.call_key_by_call_id.get(call_id).cloned() else {
            return;
        };

        if let Some(entry) = self.same_step_in_flight.get(&key) {
            if entry.primary_call_id == call_id
                && entry.result_tx.send(Some(result.clone())).is_err()
            {
                tracing::warn!(
                    call_id,
                    "tool deduplication result send failed: no receivers"
                );
            }
        }

        self.step_call_keys.push(key);
    }

    /// 同 step 重复调用等待 Primary 的最终结果（含 PostToolUse 处理后的内容）。
    pub(crate) async fn await_same_step_result(&self, duplicate_call_id: &str) -> ToolResult {
        let key = self
            .call_key_by_call_id
            .get(duplicate_call_id)
            .expect("duplicate call must have registered call key");

        let entry = self
            .same_step_in_flight
            .get(key)
            .expect("duplicate call must have in-flight entry");

        let mut rx = entry.result_tx.subscribe();
        loop {
            if let Some(result) = rx.borrow().clone() {
                let mut duplicate = result;
                duplicate.call_id = duplicate_call_id.to_string();
                return duplicate;
            }
            if rx.changed().await.is_err() {
                break;
            }
        }

        ToolResult {
            call_id: duplicate_call_id.to_string(),
            content: "Tool deduplication failed: primary call did not produce a result".into(),
            is_error: true,
            error: Some("same-step deduplication primary missing".into()),
            metadata: Default::default(),
            duration_ms: None,
        }
    }

    /// 每个含工具调用的 step 结束时调用，更新跨 step 连续重复计数。
    pub(crate) fn end_step(&mut self) {
        for key in self.step_call_keys.drain(..) {
            if self.consecutive_key.as_deref() == Some(key.as_str()) {
                self.consecutive_count += 1;
            } else {
                self.consecutive_key = Some(key);
                self.consecutive_count = 1;
            }
        }
    }

    /// 构建 LLM 请求前检查；连续 3 / 5 / 8 次重复时返回 `<system-reminder>` 文本。
    pub(crate) fn check_reminder(&self) -> Option<String> {
        let count = self.consecutive_count;
        let key = self.consecutive_key.as_ref()?;
        let (tool_name, args_json) = parse_call_key(key);

        let reminder = match count {
            3 => "<system-reminder>You appear to be repeating the same tool call across multiple \
                  steps. Consider trying a different approach.</system-reminder>"
                .to_string(),
            5 => format!(
                "<system-reminder>Warning: you have called `{tool_name}` with identical arguments \
                 {count} times in recent steps. Vary your strategy.</system-reminder>"
            ),
            8 => format!(
                "<system-reminder>Critical: `{tool_name}` has been invoked {count} consecutive \
                 times with the same arguments: {args_json}. Stop repeating this call and change \
                 your approach.</system-reminder>"
            ),
            _ => return None,
        };

        Some(reminder)
    }

    #[cfg(test)]
    pub(crate) fn consecutive_count(&self) -> u32 {
        self.consecutive_count
    }
}

fn make_call_key(tool_name: &str, args: &serde_json::Value) -> String {
    format!("{tool_name}:{}", normalize_json_value(args))
}

fn normalize_json_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            for key in keys {
                sorted.insert(key.clone(), normalize_json_value(&map[key]));
            }
            serde_json::Value::Object(sorted)
        },
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(normalize_json_value).collect())
        },
        other => other.clone(),
    }
}

fn parse_call_key(key: &str) -> (&str, &str) {
    key.split_once(':').unwrap_or((key, "{}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_args() -> serde_json::Value {
        serde_json::json!({"path": "/tmp/a.rs", "pattern": "fn main"})
    }

    #[test]
    fn stable_json_key_is_order_independent() {
        let a = serde_json::json!({"b": 2, "a": 1});
        let b = serde_json::json!({"a": 1, "b": 2});
        assert_eq!(make_call_key("Read", &a), make_call_key("Read", &b));
    }

    #[test]
    fn same_step_second_call_is_duplicate() {
        let mut dedup = ToolCallDeduplicator::new();
        dedup.begin_step();
        let args = sample_args();

        assert_eq!(
            dedup.check_same_step("call-1", "Grep", &args),
            SameStepCheck::Primary
        );
        assert_eq!(
            dedup.check_same_step("call-2", "Grep", &args),
            SameStepCheck::Duplicate
        );
    }

    #[test]
    fn cross_step_streak_increments_on_identical_calls() {
        let mut dedup = ToolCallDeduplicator::new();
        let args = sample_args();

        dedup.begin_step();
        dedup.check_same_step("c1", "Grep", &args);
        dedup.finalize_result(
            "c1",
            &ToolResult {
                call_id: "c1".into(),
                content: "ok".into(),
                is_error: false,
                error: None,
                metadata: Default::default(),
                duration_ms: None,
            },
        );
        dedup.end_step();
        assert_eq!(dedup.consecutive_count(), 1);

        dedup.begin_step();
        dedup.check_same_step("c2", "Grep", &args);
        dedup.finalize_result(
            "c2",
            &ToolResult {
                call_id: "c2".into(),
                content: "ok".into(),
                is_error: false,
                error: None,
                metadata: Default::default(),
                duration_ms: None,
            },
        );
        dedup.end_step();
        assert_eq!(dedup.consecutive_count(), 2);
    }

    #[test]
    fn cross_step_streak_resets_on_different_call() {
        let mut dedup = ToolCallDeduplicator::new();
        let args_a = sample_args();
        let args_b = serde_json::json!({"path": "/other.rs"});

        dedup.begin_step();
        dedup.check_same_step("c1", "Grep", &args_a);
        dedup.finalize_result(
            "c1",
            &ToolResult {
                call_id: "c1".into(),
                content: "ok".into(),
                is_error: false,
                error: None,
                metadata: Default::default(),
                duration_ms: None,
            },
        );
        dedup.end_step();

        dedup.begin_step();
        dedup.check_same_step("c2", "Read", &args_b);
        dedup.finalize_result(
            "c2",
            &ToolResult {
                call_id: "c2".into(),
                content: "ok".into(),
                is_error: false,
                error: None,
                metadata: Default::default(),
                duration_ms: None,
            },
        );
        dedup.end_step();

        assert_eq!(dedup.consecutive_count(), 1);
    }

    #[test]
    fn check_reminder_fires_at_thresholds_only() {
        let mut dedup = ToolCallDeduplicator::new();
        let args = sample_args();

        for step in 1..=8 {
            dedup.begin_step();
            dedup.check_same_step(&format!("c{step}"), "Grep", &args);
            dedup.finalize_result(
                &format!("c{step}"),
                &ToolResult {
                    call_id: format!("c{step}"),
                    content: "ok".into(),
                    is_error: false,
                    error: None,
                    metadata: Default::default(),
                    duration_ms: None,
                },
            );
            dedup.end_step();

            let reminder = dedup.check_reminder();
            match step {
                3 | 5 | 8 => assert!(reminder.is_some(), "step {step} should remind"),
                _ => assert!(reminder.is_none(), "step {step} should not remind"),
            }
        }
    }

    #[tokio::test]
    async fn duplicate_awaits_primary_finalized_result() {
        let mut dedup = ToolCallDeduplicator::new();
        dedup.begin_step();
        let args = sample_args();
        dedup.check_same_step("primary", "Read", &args);
        dedup.check_same_step("duplicate", "Read", &args);

        dedup.finalize_result(
            "primary",
            &ToolResult {
                call_id: "primary".into(),
                content: "file contents".into(),
                is_error: false,
                error: None,
                metadata: Default::default(),
                duration_ms: Some(12),
            },
        );

        let duplicate = dedup.await_same_step_result("duplicate").await;
        assert_eq!(duplicate.call_id, "duplicate");
        assert_eq!(duplicate.content, "file contents");
        assert_eq!(duplicate.duration_ms, Some(12));
    }
}
