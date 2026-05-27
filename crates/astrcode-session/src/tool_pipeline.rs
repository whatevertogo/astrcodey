//! Tool execution pipeline — preprocessing, parallel/sequential scheduling,
//! result commit, and persistence.

use std::{collections::BTreeMap, sync::Arc, time::Instant};

use astrcode_core::{
    event::EventPayload,
    extension::{PostToolUseContext, PostToolUseResult, PreToolUseContext, PreToolUseResult},
    storage::ToolResultArtifactReader,
    tool::{ExecutionMode, ToolDefinition, ToolResult},
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_tools::registry::ToolRegistry;
use tokio::task::JoinSet;

use super::{
    deferred_tools::{discovered_deferred_tool_names, tool_is_visible},
    tool_exec::{ToolCallRuntimeContext, ToolRuntimeCapabilities, execute_tool_call},
    tool_json_repair::parse_and_repair_json,
    tool_types::{
        CommitToolResults, ExecutableToolCall, ExecuteToolCalls, PendingCommittedToolResult,
        PendingToolCall, PreparedToolCall, PreparedToolOutcome, ToolExecutionStep,
    },
    turn_context::{SharedTurnContext, TurnError},
    turn_publish::TurnPublisher,
};
use crate::{
    llm_request_history::committed_tool_result_content_len,
    session::Session,
    tool_results::{
        MAX_TOOL_RESULTS_PER_MESSAGE_CHARS, TOOL_RESULT_PREVIEW_CHARS,
        persisted_tool_result_summary, should_persist_tool_result, tool_result_inline_limit,
        tool_result_preview,
    },
};

pub struct ToolPipeline {
    shared: SharedTurnContext,
    tool_registry: Arc<ToolRegistry>,
    extension_runner: Arc<ExtensionRunner>,
    session: Arc<Session>,
    tool_runtime_capabilities: ToolRuntimeCapabilities,
}

impl ToolPipeline {
    pub fn new(
        shared: SharedTurnContext,
        tool_registry: Arc<ToolRegistry>,
        extension_runner: Arc<ExtensionRunner>,
        session: Arc<Session>,
        tool_runtime_capabilities: ToolRuntimeCapabilities,
    ) -> Self {
        Self {
            shared,
            tool_registry,
            extension_runner,
            session,
            tool_runtime_capabilities,
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

    /// 构建工具调用的运行时上下文。
    fn make_runtime_context(
        &self,
        tools: &[ToolDefinition],
        publisher: Arc<TurnPublisher>,
    ) -> ToolCallRuntimeContext {
        ToolCallRuntimeContext {
            session_id: self.shared.session_id.clone(),
            working_dir: self.shared.working_dir.clone(),
            tools: tools.to_vec(),
            tool_result_reader: Some(Arc::clone(&self.session) as Arc<dyn ToolResultArtifactReader>),
            publisher,
            capabilities: self.tool_runtime_capabilities.clone(),
        }
    }

    /// 预处理工具调用列表。
    ///
    /// 对每个待执行的工具调用依次执行：
    /// 1. 解析 JSON 参数（解析失败时尝试修复，仍失败则使用空对象并记录警告）。
    /// 2. 检查工具白名单，不在白名单中的工具直接标记为 `Blocked`。
    /// 3. 分发 `PreToolUse` 扩展钩子，允许扩展修改输入或阻止执行。
    /// 4. 根据工具注册表确定执行模式（并行 / 串行）。
    pub async fn prepare_tool_calls(
        &self,
        tool_calls: &[PendingToolCall],
        tools: &[ToolDefinition],
        publisher: &TurnPublisher,
    ) -> Result<Vec<PreparedToolCall>, TurnError> {
        let mut prepared = Vec::with_capacity(tool_calls.len());

        for (index, tc) in tool_calls.iter().enumerate() {
            let args: serde_json::Value = parse_and_repair_json(&tc.arguments, &tc.name);

            if !tool_is_visible(tools, &tc.name) {
                let blocked_result = ToolResult {
                    call_id: tc.call_id.clone(),
                    content: format!(
                        "Tool '{}' has not been loaded for this request. Use `tool_search_tool` \
                         to fetch its schema before calling it.",
                        tc.name
                    ),
                    is_error: true,
                    error: Some(format!("tool '{}' is not loaded", tc.name)),
                    metadata: Default::default(),
                    duration_ms: None,
                };
                send_tool_requested(publisher, tc, &args).await?;
                prepared.push(PreparedToolCall {
                    index,
                    call_id: tc.call_id.clone(),
                    name: tc.name.clone(),
                    tool_input: args,
                    mode: ExecutionMode::Sequential,
                    outcome: PreparedToolOutcome::Blocked(blocked_result),
                });
                continue;
            }

            let pre_ctx = PreToolUseContext {
                session_id: self.shared.session_id.to_string(),
                working_dir: self.shared.working_dir.clone(),
                model: self.shared.model_selection(),
                tool_name: tc.name.clone(),
                tool_input: args.clone(),
                available_tools: tools.to_vec(),
                event_tx: self.shared.turn_event_tx.clone(),
                extension_event_sink: None,
                session_store_dir: self.shared.session_store_dir.clone(),
            };

            let pre_hook_result = self.extension_runner.emit_pre_tool_use(pre_ctx).await?;

            let (tool_input, outcome) = match pre_hook_result {
                PreToolUseResult::ModifyInput { tool_input } => {
                    (tool_input, PreparedToolOutcome::Ready)
                },
                PreToolUseResult::Block { reason } => {
                    let outcome = PreparedToolOutcome::Blocked(ToolResult {
                        call_id: tc.call_id.clone(),
                        content: format!("Tool execution blocked by hook: {reason}"),
                        is_error: true,
                        error: Some(reason),
                        metadata: Default::default(),
                        duration_ms: None,
                    });
                    (args, outcome)
                },
                PreToolUseResult::Allow => (args, PreparedToolOutcome::Ready),
            };

            send_tool_requested(publisher, tc, &tool_input).await?;

            let mode = match &outcome {
                PreparedToolOutcome::Ready => self.tool_registry.execution_mode(&tc.name),
                PreparedToolOutcome::Blocked(_) => ExecutionMode::Sequential,
            };

            prepared.push(PreparedToolCall {
                index,
                call_id: tc.call_id.clone(),
                name: tc.name.clone(),
                tool_input,
                mode,
                outcome,
            });
        }

        Ok(prepared)
    }

    /// 执行已预处理的工具调用。
    ///
    /// 按顺序遍历预处理结果，根据执行模式决定调度方式：
    /// - **Blocked**：先提交已完成的并行批次，再提交预处理阶段的阻止结果。
    /// - **Parallel**：加入当前并行批次，由 `flush_parallel_batch` 统一调度。
    /// - **Sequential**：先提交当前并行批次，再单独执行并提交当前调用。
    pub async fn execute_and_commit(
        &self,
        mut input: ExecuteToolCalls<'_>,
    ) -> Result<Vec<String>, TurnError> {
        let mut discovered_tools = Vec::new();
        let mut parallel_batch = Vec::new();
        let mut parallel_batch_start = None;

        for position in 0..input.prepared.len() {
            let step = {
                let call = &input.prepared[position];
                match &call.outcome {
                    PreparedToolOutcome::Blocked(result) => {
                        ToolExecutionStep::Blocked(result.clone())
                    },
                    PreparedToolOutcome::Ready if call.mode == ExecutionMode::Parallel => {
                        ToolExecutionStep::Parallel(call.to_executable())
                    },
                    PreparedToolOutcome::Ready => {
                        ToolExecutionStep::Sequential(call.to_executable())
                    },
                }
            };

            match step {
                ToolExecutionStep::Blocked(result) => {
                    discovered_tools.extend(
                        self.flush_and_commit_parallel_batch(
                            &mut parallel_batch,
                            &mut parallel_batch_start,
                            &mut input,
                        )
                        .await?,
                    );
                    discovered_tools.extend(
                        self.commit_single_tool_result(&mut input, position, result)
                            .await?,
                    );
                },
                ToolExecutionStep::Parallel(executable) => {
                    if parallel_batch_start.is_none() {
                        parallel_batch_start = Some(position);
                    }
                    parallel_batch.push(executable);
                },
                ToolExecutionStep::Sequential(executable) => {
                    discovered_tools.extend(
                        self.flush_and_commit_parallel_batch(
                            &mut parallel_batch,
                            &mut parallel_batch_start,
                            &mut input,
                        )
                        .await?,
                    );
                    let publisher = Arc::clone(&input.publisher);
                    let (index, result) = execute_tool_call(
                        Arc::clone(&self.tool_registry),
                        self.make_runtime_context(input.tools, Arc::clone(&publisher)),
                        executable,
                    )
                    .await;
                    let mut results = BTreeMap::new();
                    results.insert(index, result);
                    discovered_tools.extend(
                        self.commit_tool_results(CommitToolResults {
                            prepared: &input.prepared[position..position + 1],
                            results,
                            state: input.state,
                            publisher,
                        })
                        .await?,
                    );
                },
            }
        }

        discovered_tools.extend(
            self.flush_and_commit_parallel_batch(
                &mut parallel_batch,
                &mut parallel_batch_start,
                &mut input,
            )
            .await?,
        );

        Ok(discovered_tools)
    }

    async fn flush_and_commit_parallel_batch(
        &self,
        parallel_batch: &mut Vec<ExecutableToolCall>,
        parallel_batch_start: &mut Option<usize>,
        input: &mut ExecuteToolCalls<'_>,
    ) -> Result<Vec<String>, TurnError> {
        let Some(batch_start) = parallel_batch_start.take() else {
            return Ok(Vec::new());
        };
        let batch_len = parallel_batch.len();
        let batch_end = batch_start + batch_len;
        let mut results = BTreeMap::new();

        self.flush_parallel_batch(
            parallel_batch,
            input.tools,
            Arc::clone(&input.publisher),
            &mut results,
        )
        .await?;

        self.commit_tool_results(CommitToolResults {
            prepared: &input.prepared[batch_start..batch_end],
            results,
            state: input.state,
            publisher: Arc::clone(&input.publisher),
        })
        .await
    }

    /// 将单个工具结果（通常是 Blocked 或 Sequential 调用）包装后委托给 `commit_tool_results`。
    async fn commit_single_tool_result(
        &self,
        input: &mut ExecuteToolCalls<'_>,
        position: usize,
        result: ToolResult,
    ) -> Result<Vec<String>, TurnError> {
        let mut results = BTreeMap::new();
        results.insert(input.prepared[position].index, result);
        self.commit_tool_results(CommitToolResults {
            prepared: &input.prepared[position..position + 1],
            results,
            state: input.state,
            publisher: Arc::clone(&input.publisher),
        })
        .await
    }

    /// 刷新并行工具调用批次。
    ///
    /// 使用 `JoinSet` 同时启动最多 `agent.tool_max_parallel_calls` 个工具调用任务，
    /// 每当一个任务完成后立即补充下一个待执行调用，保持并发水位不变。
    async fn flush_parallel_batch(
        &self,
        batch: &mut Vec<ExecutableToolCall>,
        tools: &[ToolDefinition],
        publisher: Arc<TurnPublisher>,
        results: &mut BTreeMap<usize, ToolResult>,
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
        let batch_len = batch.len();
        let batch_started_at = Instant::now();
        tracing::debug!(batch_len, max_parallel, "flushing parallel tool batch");

        let mut pending = std::mem::take(batch).into_iter();
        let mut join_set = JoinSet::new();

        for _ in 0..max_parallel {
            let Some(call) = pending.next() else { break };
            self.spawn_tool_call(&mut join_set, call, tools, Arc::clone(&publisher));
        }

        while let Some(joined) = join_set.join_next().await {
            let (index, result) =
                joined.map_err(|err| TurnError::ToolTaskJoinFailed(err.to_string()))?;
            results.insert(index, result);

            if let Some(call) = pending.next() {
                self.spawn_tool_call(&mut join_set, call, tools, Arc::clone(&publisher));
            }
        }

        tracing::debug!(
            batch_len,
            duration_ms = batch_started_at.elapsed().as_millis() as u64,
            "parallel tool batch flushed"
        );
        Ok(())
    }

    /// 将单个工具调用封装为异步任务并加入 `JoinSet`。
    fn spawn_tool_call(
        &self,
        join_set: &mut JoinSet<(usize, ToolResult)>,
        call: ExecutableToolCall,
        tools: &[ToolDefinition],
        publisher: Arc<TurnPublisher>,
    ) {
        let tool_registry = Arc::clone(&self.tool_registry);
        let ctx = self.make_runtime_context(tools, publisher);

        join_set.spawn(async move { execute_tool_call(tool_registry, ctx, call).await });
    }

    /// 提交工具执行结果。
    ///
    /// 对每个已执行的工具调用依次处理：
    /// 1. 分发 `PostToolUse` 扩展钩子，允许扩展修改结果内容或阻止。
    /// 2. 通过 `TurnPublisher` 发送 durable `ToolCallCompleted`。
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
                    session_id: self.shared.session_id.to_string(),
                    working_dir: self.shared.working_dir.clone(),
                    model: self.shared.model_selection(),
                    tool_name: call.name.clone(),
                    tool_input: call.tool_input.clone(),
                    tool_result: result.clone(),
                    is_error: result.is_error,
                    event_tx: self.shared.turn_event_tx.clone(),
                    extension_event_sink: None,
                    session_store_dir: self.shared.session_store_dir.clone(),
                };

                match self.extension_runner.emit_post_tool_use(post_ctx).await? {
                    PostToolUseResult::ModifyResult { content } => {
                        result.content = content;
                        if result.is_error {
                            result.error = Some(result.content.clone());
                        }
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
                        session_id: self.shared.session_id.to_string(),
                        working_dir: self.shared.working_dir.clone(),
                        model: self.shared.model_selection(),
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

            pending_results.push(PendingCommittedToolResult {
                call_id: call.call_id.clone(),
                tool_name: call.name.clone(),
                result,
                arguments: call.tool_input.to_string(),
                arguments_json: Some(call.tool_input.clone()),
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
                    arguments_json,
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
        let original_content = result.content.clone();
        let preview = tool_result_preview(&original_content, TOOL_RESULT_PREVIEW_CHARS);
        let reference = self
            .session
            .write_tool_artifact(astrcode_core::storage::ToolResultArtifactInput {
                call_id: call_id.to_string(),
                tool_name: tool_name.to_string(),
                content: original_content,
            })
            .await
            .map_err(|error| TurnError::PersistToolResultFailed(error.to_string()))?;
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
                        && !is_artifact_read(&pending.result);
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
    publisher: &TurnPublisher,
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

#[cfg(test)]
mod tests {
    use astrcode_core::tool::ExecutionMode;

    use super::*;
    use crate::tool_types::{PreparedToolCall, PreparedToolOutcome};

    fn execution_plan(prepared: &[PreparedToolCall]) -> Vec<&'static str> {
        prepared
            .iter()
            .map(|call| match &call.outcome {
                PreparedToolOutcome::Blocked(_) => "blocked",
                PreparedToolOutcome::Ready if call.mode == ExecutionMode::Parallel => "parallel",
                PreparedToolOutcome::Ready => "sequential",
            })
            .collect()
    }

    fn sample_call(index: usize, mode: ExecutionMode, blocked: bool) -> PreparedToolCall {
        PreparedToolCall {
            index,
            call_id: format!("call-{index}"),
            name: "tool".into(),
            tool_input: serde_json::json!({}),
            mode,
            outcome: if blocked {
                PreparedToolOutcome::Blocked(ToolResult {
                    call_id: format!("call-{index}"),
                    content: "blocked".into(),
                    is_error: true,
                    error: Some("blocked".into()),
                    metadata: Default::default(),
                    duration_ms: None,
                })
            } else {
                PreparedToolOutcome::Ready
            },
        }
    }

    #[test]
    fn execution_plan_preserves_parallel_sequential_blocked_order() {
        let prepared = vec![
            sample_call(0, ExecutionMode::Parallel, false),
            sample_call(1, ExecutionMode::Parallel, false),
            sample_call(2, ExecutionMode::Sequential, false),
            sample_call(3, ExecutionMode::Parallel, true),
            sample_call(4, ExecutionMode::Sequential, false),
        ];
        assert_eq!(
            execution_plan(&prepared),
            vec![
                "parallel",
                "parallel",
                "sequential",
                "blocked",
                "sequential"
            ]
        );
    }
}
