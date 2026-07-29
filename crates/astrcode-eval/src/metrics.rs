//! 从 event log 提取行为指标。

use std::collections::BTreeMap;

use astrcode_core::event::{DurableEventPayload, StoredEvent};
use serde::{Deserialize, Serialize};

/// 从 session event log 自动提取的统计指标。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Metrics {
    /// 总 turn 数。
    pub total_turns: usize,
    /// 工具调用次数（按工具名分类）。
    pub tool_calls: BTreeMap<String, usize>,
    /// 错误发生次数。
    pub errors: usize,
    /// 是否触发了 context compaction。
    pub compactions: usize,
    /// 总耗时（ms，从首个 TurnStarted 到最后一个 TurnCompleted）。
    pub total_duration_ms: u64,
    /// 最终完成原因。
    pub finish_reason: String,
}

impl Metrics {
    /// 纯函数：从事件列表提取指标。
    pub fn from_events(events: &[StoredEvent]) -> Self {
        let mut metrics = Self::default();
        let mut first_turn_ts = None;
        let mut last_turn_ts = None;

        for event in events {
            match &event.payload {
                DurableEventPayload::TurnStarted => {
                    metrics.total_turns += 1;
                    if first_turn_ts.is_none() {
                        first_turn_ts = Some(event.timestamp);
                    }
                },
                DurableEventPayload::TurnCompleted { finish_reason } => {
                    last_turn_ts = Some(event.timestamp);
                    metrics.finish_reason = finish_reason.clone();
                },
                DurableEventPayload::ToolCallCompleted { tool_name, .. } => {
                    *metrics.tool_calls.entry(tool_name.clone()).or_default() += 1;
                },
                DurableEventPayload::ToolCallFailed { tool_name, .. } => {
                    *metrics.tool_calls.entry(tool_name.clone()).or_default() += 1;
                    metrics.errors += 1;
                },
                DurableEventPayload::ToolCallCancelled { tool_name, .. } => {
                    *metrics.tool_calls.entry(tool_name.clone()).or_default() += 1;
                },
                DurableEventPayload::ErrorOccurred { .. } => {
                    metrics.errors += 1;
                },
                DurableEventPayload::TranscriptRewritten { .. } => {
                    metrics.compactions += 1;
                },
                _ => {},
            }
        }

        if let (Some(first), Some(last)) = (first_turn_ts, last_turn_ts) {
            metrics.total_duration_ms = (last - first).num_milliseconds().max(0) as u64;
        }

        metrics
    }
}
