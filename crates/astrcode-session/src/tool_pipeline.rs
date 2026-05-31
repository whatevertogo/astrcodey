//! Tool execution pipeline — preprocessing, conflict-graph scheduling,
//! result commit, and persistence.

use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

use astrcode_core::{
    event::EventPayload,
    extension::{PostToolUseContext, PostToolUseResult, PreToolUseContext, PreToolUseResult},
    permission::{ApprovalDecision, ApprovalSource, PermissionContext, PermissionDecision},
    storage::ToolResultArtifactReader,
    tool::{ToolDefinition, ToolResult},
    tool_access::ResourceAccess,
    tool_ui::{complete_questionnaire_content, is_awaiting_user_input_content},
    types::ToolCallId,
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_tools::registry::ToolRegistry;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{
    deferred_tools::{discovered_deferred_tool_names, tool_is_visible, unavailable_tool_guidance},
    permission::APPROVAL_TIMEOUT_SECS,
    tool_deduplicator::{SameStepCheck, ToolCallDeduplicator},
    tool_exec::{ToolCallRuntimeContext, TurnToolContext, execute_tool_call},
    tool_json_repair::parse_and_repair_json,
    tool_scheduler::ToolScheduler,
    tool_types::{
        CommitToolResults, ExecuteToolCalls, PendingCommittedToolResult, PendingToolCall,
        PreparedToolCall, PreparedToolOutcome,
    },
    turn_context::{SharedTurnContext, TurnError},
    turn_publish::TurnEvents,
};
use crate::{
    llm_request_history::committed_tool_result_content_len,
    session::Session,
    tool_results::{
        MAX_TOOL_RESULTS_PER_MESSAGE_CHARS, TOOL_RESULT_PREVIEW_CHARS,
        is_persisted_tool_result_summary, is_tool_result_artifact_path,
        persisted_tool_result_summary, should_persist_tool_result, tool_result_inline_limit,
        tool_result_preview,
    },
};

pub struct ToolCalls {
    turn: TurnToolContext,
    tool_registry: Arc<ToolRegistry>,
    extension_runner: Arc<ExtensionRunner>,
    session: Session,
    cancellation_token: CancellationToken,
}

impl ToolCalls {
    pub fn new(
        turn: TurnToolContext,
        tool_registry: Arc<ToolRegistry>,
        extension_runner: Arc<ExtensionRunner>,
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
    fn make_runtime_context(
        &self,
        tools: &[ToolDefinition],
        publisher: Arc<TurnEvents>,
    ) -> ToolCallRuntimeContext {
        ToolCallRuntimeContext {
            turn: self.turn.clone(),
            tools: tools.to_vec(),
            tool_result_reader: Some(
                Arc::new(self.session.clone()) as Arc<dyn ToolResultArtifactReader>
            ),
            publisher,
            cancellation_token: self.cancellation_token.clone(),
        }
    }

    /// 预处理工具调用列表。
    ///
    /// 对每个待执行的工具调用依次执行：
    /// 1. 解析 JSON 参数（解析失败时尝试修复，仍失败则使用空对象并记录警告）。
    /// 2. 检查工具白名单，不在白名单中的工具直接标记为 `Blocked`。
    /// 3. 分发 `PreToolUse` 扩展钩子，允许扩展修改输入或阻止执行。
    /// 4. 解析资源访问声明，供冲突图调度器判定并行性。
    pub async fn prepare_tool_calls(
        &self,
        tool_calls: &[PendingToolCall],
        tools: &[ToolDefinition],
        publisher: &TurnEvents,
        deduplicator: &mut ToolCallDeduplicator,
    ) -> Result<Vec<PreparedToolCall>, TurnError> {
        let mut prepared = Vec::with_capacity(tool_calls.len());

        for (index, tc) in tool_calls.iter().enumerate() {
            let args: serde_json::Value = parse_and_repair_json(&tc.arguments, &tc.name);

            if !tool_is_visible(tools, &tc.name) {
                let guidance = unavailable_tool_guidance(
                    &tc.name,
                    tools,
                    &self.tool_registry.list_definitions(),
                );
                let blocked_result = ToolResult {
                    call_id: tc.call_id.clone(),
                    content: guidance,
                    is_error: true,
                    error: Some(format!("tool '{}' is not available", tc.name)),
                    metadata: Default::default(),
                    duration_ms: None,
                };
                send_tool_requested(publisher, tc, &args).await?;
                prepared.push(PreparedToolCall {
                    index,
                    call_id: tc.call_id.clone(),
                    name: tc.name.clone(),
                    tool_input: args,
                    accesses: Vec::new(),
                    outcome: PreparedToolOutcome::Blocked(blocked_result),
                });
                continue;
            }

            let pre_ctx = PreToolUseContext {
                session_id: self.turn.shared.session_id.to_string(),
                working_dir: self.turn.shared.working_dir.clone(),
                model: self.turn.shared.model_selection(),
                tool_name: tc.name.clone(),
                tool_input: args.clone(),
                approval_mode: self.turn.shared.approval_mode,
                available_tools: tools.to_vec(),
                event_tx: self.turn.shared.turn_event_tx.clone(),
                extension_event_sink: None,
                session_store_dir: self.turn.shared.session_store_dir.clone(),
            };

            let pre_hook_result = self.extension_runner.emit_pre_tool_use(pre_ctx).await?;

            let (tool_input, mut outcome) = match &pre_hook_result {
                PreToolUseResult::Ask { prompt, rule_key } => (
                    args,
                    PreparedToolOutcome::NeedsApproval {
                        prompt: prompt.clone(),
                        rule_key: rule_key.clone(),
                        source: ApprovalSource::Extension,
                    },
                ),
                PreToolUseResult::ModifyInput { tool_input } => {
                    (tool_input.clone(), PreparedToolOutcome::Ready)
                },
                PreToolUseResult::Block { reason } => {
                    let outcome = PreparedToolOutcome::Blocked(ToolResult {
                        call_id: tc.call_id.clone(),
                        content: format!("Tool execution blocked by hook: {reason}"),
                        is_error: true,
                        error: Some(reason.clone()),
                        metadata: Default::default(),
                        duration_ms: None,
                    });
                    (args, outcome)
                },
                PreToolUseResult::Allow => (args, PreparedToolOutcome::Ready),
            };

            if matches!(outcome, PreparedToolOutcome::Ready) {
                outcome = self.evaluate_permission_chain(&tc.call_id, &tc.name, &tool_input);
            }

            let same_step = deduplicator.check_same_step(&tc.call_id, &tc.name, &tool_input);
            outcome = match (outcome, same_step) {
                (_, SameStepCheck::Duplicate) => PreparedToolOutcome::DuplicateSameStep,
                (PreparedToolOutcome::Ready, SameStepCheck::Primary) => PreparedToolOutcome::Ready,
                (blocked @ PreparedToolOutcome::Blocked(_), SameStepCheck::Primary) => blocked,
                (needs @ PreparedToolOutcome::NeedsApproval { .. }, SameStepCheck::Primary) => {
                    needs
                },
                (PreparedToolOutcome::DuplicateSameStep, SameStepCheck::Primary) => {
                    PreparedToolOutcome::DuplicateSameStep
                },
            };

            send_tool_requested(publisher, tc, &tool_input).await?;

            let accesses = match &outcome {
                PreparedToolOutcome::Ready => self
                    .tool_registry
                    .resource_accesses(
                        &tc.name,
                        &tool_input,
                        Path::new(&self.turn.shared.working_dir),
                    )
                    .unwrap_or_else(|_| vec![ResourceAccess::all()]),
                PreparedToolOutcome::Blocked(_)
                | PreparedToolOutcome::DuplicateSameStep
                | PreparedToolOutcome::NeedsApproval { .. } => Vec::new(),
            };

            prepared.push(PreparedToolCall {
                index,
                call_id: tc.call_id.clone(),
                name: tc.name.clone(),
                tool_input,
                accesses,
                outcome,
            });
        }

        Ok(prepared)
    }

    fn evaluate_permission_chain(
        &self,
        call_id: &str,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> PreparedToolOutcome {
        let accesses = self
            .tool_registry
            .resource_accesses(
                tool_name,
                tool_input,
                Path::new(&self.turn.shared.working_dir),
            )
            .unwrap_or_else(|_| vec![ResourceAccess::all()]);
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
            PermissionDecision::Allow => PreparedToolOutcome::Ready,
            PermissionDecision::Deny { reason } => PreparedToolOutcome::Blocked(ToolResult {
                call_id: call_id.to_string(),
                content: reason.clone(),
                is_error: true,
                error: Some(reason),
                metadata: Default::default(),
                duration_ms: None,
            }),
            PermissionDecision::Ask { prompt, rule_key } => {
                if let Some(key) = rule_key.as_deref() {
                    if self.turn.shared.approval_history.is_allowed_always(key) {
                        return PreparedToolOutcome::Ready;
                    }
                    if self.turn.shared.approval_history.is_denied_always(key) {
                        return PreparedToolOutcome::Blocked(ToolResult {
                            call_id: call_id.to_string(),
                            content: format!("Denied by session approval memory ({key})"),
                            is_error: true,
                            error: Some(format!("Denied by session approval memory ({key})")),
                            metadata: Default::default(),
                            duration_ms: None,
                        });
                    }
                }
                PreparedToolOutcome::NeedsApproval {
                    prompt,
                    rule_key,
                    source: ApprovalSource::Core,
                }
            },
            PermissionDecision::Pass => PreparedToolOutcome::Blocked(ToolResult {
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
    /// 所有 Ready 调用提交给冲突图调度器；调度器基于资源访问声明决定并行/串行。
    /// 结果按 LLM 返回的原始顺序依次 commit。
    pub async fn execute_and_commit(
        &self,
        input: ExecuteToolCalls<'_>,
    ) -> Result<Vec<String>, TurnError> {
        let max_parallel = self
            .session
            .caps()
            .read_effective()
            .agent
            .tool_max_parallel_calls
            .max(1);
        let mut scheduler = ToolScheduler::new(max_parallel);
        let mut discovered_tools = Vec::new();

        enum ResultSource {
            Blocked(ToolResult),
            Scheduled(oneshot::Receiver<(usize, ToolResult)>),
            DuplicateSameStep,
            PendingApproval {
                rx: oneshot::Receiver<ApprovalDecision>,
                prompt: String,
                rule_key: Option<String>,
                source: ApprovalSource,
            },
        }

        let mut sources = Vec::with_capacity(input.prepared.len());
        for call in input.prepared {
            if self.cancellation_token.is_cancelled() {
                return Err(TurnError::Aborted);
            }
            match &call.outcome {
                PreparedToolOutcome::Blocked(result) => {
                    sources.push(ResultSource::Blocked(result.clone()));
                },
                PreparedToolOutcome::DuplicateSameStep => {
                    sources.push(ResultSource::DuplicateSameStep);
                },
                PreparedToolOutcome::NeedsApproval {
                    prompt,
                    rule_key,
                    source,
                } => {
                    let (tx, rx) = oneshot::channel();
                    self.session
                        .runtime()
                        .register_pending_approval(ToolCallId::from(call.call_id.as_str()), tx);
                    input
                        .publisher
                        .durable(EventPayload::ToolApprovalRequested {
                            call_id: call.call_id.clone().into(),
                            tool_name: call.name.clone(),
                            prompt: prompt.clone(),
                            rule_key: rule_key.clone(),
                            source: *source,
                            arguments: call.tool_input.clone(),
                        })
                        .await?;
                    sources.push(ResultSource::PendingApproval {
                        rx,
                        prompt: prompt.clone(),
                        rule_key: rule_key.clone(),
                        source: *source,
                    });
                },
                PreparedToolOutcome::Ready => {
                    let executable = call.to_executable();
                    let accesses = call.accesses.clone();
                    let tool_registry = Arc::clone(&self.tool_registry);
                    let ctx = self.make_runtime_context(input.tools, Arc::clone(&input.publisher));
                    let rx = scheduler.submit(accesses, move || async move {
                        execute_tool_call(tool_registry, ctx, executable).await
                    });
                    sources.push(ResultSource::Scheduled(rx));
                },
            }
        }

        for (position, source) in sources.into_iter().enumerate() {
            if self.cancellation_token.is_cancelled() {
                return Err(TurnError::Aborted);
            }

            let mut result = match source {
                ResultSource::Blocked(result) => result,
                ResultSource::DuplicateSameStep => {
                    input
                        .state
                        .tool_deduplicator()
                        .await_same_step_result(&input.prepared[position].call_id)
                        .await
                },
                ResultSource::Scheduled(rx) => {
                    let (_index, result) = scheduler
                        .await_result(rx)
                        .await
                        .map_err(|_| TurnError::Aborted)?;
                    result
                },
                ResultSource::PendingApproval {
                    rx,
                    prompt,
                    rule_key,
                    source,
                } => {
                    let call = &input.prepared[position];
                    let (decision, resolution_detail) =
                        match tokio::time::timeout(Duration::from_secs(APPROVAL_TIMEOUT_SECS), rx)
                            .await
                        {
                            Ok(Ok(decision)) => (decision, None),
                            Ok(Err(_)) => (
                                ApprovalDecision::DenyOnce,
                                Some("approval receiver dropped".into()),
                            ),
                            Err(_) => (
                                ApprovalDecision::DenyOnce,
                                Some(format!("approval timed out after {APPROVAL_TIMEOUT_SECS}s")),
                            ),
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
                        let executable = call.to_executable();
                        let accesses = self
                            .tool_registry
                            .resource_accesses(
                                &call.name,
                                &call.tool_input,
                                Path::new(&self.turn.shared.working_dir),
                            )
                            .unwrap_or_else(|_| vec![ResourceAccess::all()]);
                        let tool_registry = Arc::clone(&self.tool_registry);
                        let ctx =
                            self.make_runtime_context(input.tools, Arc::clone(&input.publisher));
                        let scheduled = scheduler.submit(accesses, move || async move {
                            execute_tool_call(tool_registry, ctx, executable).await
                        });
                        let (_index, result) = scheduler
                            .await_result(scheduled)
                            .await
                            .map_err(|_| TurnError::Aborted)?;
                        result
                    } else {
                        let reason = resolution_detail
                            .map(|detail| {
                                format!("Tool execution denied ({detail}, {source:?}): {prompt}")
                            })
                            .unwrap_or_else(|| {
                                format!("Tool execution denied by user ({source:?}): {prompt}")
                            });
                        ToolResult {
                            call_id: call.call_id.clone(),
                            content: reason.clone(),
                            is_error: true,
                            error: Some(reason),
                            metadata: Default::default(),
                            duration_ms: None,
                        }
                    }
                },
            };

            if is_awaiting_user_input_content(&result.content) {
                result = self
                    .await_tool_ui_response(&input.prepared[position], result)
                    .await?;
            }

            let mut results = HashMap::new();
            results.insert(input.prepared[position].index, result);
            discovered_tools.extend(
                self.commit_tool_results(CommitToolResults {
                    prepared: &input.prepared[position..position + 1],
                    results,
                    state: input.state,
                    publisher: Arc::clone(&input.publisher),
                })
                .await?,
            );
        }

        Ok(discovered_tools)
    }

    async fn await_tool_ui_response(
        &self,
        call: &super::tool_types::PreparedToolCall,
        mut result: ToolResult,
    ) -> Result<ToolResult, TurnError> {
        let (tx, rx) = oneshot::channel();
        self.session
            .runtime()
            .register_pending_tool_ui_response(ToolCallId::from(call.call_id.as_str()), tx);

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
        mut input: CommitToolResults<'_>,
    ) -> Result<Vec<String>, TurnError> {
        let mut pending_results = Vec::with_capacity(input.prepared.len());
        for call in input.prepared {
            let mut result = input
                .results
                .remove(&call.index)
                .unwrap_or_else(|| missing_tool_result(call));

            if matches!(&call.outcome, PreparedToolOutcome::Ready) {
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
                    event_tx: self.turn.shared.turn_event_tx.clone(),
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

            if !matches!(&call.outcome, PreparedToolOutcome::DuplicateSameStep) {
                input
                    .state
                    .tool_deduplicator_mut()
                    .finalize_result(&call.call_id, &result);
            }

            pending_results.push(PendingCommittedToolResult {
                call_id: call.call_id.clone(),
                tool_name: call.name.clone(),
                result,
                arguments: call.tool_input.to_string(),
                arguments_json: call.tool_input.clone(),
            });
        }

        for pending in &mut pending_results {
            // 对于超过 inline 限制的工具结果，先持久化到磁盘并替换为摘要引用，再继续后续处理。
            self.persist_large_tool_result(
                &pending.tool_name,
                &pending.call_id,
                &mut pending.result,
            )
            .await?;
        }
        let model = input.publisher.snapshot_model().await?;
        let committed_tool_result_chars = committed_tool_result_content_len(&model);
        // 当累计工具结果超过消息字符预算时，按体积从大到小持久化，直到总量回到预算内。
        self.enforce_tool_result_message_budget(committed_tool_result_chars, &mut pending_results)
            .await?;

        let mut discovered_tools = Vec::new();
        for pending in pending_results {
            let PendingCommittedToolResult {
                call_id,
                tool_name,
                result,
                arguments,
                arguments_json,
            } = pending;
            discovered_tools.extend(discovered_deferred_tool_names(&result));
            input
                .publisher
                .durable(EventPayload::ToolCallCompleted {
                    call_id: call_id.into(),
                    tool_name,
                    result: result.clone(),
                    arguments,
                    arguments_json: Some(arguments_json),
                })
                .await?;
            input.state.push_tool_result(result);
        }

        Ok(discovered_tools)
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
                "path": reference.path.clone(),
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
        pending_results: &mut [PendingCommittedToolResult],
    ) -> Result<(), TurnError> {
        let mut total: usize = committed_tool_result_chars
            + pending_results
                .iter()
                .map(|pending| pending.result.content.len())
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
                        should_persist_tool_result(&pending.result.content, limit)
                    }) && !pending.result.metadata.contains_key("persistedToolResult")
                        && !is_artifact_read(&pending.result)
                        && !is_persisted_tool_result_summary(&pending.result.content)
                        && !pending
                            .result
                            .metadata
                            .get("path")
                            .and_then(|value| value.as_str())
                            .is_some_and(is_tool_result_artifact_path);
                can_persist.then_some(index)
            })
            .collect();
        candidates.sort_by(|left, right| {
            pending_results[*right]
                .result
                .content
                .len()
                .cmp(&pending_results[*left].result.content.len())
        });

        for index in candidates {
            if total <= MAX_TOOL_RESULTS_PER_MESSAGE_CHARS {
                break;
            }
            let pending = &mut pending_results[index];
            let before = pending.result.content.len();
            self.persist_tool_result(&pending.tool_name, &pending.call_id, &mut pending.result)
                .await?;
            let after = pending.result.content.len();
            total = total.saturating_sub(before).saturating_add(after);
        }

        Ok(())
    }
}

fn is_artifact_read(result: &ToolResult) -> bool {
    result.metadata.get("source").and_then(|v| v.as_str()) == Some("toolResultArtifact")
}

// ─── Tool event & message helpers ────────────────────────────────────────

async fn send_tool_requested(
    publisher: &TurnEvents,
    tc: &PendingToolCall,
    arguments: &serde_json::Value,
) -> Result<(), TurnError> {
    publisher
        .durable(EventPayload::ToolCallRequested {
            call_id: tc.call_id.clone().into(),
            tool_name: tc.name.clone(),
            arguments: arguments.clone(),
        })
        .await
}

fn missing_tool_result(call: &PreparedToolCall) -> ToolResult {
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
