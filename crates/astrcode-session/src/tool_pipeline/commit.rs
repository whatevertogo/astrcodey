use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use astrcode_core::tool::ToolResult;
use astrcode_extension_sdk::extension::{PostToolUseContext, PostToolUseResult};

use super::{
    ToolCalls,
    events::{finish_tool_call, missing_tool_outcome, tool_result_for_output},
};
use crate::{
    deferred_tools::discovered_deferred_tool_names,
    llm_request_history::committed_tool_result_content_len,
    tool_results::{
        MAX_TOOL_RESULTS_PER_MESSAGE_CHARS, PERSISTED_TOOL_RESULT_METADATA_KEY,
        TOOL_RESULT_PREVIEW_CHARS, persisted_tool_result_summary, should_auto_persist_tool_result,
        tool_result_preview,
    },
    tool_types::{PreparedToolDisposition, PreparedToolInvocation, ToolExecutionOutcome},
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
    pub async fn commit_tool_outcomes(
        &self,
        prepared: &[PreparedToolInvocation],
        mut outcomes: HashMap<usize, ToolExecutionOutcome>,
        pending_declared: &mut HashSet<String>,
        state: &mut TurnState,
        publisher: Arc<TurnEvents>,
    ) -> Result<Vec<String>, TurnError> {
        let mut pending = Vec::with_capacity(prepared.len());
        for call in prepared {
            let mut outcome = outcomes
                .remove(&call.index)
                .unwrap_or_else(|| missing_tool_outcome(call));
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
                discovered_tools.extend(discovered_deferred_tool_names(result));
            }
            let (arguments, arguments_json) = crate::tool_types::tool_call_completion_arguments(
                item.call.tool_input.clone(),
                item.call.raw_arguments.clone(),
            );
            finish_tool_call(
                &publisher,
                &item.call.call_id,
                item.call.name.clone(),
                &item.outcome,
                arguments,
                arguments_json,
            )
            .await?;
            pending_declared.remove(&item.call.call_id);
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

        let post_ctx = PostToolUseContext {
            session_id: self.turn.shared.session_id.to_string(),
            working_dir: self.turn.shared.working_dir.clone(),
            model: self.turn.shared.model_selection(),
            call_id: call.call_id.clone().into(),
            tool_name: call.name.clone(),
            tool_input: call.tool_input.clone(),
            tool_result: result.clone(),
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
        Ok(())
    }

    /// 检查工具结果是否超过 inline 限制，超限则持久化到磁盘并替换为摘要引用。
    async fn persist_large_tool_result(
        &self,
        tool_name: &str,
        call_id: &str,
        result: &mut ToolResult,
    ) -> Result<(), TurnError> {
        if !should_auto_persist_tool_result(tool_name, result) {
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
        result.metadata.insert(
            PERSISTED_TOOL_RESULT_METADATA_KEY.into(),
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
                should_auto_persist_tool_result(&item.call.name, result).then_some(index)
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
