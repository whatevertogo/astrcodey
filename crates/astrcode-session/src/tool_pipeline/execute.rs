use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use astrcode_core::{
    event::EventPayload,
    permission::{ApprovalDecision, ApprovalSource},
    tool::{ExecutionMode, ToolDefinition, ToolResult},
    tool_ui::{complete_questionnaire_content, is_awaiting_user_input_content},
    types::ToolCallId,
};
use tokio::{sync::oneshot, task::JoinSet};

use super::{
    ToolCalls,
    events::{complete_tool_call, tool_ui_response_error_result},
};
use crate::{
    permission::APPROVAL_TIMEOUT_SECS,
    tool_exec::execute_tool_call,
    tool_types::{
        CommittedToolResults, ExecutableToolInvocation, ExecuteDeclaredToolBatch,
        PreparedToolInvocation, PreparedToolInvocationOutcome,
    },
    turn_context::TurnError,
    turn_publish::TurnEvents,
};

impl ToolCalls {
    /// 执行已预处理的工具调用。
    ///
    /// 只读工具按连续批次并发执行；写入、shell、terminal 以及审批/阻止结果都会先刷新当前
    /// 只读批次，再按原始顺序串行处理。
    pub async fn execute_and_commit(
        &self,
        mut input: ExecuteDeclaredToolBatch<'_>,
    ) -> Result<CommittedToolResults, TurnError> {
        let mut pending_declared = input
            .declared
            .prepared
            .iter()
            .map(|call| call.call_id.clone())
            .collect::<HashSet<_>>();
        let result = self
            .execute_declared_batch(&mut input, &mut pending_declared)
            .await;
        if let Err(error) = &result {
            self.complete_pending_declared_as_failed(&mut input, &mut pending_declared, error)
                .await;
        }
        result
    }

    async fn execute_declared_batch(
        &self,
        input: &mut ExecuteDeclaredToolBatch<'_>,
        pending_declared: &mut HashSet<String>,
    ) -> Result<CommittedToolResults, TurnError> {
        let mut committed = CommittedToolResults::default();
        let tools = Arc::from(input.tools);
        let mut parallel_batch = Vec::new();
        let mut parallel_batch_start = None;

        for position in 0..input.declared.prepared.len() {
            if self.cancellation_token.is_cancelled() {
                return Err(TurnError::Aborted);
            }
            let call = input.declared.prepared[position].clone();
            match &call.outcome {
                PreparedToolInvocationOutcome::Blocked(result) => {
                    committed.extend(
                        self.flush_and_commit_parallel_batch(
                            &mut parallel_batch,
                            &mut parallel_batch_start,
                            input,
                            pending_declared,
                            Arc::clone(&tools),
                        )
                        .await?,
                    );
                    committed.extend(
                        self.commit_single_result(
                            input,
                            pending_declared,
                            position,
                            result.clone(),
                        )
                        .await?,
                    );
                },
                PreparedToolInvocationOutcome::DuplicateSameStep => {
                    committed.extend(
                        self.flush_and_commit_parallel_batch(
                            &mut parallel_batch,
                            &mut parallel_batch_start,
                            input,
                            pending_declared,
                            Arc::clone(&tools),
                        )
                        .await?,
                    );
                    let result = input
                        .state
                        .tool_deduplicator()
                        .await_same_step_result(&input.declared.prepared[position].call_id)
                        .await;
                    committed.extend(
                        self.commit_single_result(input, pending_declared, position, result)
                            .await?,
                    );
                },
                PreparedToolInvocationOutcome::NeedsApproval {
                    prompt,
                    rule_key,
                    source,
                } => {
                    committed.extend(
                        self.flush_and_commit_parallel_batch(
                            &mut parallel_batch,
                            &mut parallel_batch_start,
                            input,
                            pending_declared,
                            Arc::clone(&tools),
                        )
                        .await?,
                    );
                    let result = self
                        .request_approval_and_resolve(
                            input,
                            position,
                            prompt.clone(),
                            rule_key.clone(),
                            *source,
                            Arc::clone(&tools),
                        )
                        .await?;
                    committed.extend(
                        self.commit_single_result(input, pending_declared, position, result)
                            .await?,
                    );
                },
                PreparedToolInvocationOutcome::Ready if call.mode == ExecutionMode::Parallel => {
                    if let Some(result) = input.declared.pre_executed.remove(&call.index) {
                        committed.extend(
                            self.flush_and_commit_parallel_batch(
                                &mut parallel_batch,
                                &mut parallel_batch_start,
                                input,
                                pending_declared,
                                Arc::clone(&tools),
                            )
                            .await?,
                        );
                        committed.extend(
                            self.commit_single_result(input, pending_declared, position, result)
                                .await?,
                        );
                    } else {
                        if parallel_batch_start.is_none() {
                            parallel_batch_start = Some(position);
                        }
                        parallel_batch.push(call.to_executable());
                    }
                },
                PreparedToolInvocationOutcome::Ready => {
                    committed.extend(
                        self.flush_and_commit_parallel_batch(
                            &mut parallel_batch,
                            &mut parallel_batch_start,
                            input,
                            pending_declared,
                            Arc::clone(&tools),
                        )
                        .await?,
                    );
                    let result = if let Some(r) = input.declared.pre_executed.remove(&call.index) {
                        r
                    } else {
                        self.execute_single_tool(call.to_executable(), Arc::clone(&tools))
                            .await?
                    };
                    committed.extend(
                        self.commit_single_result(input, pending_declared, position, result)
                            .await?,
                    );
                },
            }
        }

        committed.extend(
            self.flush_and_commit_parallel_batch(
                &mut parallel_batch,
                &mut parallel_batch_start,
                input,
                pending_declared,
                tools,
            )
            .await?,
        );

        Ok(committed)
    }

    async fn complete_pending_declared_as_failed(
        &self,
        input: &mut ExecuteDeclaredToolBatch<'_>,
        pending_declared: &mut HashSet<String>,
        error: &TurnError,
    ) {
        if pending_declared.is_empty() {
            return;
        }
        let message = match error {
            TurnError::Aborted => "Tool execution cancelled before completion".to_string(),
            other => format!("Tool execution failed before completion: {other}"),
        };
        for call in &input.declared.prepared {
            if !pending_declared.remove(&call.call_id) {
                continue;
            }
            let result = ToolResult {
                call_id: call.call_id.clone(),
                content: message.clone(),
                is_error: true,
                error: Some(message.clone()),
                metadata: Default::default(),
                duration_ms: None,
            };
            if let Err(commit_error) = complete_tool_call(
                &input.publisher,
                &call.call_id,
                call.name.clone(),
                result.clone(),
                call.tool_input.to_string(),
                Some(call.tool_input.clone()),
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
            input.state.record_tool_result(result);
        }
    }

    async fn request_approval_and_resolve(
        &self,
        input: &ExecuteDeclaredToolBatch<'_>,
        position: usize,
        prompt: String,
        rule_key: Option<String>,
        source: ApprovalSource,
        tools: Arc<[ToolDefinition]>,
    ) -> Result<ToolResult, TurnError> {
        let call = &input.declared.prepared[position];
        let (tx, rx) = oneshot::channel();
        let runtime = self.session.runtime();
        let _pending_approval =
            runtime.register_pending_approval(ToolCallId::from(call.call_id.as_str()), tx);
        input
            .publisher
            .durable(EventPayload::ToolApprovalRequested {
                call_id: call.call_id.clone().into(),
                tool_name: call.name.clone(),
                prompt: prompt.clone(),
                rule_key: rule_key.clone(),
                source,
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
            .durable(EventPayload::ToolApprovalResolved {
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
                .record_decision(rule_key.as_deref(), decision);
            if let Some(dir) = self.turn.shared.session_store_dir.as_deref() {
                let path = crate::permission::approval_history_path(dir);
                let _ = self.turn.shared.approval_history.persist_to(&path);
            }
        }
        if decision.allows() {
            return self.execute_single_tool(call.to_executable(), tools).await;
        }
        let reason = resolution_detail
            .map(|detail| format!("Tool execution denied ({detail}, {source:?}): {prompt}"))
            .unwrap_or_else(|| format!("Tool execution denied by user ({source:?}): {prompt}"));
        Ok(ToolResult {
            call_id: call.call_id.clone(),
            content: reason.clone(),
            is_error: true,
            error: Some(reason),
            metadata: Default::default(),
            duration_ms: None,
        })
    }

    async fn flush_and_commit_parallel_batch(
        &self,
        parallel_batch: &mut Vec<ExecutableToolInvocation>,
        parallel_batch_start: &mut Option<usize>,
        input: &mut ExecuteDeclaredToolBatch<'_>,
        pending_declared: &mut HashSet<String>,
        tools: Arc<[ToolDefinition]>,
    ) -> Result<CommittedToolResults, TurnError> {
        let Some(batch_start) = parallel_batch_start.take() else {
            return Ok(CommittedToolResults::default());
        };
        let batch_len = parallel_batch.len();
        let batch_end = batch_start + batch_len;
        let mut results = HashMap::new();

        self.flush_parallel_batch(parallel_batch, tools, &mut results)
            .await?;

        self.commit_tool_results(
            &input.declared.prepared[batch_start..batch_end],
            results,
            pending_declared,
            input.state,
            Arc::clone(&input.publisher),
        )
        .await
    }

    async fn flush_parallel_batch(
        &self,
        batch: &mut Vec<ExecutableToolInvocation>,
        tools: Arc<[ToolDefinition]>,
        results: &mut HashMap<usize, ToolResult>,
    ) -> Result<(), TurnError> {
        if batch.is_empty() {
            return Ok(());
        }
        let max_parallel = self
            .session
            .caps()
            .read_effective()
            .agent
            .tool_max_parallel_calls
            .max(1);
        let mut pending = std::mem::take(batch).into_iter();
        let mut join_set = JoinSet::new();

        for _ in 0..max_parallel {
            let Some(call) = pending.next() else { break };
            self.spawn_tool_call(&mut join_set, call, Arc::clone(&tools));
        }

        loop {
            let joined = tokio::select! {
                _ = self.cancellation_token.cancelled() => {
                    join_set.abort_all();
                    return Err(TurnError::Aborted);
                },
                joined = join_set.join_next() => joined,
            };
            let Some(joined) = joined else {
                break;
            };
            let (index, result) = joined?;
            results.insert(index, result);

            if let Some(call) = pending.next() {
                self.spawn_tool_call(&mut join_set, call, Arc::clone(&tools));
            }
        }
        Ok(())
    }

    fn spawn_tool_call(
        &self,
        join_set: &mut JoinSet<(usize, ToolResult)>,
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
    ) -> Result<ToolResult, TurnError> {
        let (_index, result) = execute_tool_call(
            Arc::clone(&self.tool_registry),
            self.make_runtime_context(tools),
            call,
        )
        .await;
        Ok(result)
    }

    async fn commit_single_result(
        &self,
        input: &mut ExecuteDeclaredToolBatch<'_>,
        pending_declared: &mut HashSet<String>,
        position: usize,
        mut result: ToolResult,
    ) -> Result<CommittedToolResults, TurnError> {
        if is_awaiting_user_input_content(&result.content) {
            result = self
                .await_tool_ui_response(
                    &input.declared.prepared[position],
                    result,
                    Arc::clone(&input.publisher),
                )
                .await?;
        }

        let mut results = HashMap::new();
        results.insert(input.declared.prepared[position].index, result);
        self.commit_tool_results(
            &input.declared.prepared[position..position + 1],
            results,
            pending_declared,
            input.state,
            Arc::clone(&input.publisher),
        )
        .await
    }

    async fn await_tool_ui_response(
        &self,
        call: &PreparedToolInvocation,
        mut result: ToolResult,
        publisher: Arc<TurnEvents>,
    ) -> Result<ToolResult, TurnError> {
        publisher
            .durable(EventPayload::ToolCallInteractionPending {
                call_id: call.call_id.clone().into(),
                content: result.content.clone(),
                metadata: result.metadata.clone(),
            })
            .await?;

        let (tx, rx) = oneshot::channel();
        let runtime = self.session.runtime();
        let _pending_response =
            runtime.register_pending_tool_ui_response(ToolCallId::from(call.call_id.as_str()), tx);

        let answers = tokio::select! {
            _ = self.cancellation_token.cancelled() => return Err(TurnError::Aborted),
            response = tokio::time::timeout(Duration::from_secs(APPROVAL_TIMEOUT_SECS), rx) => {
                match response {
                    Ok(Ok(answers)) => answers,
                    Ok(Err(_)) => {
                        return Ok(tool_ui_response_error_result(
                            &call.call_id,
                            "tool UI response channel closed before user answered",
                        ));
                    }
                    Err(_) => {
                        return Ok(tool_ui_response_error_result(
                            &call.call_id,
                            &format!(
                                "tool UI response timed out after {APPROVAL_TIMEOUT_SECS}s"
                            ),
                        ));
                    }
                }
            }
        };

        let questions = call
            .tool_input
            .get("questions")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([]));
        let content = match complete_questionnaire_content(&questions, &answers) {
            Ok(content) => content,
            Err(error) => {
                return Ok(tool_ui_response_error_result(&call.call_id, &error));
            },
        };
        result.content = content;
        Ok(result)
    }
}
