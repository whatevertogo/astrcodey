//! 子 Agent 会话链接：唯一线缆 DTO 与集中构造逻辑。

use astrcode_core::{
    event::Phase,
    storage::{AgentSessionLinkView, AgentSessionStatus},
};
use serde::{Deserialize, Serialize};

/// 子 Agent 会话的运行状态（HTTP/SSE 与进程内通知共用）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionStatusDto {
    #[default]
    Running,
    Completed,
    Failed,
}

impl From<AgentSessionStatus> for AgentSessionStatusDto {
    fn from(status: AgentSessionStatus) -> Self {
        match status {
            AgentSessionStatus::Running => Self::Running,
            AgentSessionStatus::Completed => Self::Completed,
            AgentSessionStatus::Failed => Self::Failed,
        }
    }
}

/// 子 Agent 会话链接（HTTP/SSE 与进程内通知共用的线缆 DTO，camelCase 序列化）。
///
/// `status` 为 `None` 时表示增量 patch 不改动终态（仅更新 phase / currentTool）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionLinkDto {
    pub child_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<AgentSessionStatusDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_tool: Option<String>,
}

impl AgentSessionLinkDto {
    /// 从 storage 读模型投影全量 snapshot 条目（始终携带 status）。
    pub fn from_view(link: &AgentSessionLinkView) -> Self {
        Self {
            child_session_id: link.child_session_id.to_string(),
            tool_call_id: link.tool_call_id.as_ref().map(ToString::to_string),
            agent_name: Some(link.agent_name.clone()),
            task: Some(link.task.clone()),
            status: Some(link.status.into()),
            final_session_id: link.final_session_id.as_ref().map(ToString::to_string),
            summary: link.summary.clone(),
            error: link.error.clone(),
            phase: link.phase,
            current_tool: link.current_tool.clone(),
        }
    }

    /// `AgentSessionSpawned` 事件投影。
    pub fn spawned(
        child_session_id: impl AsRef<str>,
        tool_call_id: impl AsRef<str>,
        agent_name: impl AsRef<str>,
        task: impl AsRef<str>,
    ) -> Self {
        Self {
            child_session_id: child_session_id.as_ref().to_string(),
            tool_call_id: Some(tool_call_id.as_ref().to_string()),
            agent_name: Some(agent_name.as_ref().to_string()),
            task: Some(task.as_ref().to_string()),
            status: Some(AgentSessionStatusDto::Running),
            phase: Some(Phase::Thinking),
            ..Default::default()
        }
    }

    /// `AgentSessionCompleted` 事件投影。
    pub fn completed(
        child_session_id: impl AsRef<str>,
        final_session_id: impl AsRef<str>,
        summary: impl AsRef<str>,
    ) -> Self {
        Self {
            child_session_id: child_session_id.as_ref().to_string(),
            status: Some(AgentSessionStatusDto::Completed),
            final_session_id: Some(final_session_id.as_ref().to_string()),
            summary: Some(summary.as_ref().to_string()),
            ..Default::default()
        }
    }

    /// `AgentSessionFailed` 事件投影。
    pub fn failed(
        child_session_id: impl AsRef<str>,
        final_session_id: impl AsRef<str>,
        error: impl AsRef<str>,
    ) -> Self {
        Self {
            child_session_id: child_session_id.as_ref().to_string(),
            status: Some(AgentSessionStatusDto::Failed),
            final_session_id: Some(final_session_id.as_ref().to_string()),
            error: Some(error.as_ref().to_string()),
            ..Default::default()
        }
    }

    /// 子 session 阶段刷新；省略 status，避免覆盖终态。
    pub fn phase_only(
        child_session_id: impl AsRef<str>,
        phase: Phase,
        current_tool: Option<String>,
    ) -> Self {
        Self {
            child_session_id: child_session_id.as_ref().to_string(),
            phase: Some(phase),
            current_tool,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use astrcode_core::{
        storage::{AgentSessionLinkView, AgentSessionStatus},
        types::{SessionId, ToolCallId},
    };
    use serde_json::json;

    use super::*;

    fn sample_view(status: AgentSessionStatus) -> AgentSessionLinkView {
        AgentSessionLinkView {
            child_session_id: SessionId::from("child-1"),
            tool_call_id: Some(ToolCallId::from("tool-1")),
            agent_name: "explorer".into(),
            task: "scan repo".into(),
            status,
            final_session_id: None,
            summary: None,
            error: None,
            phase: None,
            current_tool: None,
        }
    }

    #[test]
    fn from_view_always_includes_status_on_wire() {
        let dto = AgentSessionLinkDto::from_view(&sample_view(AgentSessionStatus::Running));
        let value = serde_json::to_value(&dto).unwrap();
        assert_eq!(value["status"], json!("running"));
        assert_eq!(value["childSessionId"], json!("child-1"));
        assert_eq!(value["agentName"], json!("explorer"));
    }

    #[test]
    fn phase_only_patch_omits_status_on_wire() {
        let dto =
            AgentSessionLinkDto::phase_only("child-1", Phase::CallingTool, Some("read".into()));
        let value = serde_json::to_value(&dto).unwrap();
        assert!(value.get("status").is_none());
        assert_eq!(value["phase"], json!("calling_tool"));
        assert_eq!(value["currentTool"], json!("read"));
    }

    #[test]
    fn spawned_includes_running_status() {
        let dto = AgentSessionLinkDto::spawned("child-1", "tool-1", "reviewer", "review diff");
        assert_eq!(dto.status, Some(AgentSessionStatusDto::Running));
        assert_eq!(dto.phase, Some(Phase::Thinking));
    }

    #[test]
    fn terminal_outcomes_set_status_and_payload() {
        let completed = AgentSessionLinkDto::completed("child-1", "child-1", "done");
        assert_eq!(completed.status, Some(AgentSessionStatusDto::Completed));
        assert_eq!(completed.summary.as_deref(), Some("done"));
        assert!(completed.error.is_none());

        let failed = AgentSessionLinkDto::failed("child-1", "child-1", "timeout");
        assert_eq!(failed.status, Some(AgentSessionStatusDto::Failed));
        assert_eq!(failed.error.as_deref(), Some("timeout"));
        assert!(failed.summary.is_none());
    }

    #[test]
    fn wire_roundtrip_camel_case() {
        let original = AgentSessionLinkDto::from_view(&sample_view(AgentSessionStatus::Completed));
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("childSessionId"));
        assert!(!json.contains("child_session_id"));

        let restored: AgentSessionLinkDto = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.child_session_id, original.child_session_id);
        assert_eq!(restored.status, original.status);
    }
}
