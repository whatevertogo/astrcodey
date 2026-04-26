//! 协作工具（send/observe/close）共享的结果映射逻辑。
//!
//! 与 `result_mapping` 拆开是因为协作工具的返回类型是 `CollaborationResult`，
//! 其结构与 spawn 的 `SubRunResult` 完全不同：
//! - CollaborationResult 侧重 variant + summary/delegation 的稳定组合
//! - SubRunResult 侧重 status/handoff/artifacts 组合
//!
//! 映射策略：
//! - 协作结果本身已经表示 accepted 的动作结果，因此 `ok` 固定为 `true`
//! - `summary` → output（LLM 可见的文本摘要）
//! - 整个 CollaborationResult 序列化为 metadata（供前端消费）

use astrcode_core::{CollaborationResult, DelegationMetadata, ExecutionResultCommon};
use astrcode_runtime_contract::tool::ToolExecutionResult;
use serde_json::json;

/// 协作工具的错误结果（参数校验失败等）。
///
/// duration_ms = 0 因为错误在到达 executor 之前就被拦截了。
pub(crate) fn collaboration_error_result(
    tool_call_id: String,
    tool_name: &str,
    message: String,
) -> ToolExecutionResult {
    ToolExecutionResult::from_common(
        tool_call_id,
        tool_name,
        false,
        String::new(),
        None,
        ExecutionResultCommon::failure(message, None, 0, false),
    )
}

/// 将 CollaborationResult 映射为 ToolExecutionResult。
///
/// metadata 中序列化了完整的 CollaborationResult，前端据此渲染子 agent 状态。
pub(crate) fn map_collaboration_result(
    tool_call_id: String,
    tool_name: &str,
    result: CollaborationResult,
) -> ToolExecutionResult {
    let output = result.summary().unwrap_or_default().to_string();
    let metadata = Some(match serde_json::to_value(&result) {
        Ok(mut value) => {
            inject_advisory_projection(&mut value, &result);
            value
        },
        Err(serialization_error) => json!({
            "schema": "collaborationResult",
            "accepted": true,
            "kind": result_kind_label(&result),
            "serializationError": serialization_error.to_string(),
        }),
    });

    ToolExecutionResult::from_common(
        tool_call_id,
        tool_name,
        true,
        output,
        result.continuation().cloned(),
        ExecutionResultCommon {
            error: None,
            metadata,
            duration_ms: 0,
            truncated: false,
        },
    )
}

fn result_kind_label(result: &CollaborationResult) -> &'static str {
    match result {
        CollaborationResult::Sent { .. } => "sent",
        CollaborationResult::Observed { .. } => "observed",
        CollaborationResult::Closed { .. } => "closed",
    }
}

fn inject_advisory_projection(metadata: &mut serde_json::Value, result: &CollaborationResult) {
    let Some(object) = metadata.as_object_mut() else {
        return;
    };
    if let Some(advisory) = build_advisory_projection(result) {
        object.insert("advisory".to_string(), advisory);
    }
}

fn build_advisory_projection(result: &CollaborationResult) -> Option<serde_json::Value> {
    let delegation = result.delegation();
    let branch = delegation.map(branch_advisory);
    branch.as_ref()?;

    Some(json!({
        "branch": branch,
    }))
}

fn branch_advisory(metadata: &DelegationMetadata) -> serde_json::Value {
    json!({
        "responsibilityBranch": metadata.responsibility_summary,
        "reuseScopeSummary": metadata.reuse_scope_summary,
        "sameResponsibilityAction": "send",
        "differentResponsibilityAction": "close_or_respawn",
        "broaderToolsAction": "close_or_respawn",
    })
}
