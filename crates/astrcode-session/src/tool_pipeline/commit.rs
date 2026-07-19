use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use astrcode_core::{
    extension::{AfterToolResult, PostToolUseContext, PostToolUseResult},
    tool::ToolResult,
};

use super::{
    ToolCalls,
    events::{complete_tool_call, missing_tool_result},
};
use crate::{
    deferred_tools::discovered_deferred_tool_names,
    llm_request_history::committed_tool_result_content_len,
    tool_results::{
        MAX_TOOL_RESULTS_PER_MESSAGE_CHARS, PERSISTED_TOOL_RESULT_METADATA_KEY,
        TOOL_RESULT_PREVIEW_CHARS, persisted_tool_result_summary, should_auto_persist_tool_result,
        tool_result_preview,
    },
    tool_types::{CommittedToolResults, PreparedToolInvocation, PreparedToolInvocationOutcome},
    turn_context::TurnError,
    turn_publish::TurnEvents,
    turn_stages::TurnState,
};

impl ToolCalls {
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
            .write_tool_artifact(astrcode_core::storage::ToolResultArtifactInput {
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
                should_auto_persist_tool_result(&pending.tool_name, &pending.tool_result)
                    .then_some(index)
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
