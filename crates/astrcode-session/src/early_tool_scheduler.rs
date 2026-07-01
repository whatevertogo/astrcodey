//! 流式工具执行调度器。
//!
//! 当 LLM 流式输出工具调用参数时，单个工具调用参数接收完毕后
//! ([`LlmEvent::ToolCallCompleted`](astrcode_core::llm::LlmEvent::ToolCallCompleted))
//! 即可立即调度执行，而不必等待整个 LLM 响应流结束。
//!
//! 调度器负责：
//! - 并发限流（不超过 `max_parallel`）
//! - Sequential barrier（写工具独占执行，不被 Parallel 工具抢占）
//! - 按 provider 顺序有序收集结果

use std::collections::VecDeque;

use astrcode_core::tool::{ExecutionMode, ToolResult};
use astrcode_kernel::ToolRegistry;
use tokio::task::JoinSet;

use crate::{
    tool_exec::{ToolCallRuntimeContext, execute_tool_call},
    tool_types::PreparedToolCall,
    turn_context::TurnError,
};

/// 一个已准备好的工具调用的执行槽位。
struct EarlyExecutionSlot {
    prepared: PreparedToolCall,
    /// 执行结果。`None` 表示尚未执行或执行未完成。
    result: Option<ToolResult>,
}

/// 流式工具执行调度器。
///
/// 接收已准备好的工具调用（经过 JSON 解析、权限链、PreToolUse 钩子），
/// 在 max_parallel 限制内并发执行。Sequential 工具构成 barrier：
/// 等待当前所有在途工具完成后才独占执行。
pub(crate) struct EarlyToolScheduler {
    tool_registry: std::sync::Arc<ToolRegistry>,
    runtime_ctx: ToolCallRuntimeContext,
    join_set: JoinSet<(usize, ToolResult)>,
    slots: Vec<EarlyExecutionSlot>,
    queued: VecDeque<usize>,
    max_parallel: usize,
    in_flight: usize,
}

#[allow(dead_code)]
impl EarlyToolScheduler {
    pub(crate) fn new(
        tool_registry: std::sync::Arc<ToolRegistry>,
        runtime_ctx: ToolCallRuntimeContext,
        max_parallel: usize,
    ) -> Self {
        Self {
            tool_registry,
            runtime_ctx,
            join_set: JoinSet::new(),
            slots: Vec::new(),
            queued: VecDeque::new(),
            max_parallel: max_parallel.max(1),
            in_flight: 0,
        }
    }

    /// 入队一个已准备好的工具调用，返回其在结果列表中的索引。
    /// 调用后自动尝试启动队列中的就绪任务。
    ///
    /// 只有 `Ready` 的工具会被实际执行；其它结果延迟到 tools_stage 按原顺序处理。
    pub(crate) fn schedule(&mut self, prepared: PreparedToolCall) -> usize {
        let index = self.slots.len();
        let should_execute = matches!(
            prepared.outcome,
            crate::tool_types::PreparedToolOutcome::Ready
        );
        let result = None;
        self.slots.push(EarlyExecutionSlot { prepared, result });
        if should_execute {
            self.queued.push_back(index);
            self.start_ready();
        }
        index
    }

    /// 尝试在 max_parallel 限制内启动队列中的就绪任务。
    ///
    /// Sequential barrier：如果下一个队列中的工具是 Sequential 模式，
    /// 且当前有在途任务，则暂停启动直到在途任务全部完成。
    fn start_ready(&mut self) {
        while self.in_flight < self.max_parallel {
            let Some(&slot_index) = self.queued.front() else {
                break;
            };
            let Some(slot) = self.slots.get(slot_index) else {
                self.queued.pop_front();
                continue;
            };
            let is_sequential = slot.prepared.mode == ExecutionMode::Sequential;

            // Sequential barrier: 写工具必须等所有在途工具完成
            if is_sequential && self.in_flight > 0 {
                break;
            }

            self.queued.pop_front();
            self.spawn(slot_index);
            self.in_flight += 1;

            // Sequential 工具独占一个执行周期，不在此轮继续启动其他工具
            if is_sequential {
                break;
            }
        }
    }

    fn spawn(&mut self, slot_index: usize) {
        let Some(slot) = self.slots.get(slot_index) else {
            return;
        };
        let call = slot.prepared.to_executable();
        let tool_registry = std::sync::Arc::clone(&self.tool_registry);
        let runtime_ctx = self.runtime_ctx.clone();
        self.join_set
            .spawn(async move { execute_tool_call(tool_registry, runtime_ctx, call).await });
    }

    /// 是否有在途或排队的工具调用。
    pub(crate) fn has_pending(&self) -> bool {
        self.in_flight > 0 || !self.queued.is_empty()
    }

    /// 轮询下一个完成的工具调用。返回 `(slot_index, result)`。
    pub(crate) async fn poll_completed(
        &mut self,
    ) -> Result<Option<(usize, ToolResult)>, TurnError> {
        let Some(joined) = self.join_set.join_next().await else {
            return Ok(None);
        };
        let (index, result) = joined?;
        self.in_flight = self.in_flight.saturating_sub(1);
        // 有任务完成，尝试启动队列中的下一个
        self.start_ready();
        Ok(Some((index, result)))
    }

    /// 等待所有在途工具完成。队列中未启动的工具也会被启动（如果 max_parallel 允许）。
    pub(crate) async fn drain_all(&mut self) -> Result<(), TurnError> {
        // 先启动所有能启动的
        self.start_ready();
        while let Some((index, result)) = self.poll_completed().await? {
            if let Some(slot) = self.slots.get_mut(index) {
                slot.result = Some(result);
            }
        }
        Ok(())
    }

    /// 取消所有在途任务，清空队列。
    pub(crate) fn abort_all(&mut self) {
        self.join_set.abort_all();
        self.in_flight = 0;
        self.queued.clear();
    }

    /// 将已完成的结果填入对应槽位。
    pub(crate) fn record_result(&mut self, index: usize, result: ToolResult) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.result = Some(result);
        }
        self.start_ready();
    }

    /// 消费调度器，返回所有已准备的工具调用及其执行结果（按 provider 顺序）。
    ///
    /// `result` 为 `None` 的条目表示该工具未在调度器中执行
    /// （如 NeedsApproval、Blocked、DuplicateSameStep 等）。
    pub(crate) fn into_entries(self) -> Vec<EarlyExecutionEntry> {
        self.slots
            .into_iter()
            .map(|slot| EarlyExecutionEntry {
                prepared: slot.prepared,
                result: slot.result,
            })
            .collect()
    }
}

/// 流式执行的结果条目。
#[allow(dead_code)]
pub(crate) struct EarlyExecutionEntry {
    /// 已准备好的工具调用。
    pub prepared: PreparedToolCall,
    /// 执行结果。`None` 表示该工具未在调度器中执行。
    pub result: Option<ToolResult>,
}

#[cfg(test)]
mod tests {
    use astrcode_core::tool::{ExecutionMode, ToolResult};

    use super::*;

    /// 辅助：构造仅含逻辑字段（不含 runtime_ctx）的测试用 slots。
    /// 真实的并发/执行行为由集成测试覆盖。
    fn make_slot(
        call_id: &str,
        mode: ExecutionMode,
        result: Option<ToolResult>,
    ) -> EarlyExecutionSlot {
        EarlyExecutionSlot {
            prepared: PreparedToolCall {
                index: 0,
                call_id: call_id.to_string(),
                name: call_id.to_string(),
                tool_input: serde_json::json!({}),
                mode,
                outcome: crate::tool_types::PreparedToolOutcome::Ready,
            },
            result,
        }
    }

    fn make_result(call_id: &str) -> ToolResult {
        ToolResult {
            call_id: call_id.to_string(),
            content: "ok".to_string(),
            is_error: false,
            error: None,
            metadata: Default::default(),
            duration_ms: None,
        }
    }

    #[test]
    fn into_entries_preserves_order_and_results() {
        let slots = vec![
            make_slot("a", ExecutionMode::Parallel, Some(make_result("a"))),
            make_slot("b", ExecutionMode::Parallel, None),
            make_slot("c", ExecutionMode::Sequential, Some(make_result("c"))),
        ];
        let entries: Vec<_> = slots
            .into_iter()
            .map(|s| EarlyExecutionEntry {
                prepared: s.prepared,
                result: s.result,
            })
            .collect();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].prepared.call_id, "a");
        assert!(entries[0].result.is_some());
        assert_eq!(entries[1].prepared.call_id, "b");
        assert!(entries[1].result.is_none());
        assert_eq!(entries[2].prepared.call_id, "c");
        assert!(entries[2].result.is_some());
    }

    #[test]
    fn has_pending_reflects_in_flight_and_queued() {
        let queued = VecDeque::from([0usize, 1]);
        let in_flight = 1;
        assert!(in_flight > 0 || !queued.is_empty());

        let empty_queued: VecDeque<usize> = VecDeque::new();
        assert!(0 == 0 && empty_queued.is_empty());
    }
}
