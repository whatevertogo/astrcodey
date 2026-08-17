use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use astrcode_core::{
    event::DurableEventPayload,
    permission::ApprovalDecision,
    tool::{ExecutionMode, ToolDefinition},
    types::ToolCallId,
};
use tokio::{sync::oneshot, task::JoinSet};

use super::{ToolCalls, events::finish_tool_call};
use crate::{
    permission::APPROVAL_TIMEOUT_SECS,
    tool_exec::execute_tool_call,
    tool_types::{
        ExecutableToolInvocation, ExecuteToolBatch, PreparedToolApproval, PreparedToolDisposition,
        ToolExecutionOutcome,
    },
    turn_context::TurnError,
};

impl ToolCalls {
    /// 执行已预处理的工具调用。
    ///
    /// 只读工具按连续批次并发执行；写入、shell 以及审批/阻止结果都会先刷新当前
    /// 只读批次，再按原始顺序串行处理。
    pub(crate) async fn execute_and_commit(
        &self,
        mut input: ExecuteToolBatch<'_>,
    ) -> Result<Vec<String>, TurnError> {
        // 尚未 commit 的 declared 调用：正常路径逐个移除；turn 出错时兜底补发失败结果。
        let mut uncommitted_calls = input
            .batch
            .calls
            .iter()
            .map(|call| call.call_id.clone())
            .collect::<HashSet<_>>();
        let result = self
            .execute_declared_batch(&mut input, &mut uncommitted_calls)
            .await;
        if let Err(error) = &result {
            self.complete_pending_declared_as_failed(&mut input, &mut uncommitted_calls, error)
                .await;
        }
        result
    }

    async fn execute_declared_batch(
        &self,
        input: &mut ExecuteToolBatch<'_>,
        uncommitted_calls: &mut HashSet<String>,
    ) -> Result<Vec<String>, TurnError> {
        let mut discovered_tools = Vec::new();
        let tools = Arc::from(input.tools);
        let mut parallel_batch = Vec::new();
        let mut parallel_batch_start = None;

        for position in 0..input.batch.calls.len() {
            if self.cancellation_token.is_cancelled() {
                return Err(TurnError::Aborted);
            }
            let call = input.batch.calls[position].clone();

            // 并行调用加入当前并行批次；其余调用必须先 flush：
            // 并行批次在 flush 时才执行并提交，串行/审批/去重复用调用必须等它完成，
            // 否则 durable 事件乱序；去重复用还依赖 flush 中 finalize 主调用结果，
            // 顺序颠倒会死锁。
            let joins_parallel = matches!(
                &call.disposition,
                PreparedToolDisposition::Execute if call.mode == ExecutionMode::Parallel
            );
            if !joins_parallel {
                discovered_tools.extend(
                    self.flush_and_commit_parallel_batch(
                        &mut parallel_batch,
                        &mut parallel_batch_start,
                        input,
                        uncommitted_calls,
                        Arc::clone(&tools),
                    )
                    .await?,
                );
            }

            // 每个分支只产出本调用的 outcome；None 表示已加入并行批次，稍后统一提交。
            let outcome = match &call.disposition {
                PreparedToolDisposition::Rejected { error } => {
                    Some(ToolExecutionOutcome::failed(error.clone()))
                },
                PreparedToolDisposition::ReuseSameStep => Some(
                    input
                        .state
                        .tool_deduplicator()
                        .await_same_step_outcome(&call.call_id)
                        .await,
                ),
                PreparedToolDisposition::AwaitApprovals(approvals) => Some(
                    self.request_approvals_and_resolve(
                        input,
                        position,
                        approvals,
                        Arc::clone(&tools),
                    )
                    .await?,
                ),
                PreparedToolDisposition::Execute if call.mode == ExecutionMode::Parallel => {
                    if parallel_batch_start.is_none() {
                        parallel_batch_start = Some(position);
                    }
                    parallel_batch.push(call.to_executable());
                    None
                },
                PreparedToolDisposition::Execute => {
                    let outcome = self
                        .execute_single_tool(call.to_executable(), Arc::clone(&tools))
                        .await;
                    Some(outcome)
                },
            };
            if let Some(outcome) = outcome {
                discovered_tools.extend(
                    self.commit_single_outcome(input, uncommitted_calls, position, outcome)
                        .await?,
                );
            }
        }

        discovered_tools.extend(
            self.flush_and_commit_parallel_batch(
                &mut parallel_batch,
                &mut parallel_batch_start,
                input,
                uncommitted_calls,
                tools,
            )
            .await?,
        );

        Ok(discovered_tools)
    }

    async fn complete_pending_declared_as_failed(
        &self,
        input: &mut ExecuteToolBatch<'_>,
        uncommitted_calls: &mut HashSet<String>,
        error: &TurnError,
    ) {
        if uncommitted_calls.is_empty() {
            return;
        }
        let message = match error {
            TurnError::Aborted => "turn aborted before tool completion".to_string(),
            other => format!("tool orchestration failed before completion: {other}"),
        };
        for call in &input.batch.calls {
            if !uncommitted_calls.contains(&call.call_id) {
                continue;
            }
            let outcome = match error {
                TurnError::Aborted => ToolExecutionOutcome::cancelled(message.clone(), None),
                _ => ToolExecutionOutcome::failed(message.clone()),
            };
            let completion = crate::tool_types::tool_call_completion_arguments(
                call.tool_input.clone(),
                call.raw_arguments.clone(),
            );
            if let Err(commit_error) = finish_tool_call(
                &input.publisher,
                &call.call_id,
                call.name.clone(),
                &outcome,
                completion.arguments,
                completion.arguments_json,
            )
            .await
            {
                tracing::warn!(
                    call_id = %call.call_id,
                    error = %commit_error,
                    "failed to complete pending declared tool call after turn error"
                );
                continue;
            }
            uncommitted_calls.remove(&call.call_id);
        }
    }

    async fn request_approvals_and_resolve(
        &self,
        input: &ExecuteToolBatch<'_>,
        position: usize,
        approvals: &[PreparedToolApproval],
        tools: Arc<[ToolDefinition]>,
    ) -> Result<ToolExecutionOutcome, TurnError> {
        let call = &input.batch.calls[position];
        for PreparedToolApproval {
            prompt,
            rule_key,
            source,
        } in approvals
        {
            let (tx, rx) = oneshot::channel();
            let runtime = self.session.runtime();
            let _pending_approval =
                runtime.register_pending_approval(ToolCallId::from(call.call_id.as_str()), tx)?;
            input
                .publisher
                .durable(DurableEventPayload::ToolApprovalRequested {
                    call_id: call.call_id.clone().into(),
                    tool_name: call.name.clone(),
                    prompt: prompt.clone(),
                    rule_key: rule_key.clone(),
                    source: *source,
                    arguments: call.tool_input.clone(),
                })
                .await?;

            let (decision, resolution_detail) = tokio::select! {
                _ = self.cancellation_token.cancelled() => return Err(TurnError::Aborted),
                result = tokio::time::timeout(Duration::from_secs(APPROVAL_TIMEOUT_SECS), rx) => {
                    match result {
                        Ok(Ok(decision)) => (decision, None),
                        Ok(Err(_)) => (
                            ApprovalDecision::DenyOnce,
                            Some("approval receiver dropped".into()),
                        ),
                        Err(_) => (
                            ApprovalDecision::DenyOnce,
                            Some(format!("approval timed out after {APPROVAL_TIMEOUT_SECS}s")),
                        ),
                    }
                }
            };
            input
                .publisher
                .durable(DurableEventPayload::ToolApprovalResolved {
                    call_id: call.call_id.clone().into(),
                    decision,
                    detail: resolution_detail.clone(),
                })
                .await?;
            if matches!(
                decision,
                ApprovalDecision::AllowAlways | ApprovalDecision::DenyAlways
            ) {
                self.turn
                    .shared
                    .approval_history
                    .record_decision(rule_key.as_deref(), decision)
                    .await
                    .map_err(|error| TurnError::ApprovalHistory(error.to_string()))?;
            }
            if !decision.allows() {
                let reason = resolution_detail
                    .map(|detail| format!("Tool execution denied ({detail}, {source:?}): {prompt}"))
                    .unwrap_or_else(|| {
                        format!("Tool execution denied by user ({source:?}): {prompt}")
                    });
                return Ok(ToolExecutionOutcome::failed(reason));
            }
        }
        Ok(self.execute_single_tool(call.to_executable(), tools).await)
    }

    async fn flush_and_commit_parallel_batch(
        &self,
        parallel_batch: &mut Vec<ExecutableToolInvocation>,
        parallel_batch_start: &mut Option<usize>,
        input: &mut ExecuteToolBatch<'_>,
        uncommitted_calls: &mut HashSet<String>,
        tools: Arc<[ToolDefinition]>,
    ) -> Result<Vec<String>, TurnError> {
        let Some(batch_start) = parallel_batch_start.take() else {
            return Ok(Vec::new());
        };
        let batch_len = parallel_batch.len();
        let batch_end = batch_start + batch_len;
        // 连续性不变式：parallel_batch 条目按 position 升序逐调用压入（每个并行调用
        // 恰好一条），且批次只在遇到非并行调用时整体 flush——因此
        // `calls[batch_start..batch_end]` 与批次条目一一对应，可按下标回填 outcome。
        let mut outcomes = HashMap::new();

        self.flush_parallel_batch(parallel_batch, tools, &mut outcomes)
            .await?;

        self.commit_tool_outcomes(
            &input.batch.calls[batch_start..batch_end],
            outcomes,
            uncommitted_calls,
            input.state,
            Arc::clone(&input.publisher),
        )
        .await
    }

    async fn flush_parallel_batch(
        &self,
        batch: &mut Vec<ExecutableToolInvocation>,
        tools: Arc<[ToolDefinition]>,
        outcomes: &mut HashMap<usize, ToolExecutionOutcome>,
    ) -> Result<(), TurnError> {
        if batch.is_empty() {
            return Ok(());
        }
        let max_parallel = self.max_parallel_tool_calls();
        let mut pending = std::mem::take(batch).into_iter();
        let mut join_set = JoinSet::new();

        for _ in 0..max_parallel {
            let Some(call) = pending.next() else { break };
            self.spawn_tool_call(&mut join_set, call, Arc::clone(&tools));
        }

        loop {
            let joined = join_set.join_next().await;
            let Some(joined) = joined else {
                break;
            };
            let (index, outcome) = joined?;
            outcomes.insert(index, outcome);

            if let Some(call) = pending.next() {
                self.spawn_tool_call(&mut join_set, call, Arc::clone(&tools));
            }
        }
        Ok(())
    }

    fn spawn_tool_call(
        &self,
        join_set: &mut JoinSet<(usize, ToolExecutionOutcome)>,
        call: ExecutableToolInvocation,
        tools: Arc<[ToolDefinition]>,
    ) {
        let tool_registry = Arc::clone(&self.tool_registry);
        let ctx = self.make_runtime_context(tools);
        join_set.spawn(async move { execute_tool_call(tool_registry, ctx, call).await });
    }

    async fn execute_single_tool(
        &self,
        call: ExecutableToolInvocation,
        tools: Arc<[ToolDefinition]>,
    ) -> ToolExecutionOutcome {
        let (_index, outcome) = execute_tool_call(
            Arc::clone(&self.tool_registry),
            self.make_runtime_context(tools),
            call,
        )
        .await;
        outcome
    }

    async fn commit_single_outcome(
        &self,
        input: &mut ExecuteToolBatch<'_>,
        uncommitted_calls: &mut HashSet<String>,
        position: usize,
        outcome: ToolExecutionOutcome,
    ) -> Result<Vec<String>, TurnError> {
        let mut outcomes = HashMap::new();
        outcomes.insert(input.batch.calls[position].index, outcome);
        self.commit_tool_outcomes(
            &input.batch.calls[position..position + 1],
            outcomes,
            uncommitted_calls,
            input.state,
            Arc::clone(&input.publisher),
        )
        .await
    }
}
