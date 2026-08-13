//! 父 session 可见的 child-agent 链接与终态投影。

use astrcode_core::{
    event::{DurableEventPayload, StoredEvent},
    types::{SessionId, ToolCallId},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionStatus {
    Running,
    Completed,
    Failed,
}

/// 父会话派生的子 Agent 会话链接。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSessionLinkView {
    pub child_session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
    pub agent_name: String,
    pub task: String,
    pub status: AgentSessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub(crate) fn apply_event(event: &StoredEvent, links: &mut Vec<AgentSessionLinkView>) {
    match &event.payload {
        DurableEventPayload::AgentSessionSpawned {
            child_session_id,
            agent_name,
            task,
            tool_call_id,
            ..
        } => links.push(AgentSessionLinkView {
            child_session_id: child_session_id.clone(),
            tool_call_id: tool_call_id.clone(),
            agent_name: agent_name.clone(),
            task: task.clone(),
            status: AgentSessionStatus::Running,
            final_session_id: None,
            summary: None,
            error: None,
        }),
        DurableEventPayload::AgentSessionCompleted {
            child_session_id,
            final_session_id,
            summary,
        } => {
            if let Some(link) = find_link(links, child_session_id) {
                link.status = AgentSessionStatus::Completed;
                link.final_session_id = Some(final_session_id.clone());
                link.summary = Some(summary.clone());
                link.error = None;
            }
        },
        DurableEventPayload::AgentSessionFailed {
            child_session_id,
            final_session_id,
            error,
        } => {
            if let Some(link) = find_link(links, child_session_id) {
                link.status = AgentSessionStatus::Failed;
                link.final_session_id = Some(final_session_id.clone());
                link.error = Some(error.clone());
                link.summary = None;
            }
        },
        DurableEventPayload::AgentSessionRecycled { child_session_id } => {
            links.retain(|link| link.child_session_id != *child_session_id);
        },
        _ => {},
    }
}

fn find_link<'a>(
    links: &'a mut [AgentSessionLinkView],
    child_session_id: &SessionId,
) -> Option<&'a mut AgentSessionLinkView> {
    links
        .iter_mut()
        .find(|link| link.child_session_id == *child_session_id)
}
