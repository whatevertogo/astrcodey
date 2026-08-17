//! 子 Agent 会话链接线缆 DTO:snapshot 全量基线与增量更新载荷。

use serde::{Deserialize, Serialize};

pub use crate::wire::AgentSessionStatusDto;
use crate::wire::PhaseDto;

/// 完整的子 Agent 会话链接，用于 snapshot 基线。
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[cfg_attr(feature = "typescript", ts(optional_fields))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSessionLinkDto {
    pub child_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub agent_name: String,
    pub task: String,
    pub status: AgentSessionStatusDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 子 Agent 会话的合法增量。每个 variant 只携带该事件能够改变的字段。
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[cfg_attr(feature = "typescript", ts(optional_fields))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum AgentSessionUpdateDto {
    Spawned {
        child_session_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_call_id: Option<String>,
        agent_name: String,
        task: String,
    },
    Completed {
        child_session_id: String,
        final_session_id: String,
        summary: String,
    },
    Failed {
        child_session_id: String,
        final_session_id: String,
        error: String,
    },
    Progress {
        child_session_id: String,
        phase: PhaseDto,
        #[serde(skip_serializing_if = "Option::is_none")]
        current_tool: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn full_snapshot_and_typed_updates_keep_distinct_contracts() {
        let snapshot = AgentSessionLinkDto {
            child_session_id: "child-1".into(),
            tool_call_id: Some("tool-1".into()),
            agent_name: "explorer".into(),
            task: "scan repo".into(),
            status: AgentSessionStatusDto::Running,
            final_session_id: None,
            summary: None,
            error: None,
        };
        let value = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(value["status"], json!("running"));
        assert_eq!(value["childSessionId"], json!("child-1"));
        assert_eq!(value["agentName"], json!("explorer"));
        assert!(
            serde_json::from_value::<AgentSessionLinkDto>(json!({
                "childSessionId": "child-1",
                "agentName": "explorer",
                "task": "scan repo",
                "status": "running",
                "phase": "thinking"
            }))
            .is_err()
        );

        let updates = [
            AgentSessionUpdateDto::Spawned {
                child_session_id: "child-1".into(),
                tool_call_id: Some("tool-1".into()),
                agent_name: "explorer".into(),
                task: "scan repo".into(),
            },
            AgentSessionUpdateDto::Progress {
                child_session_id: "child-1".into(),
                phase: PhaseDto::CallingTool,
                current_tool: Some("read".into()),
            },
            AgentSessionUpdateDto::Completed {
                child_session_id: "child-1".into(),
                final_session_id: "child-final".into(),
                summary: "done".into(),
            },
        ];
        let values = updates.map(|update| serde_json::to_value(update).unwrap());
        assert_eq!(values[0]["kind"], json!("spawned"));
        assert_eq!(values[1]["phase"], json!("calling_tool"));
        assert!(values[1].get("status").is_none());
        assert_eq!(values[2]["summary"], json!("done"));
        assert!(
            serde_json::from_value::<AgentSessionUpdateDto>(json!({
                "kind": "progress",
                "childSessionId": "child-1",
                "phase": "thinking",
                "status": "running"
            }))
            .is_err()
        );
    }
}
