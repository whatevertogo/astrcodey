use std::{collections::HashMap, path::Path};

use astrcode_core::{
    extension::{PreToolUseContext, PreToolUseResult},
    permission::{ApprovalSource, PermissionContext, PermissionDecision},
    tool::{ExecutionMode, ToolDefinition, ToolResult},
    tool_access::ResourceAccess,
};

use super::{ToolCalls, events::declare_tool_batch};
use crate::{
    deferred_tools::{tool_is_visible, unavailable_tool_guidance},
    early_tool_scheduler::EarlyExecutionEntry,
    tool_deduplicator::{SameStepCheck, ToolCallDeduplicator},
    tool_json_repair::parse_and_repair_json,
    tool_types::{
        DeclaredToolBatch, PreparedToolBatch, PreparedToolInvocation,
        PreparedToolInvocationOutcome, StreamedToolCall,
    },
    turn_context::TurnError,
    turn_publish::TurnEvents,
    turn_stages::TurnState,
};

impl ToolCalls {
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
}
