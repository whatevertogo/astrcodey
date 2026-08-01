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

use astrcode_core::tool::ExecutionMode;
use tokio::task::JoinSet;

use crate::{
    ToolRegistry,
    tool_exec::{ToolCallRuntimeContext, execute_tool_call},
    tool_types::{PreparedToolDisposition, PreparedToolInvocation, ToolExecutionOutcome},
    turn_context::TurnError,
};

/// 一个已准备好的工具调用的执行槽位。
struct EarlyExecutionSlot {
    prepared: PreparedToolInvocation,
    /// 执行结果。`None` 表示尚未执行或执行未完成。
    outcome: Option<ToolExecutionOutcome>,
}

/// 流式工具执行调度器。
///
/// 接收已准备好的工具调用（经过 JSON 解析、权限链、PreToolUse 钩子），
/// 在 max_parallel 限制内并发执行。Sequential 工具构成 barrier：
/// 等待当前所有在途工具完成后才独占执行。
pub(crate) struct EarlyToolScheduler {
    tool_registry: std::sync::Arc<ToolRegistry>,
    runtime_ctx: ToolCallRuntimeContext,
    join_set: JoinSet<(usize, ToolExecutionOutcome)>,
    slots: Vec<EarlyExecutionSlot>,
    queued: VecDeque<usize>,
    max_parallel: usize,
    in_flight: usize,
}

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
    pub(crate) fn schedule(&mut self, prepared: PreparedToolInvocation) -> usize {
        let index = self.slots.len();
        let should_execute = matches!(prepared.disposition, PreparedToolDisposition::Execute);
        self.slots.push(EarlyExecutionSlot {
            prepared,
            outcome: None,
        });
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

            // Sequential 工具独占一个执行周期，不在此轮继续启动其他工具
            if is_sequential {
                break;
            }
        }
    }

    fn spawn(&mut self, slot_index: usize) {
        // 不变式:queued 中的 index 只来自 schedule() 入队时的 `self.slots.len()`,
        // 且槽位从不删除,index 必合法。取不到槽位属编程错误,panic 比静默返回
        // 安全——静默返回会漏掉下方 in_flight += 1,使 poll_completed 提前返回
        // None,剩余队列悬挂。
        let slot = &self.slots[slot_index];
        let call = slot.prepared.to_executable();
        let tool_registry = std::sync::Arc::clone(&self.tool_registry);
        let runtime_ctx = self.runtime_ctx.clone();
        self.join_set
            .spawn(async move { execute_tool_call(tool_registry, runtime_ctx, call).await });
        // in_flight 与 join_set 中的任务数一一对应:此处 +1(唯一入口),
        // poll_completed 减 1,abort_all 归零。
        self.in_flight += 1;
    }

    /// 是否有在途或排队的工具调用。
    pub(crate) fn has_pending(&self) -> bool {
        self.in_flight > 0 || !self.queued.is_empty()
    }

    /// 轮询下一个完成的工具调用。返回 `(slot_index, result)`。
    pub(crate) async fn poll_completed(
        &mut self,
    ) -> Result<Option<(usize, ToolExecutionOutcome)>, TurnError> {
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
        while let Some((index, outcome)) = self.poll_completed().await? {
            if let Some(slot) = self.slots.get_mut(index) {
                slot.outcome = Some(outcome);
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
    pub(crate) fn record_outcome(&mut self, index: usize, outcome: ToolExecutionOutcome) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.outcome = Some(outcome);
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
                outcome: slot.outcome,
            })
            .collect()
    }
}

/// 流式执行的结果条目。
pub(crate) struct EarlyExecutionEntry {
    /// 已准备好的工具调用。
    pub prepared: PreparedToolInvocation,
    /// 执行结果。`None` 表示该工具未在调度器中执行。
    pub outcome: Option<ToolExecutionOutcome>,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use astrcode_core::{
        permission::ApprovalMode,
        tool::{ExecutionMode, LlmModelIds, SessionToolSelection, ToolResult},
        types::new_session_id,
    };
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        permission::{ApprovalHistoryStore, PermissionChain},
        tool_exec::{ToolCallRuntimeContext, ToolRuntimeCapabilities, TurnToolContext},
        turn_context::SharedTurnContext,
    };

    /// 辅助：构造仅含逻辑字段（不含 runtime_ctx）的测试用 slots。
    /// 真实的并发/执行行为由集成测试覆盖。
    fn make_slot(
        call_id: &str,
        mode: ExecutionMode,
        result: Option<ToolResult>,
    ) -> EarlyExecutionSlot {
        EarlyExecutionSlot {
            prepared: PreparedToolInvocation {
                index: 0,
                call_id: call_id.to_string(),
                name: call_id.to_string(),
                tool_input: serde_json::json!({}),
                raw_arguments: None,
                mode,
                discovery_gate: None,
                disposition: PreparedToolDisposition::Execute,
            },
            outcome: result
                .map(crate::tool_types::ToolResultCommit::completed)
                .map(ToolExecutionOutcome::Completed),
        }
    }

    /// 构造真实 scheduler 骨架：slots 直接填充（不经过 schedule/spawn）。
    fn make_scheduler(slots: Vec<EarlyExecutionSlot>) -> EarlyToolScheduler {
        EarlyToolScheduler {
            tool_registry: Arc::new(ToolRegistry::new()),
            runtime_ctx: make_runtime_ctx(),
            join_set: JoinSet::new(),
            slots,
            queued: VecDeque::new(),
            max_parallel: 1,
            in_flight: 0,
        }
    }

    /// 最小可构造的 runtime 上下文（无 session、无工具），仅满足字段要求——
    /// into_entries 不触及它。
    fn make_runtime_ctx() -> ToolCallRuntimeContext {
        ToolCallRuntimeContext {
            turn: TurnToolContext {
                shared: SharedTurnContext {
                    session_id: new_session_id(),
                    working_dir: "/workspace".into(),
                    model_id: "model".into(),
                    session_store_dir: None,
                    turn_event_sender: None,
                    approval_mode: ApprovalMode::default(),
                    tool_selection: Some(SessionToolSelection::default()),
                    permission_chain: Arc::new(PermissionChain::new(Vec::new())),
                    approval_history: Arc::new(ApprovalHistoryStore::default()),
                },
                capabilities: ToolRuntimeCapabilities {
                    file_observation_store: None,
                    session_ops: None,
                    llm_models: LlmModelIds {
                        main: None,
                        small: None,
                    },
                    session_store_dir: None,
                },
            },
            tools: Arc::from([]),
            tool_result_reader: None,
            cancellation_token: CancellationToken::new(),
        }
    }

    fn make_result() -> ToolResult {
        ToolResult {
            content: "ok".to_string(),
            is_error: false,
            error: None,
            metadata: Default::default(),
            duration_ms: None,
        }
    }

    #[test]
    fn into_entries_preserves_order_and_results() {
        let scheduler = make_scheduler(vec![
            make_slot("a", ExecutionMode::Parallel, Some(make_result())),
            make_slot("b", ExecutionMode::Parallel, None),
            make_slot("c", ExecutionMode::Sequential, Some(make_result())),
        ]);
        let entries = scheduler.into_entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].prepared.call_id, "a");
        assert!(entries[0].outcome.is_some());
        assert_eq!(entries[1].prepared.call_id, "b");
        assert!(entries[1].outcome.is_none());
        assert_eq!(entries[2].prepared.call_id, "c");
        assert!(entries[2].outcome.is_some());
    }
}
