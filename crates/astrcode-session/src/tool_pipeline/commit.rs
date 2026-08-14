use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use astrcode_extension_sdk::extension::{
    PostToolUseResult, internal::runtime_post_tool_use_context,
};

use super::{
    ToolCalls,
    events::{finish_tool_call, missing_tool_outcome, tool_result_for_output},
};
use crate::{
    tool_results::{
        TOOL_RESULT_PREVIEW_CHARS, persisted_tool_result_summary, should_auto_persist_tool_result,
        tool_result_preview,
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
        let post_ctx = runtime_post_tool_use_context(
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
            || !should_auto_persist_tool_result(&result.result)
        {
            return Ok(());
        }
        self.persist_tool_result(tool_name, call_id, result).await
    }

    /// 将工具结果写入 session 存储并替换为摘要引用（含 preview 和 artifact ID）。
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
        result.metadata.insert(
            "artifactId".into(),
            serde_json::json!(&reference.artifact_id),
        );
        result.content = persisted_tool_result_summary(&reference, &preview);
        result.artifact_state = ToolResultArtifactState::Persisted;
        if result.is_error {
            result.error = Some(result.content.clone());
        }
        Ok(())
    }
}
