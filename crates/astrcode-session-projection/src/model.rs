//! Session read model 的根组合、身份、统计与跨子投影查询。

use astrcode_core::{
    event::{ParentSessionRef, Phase, SessionStarted},
    tool::SessionToolSelection,
    types::{Cursor, SessionId},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    AgentSessionLinkView, SessionExecutionState, SessionModelContext, SessionPresentation,
    SessionSystemPrompt, UnansweredToolCall,
};

/// 创建 fork session 时记录的来源位置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForkSourceRef {
    pub session_id: SessionId,
    pub cursor: Cursor,
}

/// 会话事件流的内部读模型。
///
/// 这是 storage/domain 边界类型，不是 wire DTO。它只能由事件日志重建，并由上层映射
/// 到具体传输协议。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionReadModel {
    pub identity: SessionIdentity,
    pub stats: SessionEventStats,
    pub system_prompt: SessionSystemPrompt,
    pub model_context: SessionModelContext,
    pub presentation: SessionPresentation,
    pub execution: SessionExecutionState,
    pub agent_sessions: Vec<AgentSessionLinkView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionIdentity {
    pub session_id: SessionId,
    pub working_dir: String,
    pub model_id: String,
    pub parent: Option<ParentSessionRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<ForkSourceRef>,
    pub tool_selection: SessionToolSelection,
    pub source_extension: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionEventStats {
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_seq: u64,
    pub event_count: usize,
}

impl SessionReadModel {
    pub(super) fn from_started(
        session_id: SessionId,
        started: &SessionStarted,
        timestamp: DateTime<Utc>,
    ) -> Self {
        Self {
            identity: SessionIdentity {
                session_id,
                working_dir: started.working_dir.clone(),
                model_id: started.model_id.clone(),
                parent: started.parent.clone(),
                forked_from: None,
                tool_selection: started.tool_selection.clone(),
                source_extension: started.source_extension.clone(),
            },
            stats: SessionEventStats {
                created_at: timestamp,
                updated_at: timestamp,
                last_seq: 0,
                event_count: 1,
            },
            system_prompt: SessionSystemPrompt {
                text: started.initial_system_prompt.text.clone(),
                extra: started.initial_system_prompt.extra_system_prompt.clone(),
                fingerprint: started.initial_system_prompt.fingerprint.clone(),
                source: started.initial_system_prompt.source,
            },
            model_context: SessionModelContext::default(),
            presentation: SessionPresentation::default(),
            execution: SessionExecutionState::default(),
            agent_sessions: Vec::new(),
        }
    }

    pub fn has_messages(&self) -> bool {
        !self.model_context.messages.is_empty()
    }

    pub fn cursor(&self) -> Cursor {
        self.stats.last_seq.to_string()
    }

    pub fn first_user_message(&self) -> Option<&str> {
        self.presentation.first_user_message.as_deref()
    }

    pub fn to_summary(&self) -> SessionSummary {
        SessionSummary {
            session_id: self.identity.session_id.clone(),
            created_at: self.stats.created_at.to_rfc3339(),
            updated_at: self.stats.updated_at.to_rfc3339(),
            working_dir: self.identity.working_dir.clone(),
            model_id: self.identity.model_id.clone(),
            parent_session_id: self
                .identity
                .parent
                .as_ref()
                .map(|parent| parent.session_id.clone()),
            phase: self.execution.phase,
            latest_cursor: self.cursor(),
            first_user_message: self.first_user_message().map(str::to_owned),
            source_extension: self.identity.source_extension.clone(),
        }
    }

    /// 返回 abort / repair 时必须补齐的 tool call。
    pub fn tool_calls_needing_interruption(&self) -> Vec<UnansweredToolCall> {
        self.model_context
            .tool_calls_needing_interruption(&self.execution.pending_tool_calls)
    }
}

/// 会话列表摘要读模型。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub created_at: String,
    pub updated_at: String,
    pub working_dir: String,
    pub model_id: String,
    pub parent_session_id: Option<SessionId>,
    pub phase: Phase,
    pub latest_cursor: Cursor,
    pub first_user_message: Option<String>,
    pub source_extension: Option<String>,
}
