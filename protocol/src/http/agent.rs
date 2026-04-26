//! Agent 管理相关 DTO
//!
//! 定义 Agent profile 查询、执行、子执行域（sub-run）状态查询等接口的请求/响应结构。
//! 这些 DTO 用于前端展示和管理 Agent 配置、触发 Agent 执行任务以及监控 sub-run 状态。

pub use astrcode_core::{
    AgentLifecycleStatus as AgentLifecycleDto, AgentProfile as AgentProfileDto,
    AgentTurnOutcome as AgentTurnOutcomeDto, ChildSessionLineageKind as ChildSessionLineageKindDto,
    ChildSessionNotificationKind as ChildSessionNotificationKindDto,
    SubagentContextOverrides as SubagentContextOverridesDto,
};
use serde::{Deserialize, Serialize};

use crate::http::{
    ExecutionControlDto, ResolvedSubagentContextOverridesDto, SubRunResultDto, SubRunStorageModeDto,
};

/// `POST /api/v1/agents/{id}/execute` 请求体。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentExecuteRequestDto {
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<ExecutionControlDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_overrides: Option<SubagentContextOverridesDto>,
}

/// `POST /api/v1/agents/{id}/execute` 响应体。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum AgentExecuteResponseDto {
    Accepted {
        session_id: String,
        turn_id: String,
        agent_id: String,
    },
    Handled {
        session_id: String,
        agent_id: String,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SubRunStatusSourceDto {
    Live,
    Durable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SubRunStatusDto {
    pub sub_run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub source: SubRunStatusSourceDto,
    pub agent_id: String,
    pub agent_profile: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_session_id: Option<String>,
    pub depth: usize,
    // parent_agent_id 表示“这个子会话归哪个父 agent 所有”，它描述的是控制/权限关系
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
    // parent_sub_run_id 表示“这个子执行是从哪个父 sub-run 触发出来的”，它描述的是执行谱系关系
    // WHY:同一个 agent_id 可以经历多个 sub_run_id;UI / history / SSE
    // 过滤需要的是执行树，不是权限树;只靠 parent_agent_id 会把我们拉回“推断谱系”
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_sub_run_id: Option<String>,
    pub storage_mode: SubRunStorageModeDto,
    pub lifecycle: AgentLifecycleDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_outcome: Option<AgentTurnOutcomeDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<SubRunResultDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_overrides: Option<ResolvedSubagentContextOverridesDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_limits: Option<crate::http::ResolvedExecutionLimitsDto>,
}

/// 谱系来源快照 DTO，fork/resume 时记录来源上下文。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LineageSnapshotDto {
    pub source_agent_id: String,
    pub source_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_sub_run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChildAgentRefDto {
    pub agent_id: String,
    pub session_id: String,
    pub sub_run_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_sub_run_id: Option<String>,
    pub lineage_kind: ChildSessionLineageKindDto,
    pub status: AgentLifecycleDto,
    pub open_session_id: String,
}
