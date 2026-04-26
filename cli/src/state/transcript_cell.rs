use std::collections::BTreeSet;

use astrcode_client::{AgentLifecycleDto, ConversationBlockDto, ConversationBlockStatusDto};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptCell {
    pub id: String,
    pub expanded: bool,
    pub kind: TranscriptCellKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptCellStatus {
    Streaming,
    Complete,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptCellKind {
    User {
        body: String,
    },
    Assistant {
        body: String,
        status: TranscriptCellStatus,
    },
    Thinking {
        body: String,
        status: TranscriptCellStatus,
    },
    ToolCall {
        tool_name: String,
        summary: String,
        status: TranscriptCellStatus,
        stdout: String,
        stderr: String,
        error: Option<String>,
        duration_ms: Option<u64>,
        truncated: bool,
        child_session_id: Option<String>,
    },
    Error {
        code: String,
        message: String,
    },
    SystemNote {
        note_kind: String,
        markdown: String,
    },
    ChildHandoff {
        handoff_kind: String,
        title: String,
        lifecycle: AgentLifecycleDto,
        message: String,
        child_session_id: String,
        child_agent_id: String,
    },
}

impl TranscriptCell {
    pub fn from_block(block: &ConversationBlockDto, expanded_ids: &BTreeSet<String>) -> Self {
        let id = match block {
            ConversationBlockDto::User(block) => block.id.clone(),
            ConversationBlockDto::Assistant(block) => block.id.clone(),
            ConversationBlockDto::Thinking(block) => block.id.clone(),
            ConversationBlockDto::PromptMetrics(block) => block.id.clone(),
            ConversationBlockDto::Plan(block) => block.id.clone(),
            ConversationBlockDto::ToolCall(block) => block.id.clone(),
            ConversationBlockDto::Error(block) => block.id.clone(),
            ConversationBlockDto::SystemNote(block) => block.id.clone(),
            ConversationBlockDto::ChildHandoff(block) => block.id.clone(),
        };
        let expanded = expanded_ids.contains(&id)
            || matches!(
                block,
                ConversationBlockDto::Thinking(thinking)
                    if matches!(thinking.status, ConversationBlockStatusDto::Streaming)
            );
        match block {
            ConversationBlockDto::User(block) => Self {
                id,
                expanded,
                kind: TranscriptCellKind::User {
                    body: block.markdown.clone(),
                },
            },
            ConversationBlockDto::Assistant(block) => Self {
                id,
                expanded,
                kind: TranscriptCellKind::Assistant {
                    body: block.markdown.clone(),
                    status: block.status.into(),
                },
            },
            ConversationBlockDto::Thinking(block) => Self {
                id,
                expanded,
                kind: TranscriptCellKind::Thinking {
                    body: block.markdown.clone(),
                    status: block.status.into(),
                },
            },
            ConversationBlockDto::PromptMetrics(block) => Self {
                id,
                expanded,
                kind: TranscriptCellKind::SystemNote {
                    note_kind: "prompt_metrics".to_string(),
                    markdown: format!(
                        "step #{} | context {} / {} | cache read {}",
                        block.step_index,
                        block.effective_window,
                        block.context_window,
                        block.cache_read_input_tokens.unwrap_or_default()
                    ),
                },
            },
            ConversationBlockDto::Plan(block) => Self {
                id,
                expanded,
                kind: TranscriptCellKind::SystemNote {
                    note_kind: format!(
                        "plan:{}",
                        enum_wire_name(&block.event_kind).unwrap_or_else(|| "saved".to_string())
                    ),
                    markdown: block
                        .summary
                        .clone()
                        .or_else(|| block.content.clone())
                        .unwrap_or_else(|| format!("{} ({})", block.title, block.plan_path)),
                },
            },
            ConversationBlockDto::ToolCall(block) => Self {
                id,
                expanded,
                kind: TranscriptCellKind::ToolCall {
                    tool_name: block.tool_name.clone(),
                    summary: block
                        .summary
                        .clone()
                        .or_else(|| block.error.clone())
                        .or_else(|| {
                            if block.streams.stdout.is_empty() && block.streams.stderr.is_empty() {
                                None
                            } else {
                                Some("工具输出已更新".to_string())
                            }
                        })
                        .unwrap_or_else(|| "正在执行工具调用".to_string()),
                    status: block.status.into(),
                    stdout: block.streams.stdout.clone(),
                    stderr: block.streams.stderr.clone(),
                    error: block.error.clone(),
                    duration_ms: block.duration_ms,
                    truncated: block.truncated,
                    child_session_id: block
                        .child_ref
                        .as_ref()
                        .map(|child_ref| child_ref.open_session_id.clone()),
                },
            },
            ConversationBlockDto::Error(block) => Self {
                id,
                expanded,
                kind: TranscriptCellKind::Error {
                    code: enum_wire_name(&block.code)
                        .unwrap_or_else(|| "unknown_error".to_string()),
                    message: block.message.clone(),
                },
            },
            ConversationBlockDto::SystemNote(block) => Self {
                id,
                expanded,
                kind: TranscriptCellKind::SystemNote {
                    note_kind: enum_wire_name(&block.note_kind)
                        .unwrap_or_else(|| "system_note".to_string()),
                    markdown: block.markdown.clone(),
                },
            },
            ConversationBlockDto::ChildHandoff(block) => Self {
                id,
                expanded,
                kind: TranscriptCellKind::ChildHandoff {
                    handoff_kind: enum_wire_name(&block.handoff_kind)
                        .unwrap_or_else(|| "delegated".to_string()),
                    title: block.child.title.clone(),
                    lifecycle: block.child.lifecycle,
                    message: block
                        .message
                        .clone()
                        .unwrap_or_else(|| "无摘要".to_string()),
                    child_session_id: block.child.child_session_id.clone(),
                    child_agent_id: block.child.child_agent_id.clone(),
                },
            },
        }
    }
}

impl From<ConversationBlockStatusDto> for TranscriptCellStatus {
    fn from(value: ConversationBlockStatusDto) -> Self {
        match value {
            ConversationBlockStatusDto::Streaming => Self::Streaming,
            ConversationBlockStatusDto::Complete => Self::Complete,
            ConversationBlockStatusDto::Failed => Self::Failed,
            ConversationBlockStatusDto::Cancelled => Self::Cancelled,
        }
    }
}

fn enum_wire_name<T>(value: &T) -> Option<String>
where
    T: serde::Serialize,
{
    serde_json::to_value(value)
        .ok()?
        .as_str()
        .map(|value| value.trim().to_string())
}
