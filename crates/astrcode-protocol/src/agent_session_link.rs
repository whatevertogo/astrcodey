//! 子 Agent 会话链接线缆 DTO 与增量构造逻辑。

use serde::{Deserialize, Serialize};

pub use crate::wire::AgentSessionStatusDto;
use crate::wire::PhaseDto;

/// 子 Agent 会话链接（HTTP/SSE/JSON-RPC 共用线缆 DTO，camelCase 序列化）。
///
/// `status` 为 `None` 时表示增量 patch 不改动终态（仅更新 phase / currentTool）。
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub phase: Option<PhaseDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_tool: Option<String>,
}

impl AgentSessionLinkDto {
    fn empty_patch(child_session_id: impl AsRef<str>) -> Self {
        Self {
            child_session_id: child_session_id.as_ref().to_owned(),
            tool_call_id: None,
            agent_name: None,
            task: None,
            status: None,
            final_session_id: None,
            summary: None,
            error: None,
            phase: None,
            current_tool: None,
        }
    }

    /// `AgentSessionSpawned` 事件投影。
    pub fn spawned(
        child_session_id: impl AsRef<str>,
        tool_call_id: Option<&str>,
        agent_name: impl AsRef<str>,
        task: impl AsRef<str>,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.map(str::to_owned),
            agent_name: Some(agent_name.as_ref().to_string()),
            task: Some(task.as_ref().to_string()),
            status: Some(AgentSessionStatusDto::Running),
            phase: Some(PhaseDto::Thinking),
            ..Self::empty_patch(child_session_id)
        }
    }

    /// `AgentSessionCompleted` 事件投影。
    pub fn completed(
        child_session_id: impl AsRef<str>,
        final_session_id: impl AsRef<str>,
        summary: impl AsRef<str>,
    ) -> Self {
        Self {
            status: Some(AgentSessionStatusDto::Completed),
            final_session_id: Some(final_session_id.as_ref().to_string()),
            summary: Some(summary.as_ref().to_string()),
            ..Self::empty_patch(child_session_id)
        }
    }

    /// `AgentSessionFailed` 事件投影。
    pub fn failed(
        child_session_id: impl AsRef<str>,
        final_session_id: impl AsRef<str>,
        error: impl AsRef<str>,
    ) -> Self {
        Self {
            status: Some(AgentSessionStatusDto::Failed),
            final_session_id: Some(final_session_id.as_ref().to_string()),
            error: Some(error.as_ref().to_string()),
            ..Self::empty_patch(child_session_id)
        }
    }

    /// 子 session 阶段刷新；省略 status，避免覆盖终态。
    pub fn phase_only(
        child_session_id: impl AsRef<str>,
        phase: PhaseDto,
        current_tool: Option<String>,
    ) -> Self {
        Self {
            phase: Some(phase),
            current_tool,
            ..Self::empty_patch(child_session_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn snapshot_entry(status: AgentSessionStatusDto) -> AgentSessionLinkDto {
        AgentSessionLinkDto {
            child_session_id: "child-1".into(),
            tool_call_id: Some("tool-1".into()),
            agent_name: Some("explorer".into()),
            task: Some("scan repo".into()),
            status: Some(status),
            final_session_id: None,
            summary: None,
            error: None,
            phase: None,
            current_tool: None,
        }
    }

    #[test]
    fn snapshot_entry_includes_status_on_wire() {
        let dto = snapshot_entry(AgentSessionStatusDto::Running);
        let value = serde_json::to_value(&dto).unwrap();
        assert_eq!(value["status"], json!("running"));
        assert_eq!(value["childSessionId"], json!("child-1"));
        assert_eq!(value["agentName"], json!("explorer"));
    }

    #[test]
    fn phase_only_patch_omits_status_on_wire() {
        let dto =
            AgentSessionLinkDto::phase_only("child-1", PhaseDto::CallingTool, Some("read".into()));
        let value = serde_json::to_value(&dto).unwrap();
        assert!(value.get("status").is_none());
        assert_eq!(value["phase"], json!("calling_tool"));
        assert_eq!(value["currentTool"], json!("read"));
    }

    #[test]
    fn spawned_preserves_optional_tool_call_and_running_status() {
        let attributed =
            AgentSessionLinkDto::spawned("child-1", Some("tool-1"), "reviewer", "review diff");
        let unattributed = AgentSessionLinkDto::spawned("child-2", None, "worker", "run task");

        assert_eq!(attributed.tool_call_id.as_deref(), Some("tool-1"));
        assert_eq!(attributed.status, Some(AgentSessionStatusDto::Running));
        assert_eq!(attributed.phase, Some(PhaseDto::Thinking));
        assert!(unattributed.tool_call_id.is_none());
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
        let original = snapshot_entry(AgentSessionStatusDto::Completed);
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains("childSessionId"));
        assert!(!json.contains("child_session_id"));

        let restored: AgentSessionLinkDto = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.child_session_id, original.child_session_id);
        assert_eq!(restored.status, original.status);
    }
}
