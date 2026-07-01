//! Tool execution pipeline — preprocessing, parallel read scheduling,
//! result commit, and persistence.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::Duration,
};

use astrcode_core::{
    event::EventPayload,
    extension::{
        AfterToolResult, PostToolUseContext, PostToolUseResult, PreToolUseContext, PreToolUseResult,
    },
    permission::{ApprovalDecision, ApprovalSource, PermissionContext, PermissionDecision},
    storage::ToolResultArtifactReader,
    tool::{ExecutionMode, ToolDefinition, ToolResult},
    tool_access::ResourceAccess,
    tool_ui::{complete_questionnaire_content, is_awaiting_user_input_content},
    types::ToolCallId,
};
use astrcode_kernel::{ExtensionRuntime, ToolRegistry};
use tokio::{sync::oneshot, task::JoinSet};
use tokio_util::sync::CancellationToken;

use super::{
    deferred_tools::{discovered_deferred_tool_names, tool_is_visible, unavailable_tool_guidance},
    permission::APPROVAL_TIMEOUT_SECS,
    tool_deduplicator::{SameStepCheck, ToolCallDeduplicator},
    tool_exec::{ToolCallRuntimeContext, TurnToolContext, execute_tool_call},
    tool_json_repair::parse_and_repair_json,
    tool_types::{
        CommittedToolResults, DeclaredToolBatch, ExecutableToolInvocation,
        ExecuteDeclaredToolBatch, PreparedToolBatch, PreparedToolInvocation,
        PreparedToolInvocationOutcome, StreamedToolCall,
    },
    turn_context::{SharedTurnContext, TurnError},
    turn_publish::TurnEvents,
};
use crate::{
    early_tool_scheduler::{EarlyExecutionEntry, EarlyToolScheduler},
    llm_request_history::committed_tool_result_content_len,
    session::Session,
    tool_results::{
        MAX_TOOL_RESULTS_PER_MESSAGE_CHARS, TOOL_RESULT_PREVIEW_CHARS,
        is_persisted_tool_result_summary, is_tool_result_artifact_path,
        persisted_tool_result_summary, should_persist_tool_result, tool_result_inline_limit,
        tool_result_preview,
    },
    turn_stages::TurnState,
};

pub struct ToolCalls {
    turn: TurnToolContext,
    tool_registry: Arc<ToolRegistry>,
    extension_runner: Arc<dyn ExtensionRuntime>,
    session: Session,
    cancellation_token: CancellationToken,
}

impl ToolCalls {
    pub fn new(
        turn: TurnToolContext,
        tool_registry: Arc<ToolRegistry>,
        extension_runner: Arc<dyn ExtensionRuntime>,
        session: Session,
        cancellation_token: CancellationToken,
    ) -> Self {
        Self {
            turn,
            tool_registry,
            extension_runner,
            session,
            cancellation_token,
        }
    }

    pub fn list_definitions_with_prompt_metadata(
        &self,
    ) -> Vec<(
        ToolDefinition,
        Option<astrcode_core::tool::ToolPromptMetadata>,
    )> {
        self.tool_registry.list_definitions_with_prompt_metadata()
    }

    pub(crate) fn shared(&self) -> &SharedTurnContext {
        &self.turn.shared
    }

    pub(crate) fn shared_mut(&mut self) -> &mut SharedTurnContext {
        &mut self.turn.shared
    }

    /// 构建工具调用的运行时上下文。
    pub(crate) fn make_runtime_context(
        &self,
        tools: Arc<[ToolDefinition]>,
    ) -> ToolCallRuntimeContext {
        ToolCallRuntimeContext {
            turn: self.turn.clone(),
            tools,
            tool_result_reader: Some(
                Arc::new(self.session.clone()) as Arc<dyn ToolResultArtifactReader>
            ),
            cancellation_token: self.cancellation_token.clone(),
        }
    }

    /// 创建流式工具执行调度器。
    pub(crate) fn create_early_scheduler(
        &self,
        tools: Vec<ToolDefinition>,
        max_parallel: usize,
    ) -> EarlyToolScheduler {
        let tools_arc: Arc<[ToolDefinition]> = Arc::from(tools);
        EarlyToolScheduler::new(
            Arc::clone(&self.tool_registry),
            self.make_runtime_context(tools_arc),
            max_parallel,
        )
    }

    /// 准备单个工具调用：JSON 解析、可见性检查、PreToolUse 钩子、权限链、去重。
    ///
    /// 提取为独立方法以支持流式工具调用场景中 per-tool 增量准备。
    pub(crate) async fn prepare_single_tool_call(
        &self,
        tc: &StreamedToolCall,
        index: usize,
        tools: &[ToolDefinition],
        deduplicator: &mut ToolCallDeduplicator,
    ) -> Result<PreparedToolInvocation, TurnError> {
        let args: serde_json::Value = parse_and_repair_json(&tc.arguments, &tc.name);

        if !tool_is_visible(tools, &tc.name) {
            let guidance =
                unavailable_tool_guidance(&tc.name, tools, &self.tool_registry.list_definitions());
            let blocked_result = ToolResult {
                call_id: tc.call_id.clone(),
                content: guidance,
                is_error: true,
                error: Some(format!("tool '{}' is not available", tc.name)),
                metadata: Default::default(),
                duration_ms: None,
            };
            return Ok(PreparedToolInvocation {
                index,
                call_id: tc.call_id.clone(),
                name: tc.name.clone(),
                tool_input: args,
                mode: ExecutionMode::Sequential,
                outcome: PreparedToolInvocationOutcome::Blocked(blocked_result),
            });
        }

        let pre_ctx = PreToolUseContext {
            session_id: self.turn.shared.session_id.to_string(),
            working_dir: self.turn.shared.working_dir.clone(),
            model: self.turn.shared.model_selection(),
            tool_name: tc.name.clone(),
            tool_input: args.clone(),
            approval_mode: self.turn.shared.approval_mode,
            available_tools: tools.to_vec(),
            event_tx: self.turn.shared.turn_event_tx(),
            extension_event_sink: None,
            session_store_dir: self.turn.shared.session_store_dir.clone(),
        };

        let pre_hook_result = self.extension_runner.emit_pre_tool_use(pre_ctx).await?;

        let (tool_input, mut outcome) = match pre_hook_result {
            PreToolUseResult::Ask { prompt, rule_key } => (
                args,
                PreparedToolInvocationOutcome::NeedsApproval {
                    prompt,
                    rule_key,
                    source: ApprovalSource::Extension,
                },
            ),
            PreToolUseResult::ModifyInput { tool_input } => {
                (tool_input, PreparedToolInvocationOutcome::Ready)
            },
            PreToolUseResult::Block { reason } => {
                let outcome = PreparedToolInvocationOutcome::Blocked(ToolResult {
                    call_id: tc.call_id.clone(),
                    content: format!("Tool execution blocked by hook: {reason}"),
                    is_error: true,
                    error: Some(reason),
                    metadata: Default::default(),
                    duration_ms: None,
                });
                (args, outcome)
            },
            PreToolUseResult::Allow => (args, PreparedToolInvocationOutcome::Ready),
        };

        if matches!(outcome, PreparedToolInvocationOutcome::Ready) {
            outcome = self.evaluate_permission_chain(&tc.call_id, &tc.name, &tool_input);
        }

        let same_step = deduplicator.check_same_step(&tc.call_id, &tc.name, &tool_input);
        outcome = match (outcome, same_step) {
            (_, SameStepCheck::Duplicate) => PreparedToolInvocationOutcome::DuplicateSameStep,
            (PreparedToolInvocationOutcome::Ready, SameStepCheck::Primary) => {
                PreparedToolInvocationOutcome::Ready
            },
            (blocked @ PreparedToolInvocationOutcome::Blocked(_), SameStepCheck::Primary) => {
                blocked
            },
            (
                needs @ PreparedToolInvocationOutcome::NeedsApproval { .. },
                SameStepCheck::Primary,
            ) => needs,
            (PreparedToolInvocationOutcome::DuplicateSameStep, SameStepCheck::Primary) => {
                PreparedToolInvocationOutcome::DuplicateSameStep
            },
        };

        let mode = match &outcome {
            PreparedToolInvocationOutcome::Ready => self.tool_registry.execution_mode(&tc.name),
            PreparedToolInvocationOutcome::Blocked(_)
            | PreparedToolInvocationOutcome::DuplicateSameStep
            | PreparedToolInvocationOutcome::NeedsApproval { .. } => ExecutionMode::Sequential,
        };

        Ok(PreparedToolInvocation {
            index,
            call_id: tc.call_id.clone(),
            name: tc.name.clone(),
            tool_input,
            mode,
            outcome,
        })
    }

    pub(crate) async fn prepare_tool_batch(
        &self,
        tool_calls: &[StreamedToolCall],
        early_results: Vec<EarlyExecutionEntry>,
        visible_tools: &[ToolDefinition],
        state: &mut TurnState,
    ) -> Result<PreparedToolBatch, TurnError> {
        let mut pre_executed: HashMap<usize, ToolResult> = HashMap::new();
        let mut early_entries: HashMap<usize, _> = early_results
            .into_iter()
            .map(|entry| (entry.prepared.index, entry))
            .collect();
        let mut prepared = Vec::with_capacity(tool_calls.len());

        for (index, tool_call) in tool_calls.iter().enumerate() {
            if let Some(entry) = early_entries.remove(&index) {
                if let Some(result) = entry.result {
                    pre_executed.insert(entry.prepared.index, result);
                }
                prepared.push(entry.prepared);
                continue;
            }

            let prepared_call = self
                .prepare_single_tool_call(
                    tool_call,
                    index,
                    visible_tools,
                    state.tool_deduplicator_mut(),
                )
                .await?;
            prepared.push(prepared_call);
        }

        Ok(PreparedToolBatch {
            prepared,
            pre_executed,
        })
    }

    /// Persist provider tool requests after the assistant message has been durably recorded.
    ///
    /// Streaming early execution may prepare and even execute tools before the provider stream is
    /// fully drained, but the durable transcript must preserve the provider protocol order:
    /// assistant(tool_calls) -> tool results.
    pub(crate) async fn declare_tool_batch(
        &self,
        batch: PreparedToolBatch,
        publisher: &TurnEvents,
    ) -> Result<DeclaredToolBatch, TurnError> {
        declare_tool_batch(publisher, batch).await
    }

    fn evaluate_permission_chain(
        &self,
        call_id: &str,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> PreparedToolInvocationOutcome {
        let accesses = self
            .tool_registry
            .resource_accesses(
                tool_name,
                tool_input,
                Path::new(&self.turn.shared.working_dir),
            )
            .unwrap_or_else(|error| {
                tracing::debug!(
                    tool_name,
                    error = %error,
                    "resource_accesses parse failed during permission check; treating as exclusive lock"
                );
                vec![ResourceAccess::all()]
            });
        let ctx = PermissionContext {
            tool_name,
            tool_input,
            working_dir: Path::new(&self.turn.shared.working_dir),
            resource_accesses: &accesses,
            approval_mode: self.turn.shared.approval_mode,
            session_id: self.turn.shared.session_id.as_str(),
            is_child_session: self.turn.shared.is_child_session,
            child_tool_policy: self.turn.shared.child_tool_policy.as_ref(),
        };
        match self.turn.shared.permission_chain.decide(&ctx) {
            PermissionDecision::Allow => PreparedToolInvocationOutcome::Ready,
            PermissionDecision::Deny { reason } => {
                PreparedToolInvocationOutcome::Blocked(ToolResult {
                    call_id: call_id.to_string(),
                    content: reason.clone(),
                    is_error: true,
                    error: Some(reason),
                    metadata: Default::default(),
                    duration_ms: None,
                })
            },
            PermissionDecision::Ask { prompt, rule_key } => {
                if let Some(key) = rule_key.as_deref() {
                    if self.turn.shared.approval_history.is_allowed_always(key) {
                        return PreparedToolInvocationOutcome::Ready;
                    }
                    if self.turn.shared.approval_history.is_denied_always(key) {
                        return PreparedToolInvocationOutcome::Blocked(ToolResult {
                            call_id: call_id.to_string(),
                            content: format!("Denied by session approval memory ({key})"),
                            is_error: true,
                            error: Some(format!("Denied by session approval memory ({key})")),
                            metadata: Default::default(),
                            duration_ms: None,
                        });
                    }
                }
                PreparedToolInvocationOutcome::NeedsApproval {
                    prompt,
                    rule_key,
                    source: ApprovalSource::Core,
                }
            },
            PermissionDecision::Pass => PreparedToolInvocationOutcome::Blocked(ToolResult {
                call_id: call_id.to_string(),
                content: "permission chain returned Pass without resolution".into(),
                is_error: true,
                error: Some("permission chain returned Pass without resolution".into()),
                metadata: Default::default(),
                duration_ms: None,
            }),
        }
    }

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
        call: &super::tool_types::PreparedToolInvocation,
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

    /// 提交工具执行结果。
    ///
    /// 对每个已执行的工具调用依次处理：
    /// 1. 分发 `PostToolUse` 扩展钩子，允许扩展修改结果内容或阻止。
    /// 2. 通过 `TurnEvents` 发送 durable `ToolCallCompleted`。
    /// 3. 将工具结果写入 turn 输出聚合（projection 为 LLM 历史 SSOT）。
    pub async fn commit_tool_results(
        &self,
        prepared: &[PreparedToolInvocation],
        mut results: HashMap<usize, ToolResult>,
        pending_declared: &mut HashSet<String>,
        state: &mut TurnState,
        publisher: Arc<TurnEvents>,
    ) -> Result<CommittedToolResults, TurnError> {
        let mut pending_results = Vec::with_capacity(prepared.len());
        for call in prepared {
            let mut result = results
                .remove(&call.index)
                .unwrap_or_else(|| missing_tool_result(call));

            if matches!(&call.outcome, PreparedToolInvocationOutcome::Ready) {
                if result.is_error && result.error.is_none() {
                    result.error = Some(result.content.clone());
                }

                let post_ctx = PostToolUseContext {
                    session_id: self.turn.shared.session_id.to_string(),
                    working_dir: self.turn.shared.working_dir.clone(),
                    model: self.turn.shared.model_selection(),
                    tool_name: call.name.clone(),
                    tool_input: call.tool_input.clone(),
                    tool_result: result.clone(),
                    is_error: result.is_error,
                    event_tx: self.turn.shared.turn_event_tx(),
                    extension_event_sink: None,
                    session_store_dir: self.turn.shared.session_store_dir.clone(),
                };

                match self.extension_runner.emit_post_tool_use(post_ctx).await? {
                    PostToolUseResult::ModifyResult { content } => {
                        let error = result.is_error.then(|| content.clone());
                        result.content = content;
                        result.error = error;
                    },
                    PostToolUseResult::Block { reason } => {
                        result.content = format!("Tool result blocked by hook: {reason}");
                        result.is_error = true;
                        result.error = Some(reason);
                    },
                    PostToolUseResult::Allow => {},
                }

                // PostToolUseFailure: 通知型钩子，结果不影响流程。
                if result.is_error {
                    let fail_ctx = astrcode_core::extension::PostToolUseFailureContext {
                        session_id: self.turn.shared.session_id.to_string(),
                        working_dir: self.turn.shared.working_dir.clone(),
                        model: self.turn.shared.model_selection(),
                        tool_name: call.name.clone(),
                        tool_input: call.tool_input.clone(),
                        error: result
                            .error
                            .clone()
                            .unwrap_or_else(|| result.content.clone()),
                        tool_result: result.clone(),
                    };
                    self.extension_runner
                        .emit_post_tool_use_failure(fail_ctx)
                        .await;
                }
            }

            if !matches!(
                &call.outcome,
                PreparedToolInvocationOutcome::DuplicateSameStep
            ) {
                state
                    .tool_deduplicator_mut()
                    .finalize_result(&call.call_id, &result);
            }

            pending_results.push(AfterToolResult {
                call_id: call.call_id.clone().into(),
                tool_name: call.name.clone(),
                tool_input: call.tool_input.clone(),
                tool_result: result,
            });
        }

        for pending in &mut pending_results {
            // 对于超过 inline 限制的工具结果，先持久化到磁盘并替换为摘要引用，再继续后续处理。
            let call_id = pending.call_id.to_string();
            self.persist_large_tool_result(&pending.tool_name, &call_id, &mut pending.tool_result)
                .await?;
        }
        let model = publisher.snapshot_model().await?;
        let committed_tool_result_chars = committed_tool_result_content_len(&model);
        // 当累计工具结果超过消息字符预算时，按体积从大到小持久化，直到总量回到预算内。
        self.enforce_tool_result_message_budget(committed_tool_result_chars, &mut pending_results)
            .await?;

        let mut committed = CommittedToolResults::default();
        for pending in pending_results {
            let AfterToolResult {
                call_id,
                tool_name,
                tool_input,
                tool_result,
            } = pending;
            committed
                .discovered_tools
                .extend(discovered_deferred_tool_names(&tool_result));
            committed.tool_results.push(AfterToolResult {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                tool_input: tool_input.clone(),
                tool_result: tool_result.clone(),
            });
            let arguments = tool_input.to_string();
            complete_tool_call(
                &publisher,
                call_id.as_str(),
                tool_name.clone(),
                tool_result.clone(),
                arguments,
                Some(tool_input),
            )
            .await?;
            pending_declared.remove(call_id.as_str());
            state.record_tool_result(tool_result);
        }

        Ok(committed)
    }

    /// 检查工具结果是否超过 inline 限制，超限则持久化到磁盘并替换为摘要引用。
    async fn persist_large_tool_result(
        &self,
        tool_name: &str,
        call_id: &str,
        result: &mut ToolResult,
    ) -> Result<(), TurnError> {
        let Some(inline_limit) = tool_result_inline_limit(tool_name) else {
            return Ok(());
        };
        if is_persisted_tool_result_summary(&result.content) {
            return Ok(());
        }
        if !should_persist_tool_result(&result.content, inline_limit) {
            return Ok(());
        }
        self.persist_tool_result(tool_name, call_id, result).await
    }

    /// 将工具结果写入 session 存储并替换为摘要引用（含 preview 和 artifact 路径）。
    async fn persist_tool_result(
        &self,
        tool_name: &str,
        call_id: &str,
        result: &mut ToolResult,
    ) -> Result<(), TurnError> {
        if result.metadata.contains_key("persistedToolResult") {
            return Ok(());
        }
        if is_artifact_read(result) {
            return Ok(());
        }
        if result
            .metadata
            .get("path")
            .and_then(|value| value.as_str())
            .is_some_and(is_tool_result_artifact_path)
        {
            return Ok(());
        }
        if is_persisted_tool_result_summary(&result.content) {
            return Ok(());
        }
        let original_content = result.content.clone();
        let preview = tool_result_preview(&original_content, TOOL_RESULT_PREVIEW_CHARS);
        let reference = self
            .session
            .write_tool_artifact(astrcode_core::storage::ToolResultArtifactInput {
                call_id: call_id.to_string(),
                tool_name: tool_name.to_string(),
                content: original_content,
            })
            .await?;
        result.metadata.insert(
            "persistedToolResult".into(),
            serde_json::json!({
                "bytes": reference.bytes,
                "path": &reference.path,
            }),
        );
        result.content = persisted_tool_result_summary(&reference, &preview);
        if result.is_error {
            result.error = Some(result.content.clone());
        }
        Ok(())
    }

    /// 当累计工具结果超过消息字符预算时，按体积从大到小持久化，直到总量回到预算内。
    async fn enforce_tool_result_message_budget(
        &self,
        committed_tool_result_chars: usize,
        pending_results: &mut [AfterToolResult],
    ) -> Result<(), TurnError> {
        let mut total: usize = committed_tool_result_chars
            + pending_results
                .iter()
                .map(|pending| pending.tool_result.content.len())
                .sum::<usize>();
        if total <= MAX_TOOL_RESULTS_PER_MESSAGE_CHARS {
            return Ok(());
        }

        let mut candidates: Vec<usize> = pending_results
            .iter()
            .enumerate()
            .filter_map(|(index, pending)| {
                let can_persist =
                    tool_result_inline_limit(&pending.tool_name).is_some_and(|limit| {
                        should_persist_tool_result(&pending.tool_result.content, limit)
                    }) && !pending
                        .tool_result
                        .metadata
                        .contains_key("persistedToolResult")
                        && !is_artifact_read(&pending.tool_result)
                        && !is_persisted_tool_result_summary(&pending.tool_result.content)
                        && !pending
                            .tool_result
                            .metadata
                            .get("path")
                            .and_then(|value| value.as_str())
                            .is_some_and(is_tool_result_artifact_path);
                can_persist.then_some(index)
            })
            .collect();
        candidates.sort_by(|left, right| {
            pending_results[*right]
                .tool_result
                .content
                .len()
                .cmp(&pending_results[*left].tool_result.content.len())
        });

        for index in candidates {
            if total <= MAX_TOOL_RESULTS_PER_MESSAGE_CHARS {
                break;
            }
            let pending = &mut pending_results[index];
            let before = pending.tool_result.content.len();
            self.persist_tool_result(
                &pending.tool_name,
                pending.call_id.as_str(),
                &mut pending.tool_result,
            )
            .await?;
            let after = pending.tool_result.content.len();
            total = total.saturating_sub(before).saturating_add(after);
        }

        Ok(())
    }
}

fn is_artifact_read(result: &ToolResult) -> bool {
    result.metadata.get("source").and_then(|v| v.as_str()) == Some("toolResultArtifact")
}

// ─── Tool event & message helpers ────────────────────────────────────────

async fn declare_tool_batch(
    publisher: &TurnEvents,
    batch: PreparedToolBatch,
) -> Result<DeclaredToolBatch, TurnError> {
    for call in &batch.prepared {
        declare_tool_call(publisher, call).await?;
    }
    Ok(DeclaredToolBatch {
        prepared: batch.prepared,
        pre_executed: batch.pre_executed,
    })
}

async fn declare_tool_call(
    publisher: &TurnEvents,
    call: &PreparedToolInvocation,
) -> Result<(), TurnError> {
    publisher
        .durable(EventPayload::ToolCallRequested {
            call_id: call.call_id.clone().into(),
            tool_name: call.name.clone(),
            arguments: call.tool_input.clone(),
        })
        .await
}

async fn complete_tool_call(
    publisher: &TurnEvents,
    call_id: &str,
    tool_name: String,
    result: ToolResult,
    arguments: String,
    arguments_json: Option<serde_json::Value>,
) -> Result<(), TurnError> {
    publisher
        .durable(EventPayload::ToolCallCompleted {
            call_id: call_id.into(),
            tool_name,
            result,
            arguments,
            arguments_json,
        })
        .await
}

fn missing_tool_result(call: &PreparedToolInvocation) -> ToolResult {
    let message = format!("Tool '{}' did not produce a result", call.name);
    ToolResult {
        call_id: call.call_id.clone(),
        content: message.clone(),
        is_error: true,
        error: Some(message),
        metadata: Default::default(),
        duration_ms: None,
    }
}

fn tool_ui_response_error_result(call_id: &str, message: &str) -> ToolResult {
    ToolResult {
        call_id: call_id.to_string(),
        content: message.to_string(),
        is_error: true,
        error: Some(message.to_string()),
        metadata: Default::default(),
        duration_ms: None,
    }
}
