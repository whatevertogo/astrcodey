//! `spawn` 的结果映射逻辑。
//!
//! 将 runtime 返回的 `SubRunResult` 映射为统一的 `ToolExecutionResult`，
//! 并从 handoff artifacts 中提取 `ChildAgentRef` 注入 typed 字段，
//! 使 LLM 后续协作工具（send/observe/close）可以直接复用同一 agentId。

use astrcode_core::{
    ChildAgentRef, ChildSessionLineageKind, ExecutionContinuation, ExecutionResultCommon,
    SubRunResult, SubRunStatus,
};
use astrcode_runtime_contract::tool::ToolExecutionResult;
use serde_json::{Value, json};

const TOOL_NAME: &str = "spawn";
const SUBRUN_RESULT_SCHEMA: &str = "subRunResult";

/// 参数校验失败的快捷构造。
pub(crate) fn invalid_params_result(tool_call_id: String, message: String) -> ToolExecutionResult {
    ToolExecutionResult::from_common(
        tool_call_id,
        TOOL_NAME,
        false,
        String::new(),
        None,
        ExecutionResultCommon::failure(message, None, 0, false),
    )
}

/// 将 SubRunResult 映射为 LLM 可见的 ToolExecutionResult。
///
/// 关键职责：
/// 1. 从 handoff.artifacts 提取 ChildAgentRef，注入 metadata.agentRef
/// 2. 注入 openSessionId 供前端直接打开子会话视图
/// 3. 根据 lifecycle + last_turn_outcome 决定 ok/error/output 的组合方式
pub(crate) fn map_subrun_result(tool_call_id: String, result: SubRunResult) -> ToolExecutionResult {
    let continuation = extract_child_ref(&result).map(ExecutionContinuation::child_agent);
    let error = result
        .failure()
        .map(|failure| failure.technical_message.clone());
    let output = tool_output_for_result(&result);
    let metadata = subrun_metadata(&result);

    ToolExecutionResult::from_common(
        tool_call_id,
        TOOL_NAME,
        !is_failed_outcome(&result),
        output,
        continuation,
        ExecutionResultCommon {
            error,
            metadata: Some(metadata),
            duration_ms: 0,
            truncated: false,
        },
    )
}

/// 判断子运行是否因失败结束。
///
/// 旧逻辑直接匹配 AgentStatus::Failed；拆分后，"失败"由 Idle + Failed outcome 表达。
/// Running 状态说明子 agent 仍在后台执行，不是失败。
fn is_failed_outcome(result: &SubRunResult) -> bool {
    matches!(result.status(), SubRunStatus::Failed)
}

/// 组装 metadata：schema + outcome + handoff + failure + result。
///
/// child/session 真相不再写入 metadata，而是通过 typed continuation 暴露。
fn subrun_metadata(result: &SubRunResult) -> Value {
    json!({
        "schema": SUBRUN_RESULT_SCHEMA,
        "outcome": result.status().label(),
        "handoff": result.handoff(),
        "failure": result.failure(),
        "result": result,
    })
}

/// 从 handoff artifacts 中提取 ChildAgentRef。
///
/// artifact kinds 对应 runtime 层写入的约定：
/// - "subRun": 子运行 ID
/// - "agent": 子 agent 的稳定 ID
/// - "parentSession": 父会话 ID
/// - "session": 子会话 ID（即 openSessionId）
/// - "parentAgent": 父 agent ID（可选）
///
/// 任一必需 artifact 缺失则返回 None——说明 runtime 未正确写入 handoff，
/// 这种情况下后续协作工具会因找不到 agent 而报错，属于可观测的 bug。
fn extract_child_ref(result: &SubRunResult) -> Option<ChildAgentRef> {
    let handoff = result.handoff()?;
    let sub_run_id = artifact_id(&handoff.artifacts, "subRun")?;
    let agent_id = artifact_id(&handoff.artifacts, "agent")?;
    let session_id = artifact_id(&handoff.artifacts, "parentSession")?;
    let open_session_id = artifact_id(&handoff.artifacts, "session")?;
    let parent_agent_id = artifact_id(&handoff.artifacts, "parentAgent");

    Some(ChildAgentRef {
        identity: astrcode_core::ChildExecutionIdentity {
            agent_id: agent_id.into(),
            session_id: session_id.into(),
            sub_run_id: sub_run_id.into(),
        },
        parent: astrcode_core::ParentExecutionRef {
            parent_agent_id: parent_agent_id.map(Into::into),
            parent_sub_run_id: artifact_id(&handoff.artifacts, "parentSubRun").map(Into::into),
        },
        lineage_kind: ChildSessionLineageKind::Spawn,
        status: result.status().lifecycle(),
        open_session_id: open_session_id.into(),
    })
}

/// 在 artifact 列表中按 kind 查找第一个匹配项的 id。
fn artifact_id(artifacts: &[astrcode_core::ArtifactRef], kind: &str) -> Option<String> {
    artifacts
        .iter()
        .find(|artifact| artifact.kind == kind)
        .map(|artifact| artifact.id.clone())
}

/// 生成 LLM 在 tool result 中看到的文本输出。
///
/// - 失败（Idle + Failed outcome）：展示 failure.display_message（面向用户的错误描述）
/// - 其他：展示 handoff.delivery.message（子 agent 返回的 typed upward delivery）
fn tool_output_for_result(result: &SubRunResult) -> String {
    if is_failed_outcome(result) {
        result
            .failure()
            .map(|failure| failure.display_message.clone())
            .unwrap_or_else(|| "子 Agent 执行失败。".to_string())
    } else {
        result
            .handoff()
            .and_then(|handoff| handoff.delivery.as_ref())
            .map(|delivery| delivery.payload.message().to_string())
            .unwrap_or_else(|| "子 Agent 未返回交付。".to_string())
    }
}
