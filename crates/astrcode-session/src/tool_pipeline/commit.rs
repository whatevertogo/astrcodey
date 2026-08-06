use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use astrcode_extension_sdk::extension::{PostToolUseResult, RuntimePostToolUseContext};

use super::{
    ToolCalls,
    events::{finish_tool_call, missing_tool_outcome, tool_result_for_output},
};
use crate::{
    projection_context::committed_tool_result_content_len,
    tool_results::{
        MAX_TOOL_RESULTS_PER_MESSAGE_CHARS, TOOL_RESULT_PREVIEW_CHARS,
        persisted_tool_result_summary, should_auto_persist_tool_result, tool_result_preview,
    },
    tool_types::{
        PreparedToolDisposition, PreparedToolInvocation, ToolExecutionOutcome,
        ToolResultArtifactState, ToolResultCommit,
    },
    turn_context::TurnError,
    turn_publish::TurnEvents,
    turn_stages::TurnState,
};

struct PendingToolCommit<'a> {
    call: &'a PreparedToolInvocation,
    outcome: ToolExecutionOutcome,
}

impl ToolCalls {
    /// 提交工具执行终态。
    ///
    /// 对每个已执行的工具调用依次处理：
    /// 1. 仅对正常完成的工具分发 `PostToolUse`。
    /// 2. 发送唯一的 durable 完成、失败或取消事件。
    /// 3. 将边界映射后的兼容结果写入 turn 输出聚合。
    pub(crate) async fn commit_tool_outcomes(
        &self,
        prepared: &[PreparedToolInvocation],
        mut outcomes: HashMap<usize, ToolExecutionOutcome>,
        uncommitted_calls: &mut HashSet<String>,
        state: &mut TurnState,
        publisher: Arc<TurnEvents>,
    ) -> Result<Vec<String>, TurnError> {
        let mut pending = Vec::with_capacity(prepared.len());
        for call in prepared {
            // 不变式：每个 declared call 在 commit 前必有 outcome（execute 阶段保证）。
            // 缺失说明编排层缺陷——debug 构建直接暴露，release 下兜底为 failed 结果。
            let mut outcome = match outcomes.remove(&call.index) {
                Some(outcome) => outcome,
                None => {
                    debug_assert!(
                        false,
                        "declared tool call `{}` reached commit without an outcome",
                        call.call_id
                    );
                    missing_tool_outcome(call)
                },
            };
            self.validate_discovered_tools(call, &mut outcome);
            self.apply_post_tool_use(call, &mut outcome).await?;

            if !matches!(&call.disposition, PreparedToolDisposition::ReuseSameStep) {
                state
                    .tool_deduplicator_mut()
                    .finalize_outcome(&call.call_id, &outcome);
            }

            pending.push(PendingToolCommit { call, outcome });
        }

        for item in &mut pending {
            // 对于超过 inline 限制的工具结果，先持久化到磁盘并替换为摘要引用，再继续后续处理。
            if let ToolExecutionOutcome::Completed(result) = &mut item.outcome {
                self.persist_large_tool_result(&item.call.name, &item.call.call_id, result)
                    .await?;
            }
        }
        let model = publisher.snapshot_model().await?;
        let committed_tool_result_chars = committed_tool_result_content_len(&model);
        // 当累计工具结果超过消息字符预算时，按体积从大到小持久化，直到总量回到预算内。
        self.enforce_tool_result_message_budget(committed_tool_result_chars, &mut pending)
            .await?;

        let mut discovered_tools = Vec::new();
        for item in pending {
            if let ToolExecutionOutcome::Completed(result) = &item.outcome {
                discovered_tools.extend(result.discovered_tool_names.clone());
            }
            let completion = crate::tool_types::tool_call_completion_arguments(
                item.call.tool_input.clone(),
                item.call.raw_arguments.clone(),
            );
            finish_tool_call(
                &publisher,
                &item.call.call_id,
                item.call.name.clone(),
                &item.outcome,
                completion.arguments,
                completion.arguments_json,
            )
            .await?;
            uncommitted_calls.remove(&item.call.call_id);
            state.record_tool_result(tool_result_for_output(&item.outcome));
        }

        Ok(discovered_tools)
    }

    async fn apply_post_tool_use(
        &self,
        call: &PreparedToolInvocation,
        outcome: &mut ToolExecutionOutcome,
    ) -> Result<(), TurnError> {
        if matches!(call.disposition, PreparedToolDisposition::ReuseSameStep) {
            return Ok(());
        }
        let ToolExecutionOutcome::Completed(result) = outcome else {
            return Ok(());
        };
        if result.is_error && result.error.is_none() {
            result.error = Some(result.content.clone());
        }

        let hook_call = self.turn.shared.hook_call_context();
        let post_ctx = RuntimePostToolUseContext::new(
            hook_call,
            call.call_id.clone().into(),
            call.name.clone(),
            call.tool_input.clone(),
            result.result.clone(),
        );

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
                result.discovered_tool_names.clear();
            },
            PostToolUseResult::Allow => {},
        }
        Ok(())
    }

    fn validate_discovered_tools(
        &self,
        call: &PreparedToolInvocation,
        outcome: &mut ToolExecutionOutcome,
    ) {
        let ToolExecutionOutcome::Completed(commit) = outcome else {
            return;
        };
        if commit.discovered_tool_names.is_empty() {
            return;
        }
        if let Err(error) = self.tool_registry.validate_discovered_tools(
            &call.name,
            call.discovery_gate.as_deref(),
            &commit.discovered_tool_names,
        ) {
            *outcome = ToolExecutionOutcome::failed(error);
        }
    }

    /// 检查工具结果是否超过 inline 限制，超限则持久化到磁盘并替换为摘要引用。
    async fn persist_large_tool_result(
        &self,
        tool_name: &str,
        call_id: &str,
        result: &mut ToolResultCommit,
    ) -> Result<(), TurnError> {
        if result.artifact_state == ToolResultArtifactState::Persisted
            || !should_auto_persist_tool_result(tool_name, &result.result)
        {
            return Ok(());
        }
        self.persist_tool_result(tool_name, call_id, result).await
    }

    /// 将工具结果写入 session 存储并替换为摘要引用（含 preview 和 artifact 路径）。
    async fn persist_tool_result(
        &self,
        tool_name: &str,
        call_id: &str,
        result: &mut ToolResultCommit,
    ) -> Result<(), TurnError> {
        let original_content = result.content.clone();
        let preview = tool_result_preview(&original_content, TOOL_RESULT_PREVIEW_CHARS);
        let reference = self
            .session
            .write_tool_artifact(astrcode_storage::ToolResultArtifactInput {
                call_id: call_id.to_string(),
                tool_name: tool_name.to_string(),
                content: original_content,
            })
            .await?;
        result
            .metadata
            .insert("artifactBytes".into(), serde_json::json!(reference.bytes));
        if let Some(path) = &reference.path {
            result
                .metadata
                .insert("artifactPath".into(), serde_json::json!(path));
        }
        result.content = persisted_tool_result_summary(&reference, &preview);
        result.artifact_state = ToolResultArtifactState::Persisted;
        if result.is_error {
            result.error = Some(result.content.clone());
        }
        Ok(())
    }

    /// 当累计工具结果超过消息字符预算时，按体积从大到小持久化，直到总量回到预算内。
    async fn enforce_tool_result_message_budget(
        &self,
        committed_tool_result_chars: usize,
        pending: &mut [PendingToolCommit<'_>],
    ) -> Result<(), TurnError> {
        let mut total: usize = committed_tool_result_chars
            + pending
                .iter()
                .filter_map(|item| match &item.outcome {
                    ToolExecutionOutcome::Completed(result) => Some(result.content.len()),
                    ToolExecutionOutcome::Failed { .. }
                    | ToolExecutionOutcome::Cancelled { .. } => None,
                })
                .sum::<usize>();
        if total <= MAX_TOOL_RESULTS_PER_MESSAGE_CHARS {
            return Ok(());
        }

        let mut candidates: Vec<usize> = pending
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                let ToolExecutionOutcome::Completed(result) = &item.outcome else {
                    return None;
                };
                (result.artifact_state == ToolResultArtifactState::Inline
                    && should_auto_persist_tool_result(&item.call.name, &result.result))
                .then_some(index)
            })
            .collect();
        candidates.sort_by(|left, right| {
            let content_len = |index: usize| match &pending[index].outcome {
                ToolExecutionOutcome::Completed(result) => result.content.len(),
                ToolExecutionOutcome::Failed { .. } | ToolExecutionOutcome::Cancelled { .. } => 0,
            };
            content_len(*right).cmp(&content_len(*left))
        });

        for index in candidates {
            if total <= MAX_TOOL_RESULTS_PER_MESSAGE_CHARS {
                break;
            }
            let item = &mut pending[index];
            let ToolExecutionOutcome::Completed(result) = &mut item.outcome else {
                continue;
            };
            let before = result.content.len();
            self.persist_tool_result(&item.call.name, &item.call.call_id, result)
                .await?;
            let after = result.content.len();
            total = total.saturating_sub(before).saturating_add(after);
        }

        Ok(())
    }
}
