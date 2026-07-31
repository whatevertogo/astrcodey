//! Session read-model state derived from durable events.

use std::collections::{BTreeMap, HashSet};

use astrcode_core::{
    event::{ParentSessionRef, Phase, SessionStarted, SystemPromptSource},
    llm::{LlmContent, LlmMessage, LlmRole},
    tool::SessionToolSelection,
    types::*,
    user_input::UserInput,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 子 Agent 会话的运行状态。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionStatus {
    /// 正在运行。
    Running,
    /// 正常完成。
    Completed,
    /// 失败。
    Failed,
}

/// 父会话派生的子 Agent 会话链接。
///
/// 由 `AgentSessionSpawned` 事件投影而来，表达"从父看子"的关系。
///
/// `child_session_id` 为稳定锚点；`final_session_id` 在终态事件写入后填充。
/// 当前 compact 为原地续写、不换 session id，故完成后二者相同。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSessionLinkView {
    /// 最初委托的子 session（`AgentSessionSpawned`；compact 不修改此 id）。
    pub child_session_id: SessionId,
    /// 触发此子会话的工具调用 ID。
    pub tool_call_id: ToolCallId,
    /// 子 Agent 名称（来自 RunSession 的 name）。
    pub agent_name: String,
    /// 子 Agent 任务描述（来自 RunSession 的 user_prompt）。
    pub task: String,
    /// 子会话运行状态。
    pub status: AgentSessionStatus,
    /// 产出结果的 leaf session；当前实现与 `child_session_id` 相同。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_session_id: Option<SessionId>,
    /// 子 Agent 完成摘要。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// 子 Agent 失败原因。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 一次 compaction 的投影元数据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactionView {
    /// compact 触发来源。
    pub trigger: String,
    /// 压缩前 token 数。
    pub pre_tokens: usize,
    /// 压缩后 token 数。
    pub post_tokens: usize,
    /// 压缩生成的摘要。
    pub summary: String,
    /// compact 前 transcript snapshot 路径。
    pub transcript_path: Option<String>,
    /// transcript rewrite 事件的 seq。
    pub seq: u64,
    /// compact candidate 基于的 projection seq。
    pub source_seq: u64,
    /// compact 策略。
    pub strategy: astrcode_core::compaction::CompactStrategy,
}

/// 创建 fork session 时记录的来源位置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForkSourceRef {
    pub session_id: SessionId,
    pub cursor: Cursor,
}

/// 工具执行失败消息的读模型来源标记。
pub const TOOL_CALL_FAILED_SOURCE: &str = "tool_call_failed";
/// 工具调用取消消息的读模型来源标记。
pub const TOOL_CALL_CANCELLED_SOURCE: &str = "tool_call_cancelled";

/// 会话读模型里带有 durable seq 的消息载体。
///
/// compact 需要按时间边界冻结历史前缀，并将 compact 期间到达的事件归类为尾部增量。
/// reducer 在处理 durable 事件时自然可得 `Event.seq`，因此这里将其显式挂到消息上，
/// 让边界切分不依赖“当前内存状态”推断。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SequencedLlmMessage {
    /// LLM 协议消息内容（内部结构）。
    pub message: LlmMessage,
    /// 消息所属的 durable 时序边界。
    ///
    /// 普通消息记录最近更新它的事件 seq；rewrite 输出锚定到其 `source_seq`，
    /// 以便后续 rewrite 能稳定区分前缀与并发 tail。
    pub updated_seq: u64,
    /// 消息来源标记，用于前端区分渲染。
    ///
    /// - `None`：正常消息（用户输入、LLM 回复等）
    /// - `Some("turn_aborted")`：上一轮中断标记（仅 provider 可见）
    /// - `Some("tool_call_failed" | "tool_call_cancelled")`：工具异常终态
    ///
    /// `source` 本身不进入 LLM payload；对应 `.message` 会作为 User 消息送入 provider。
    #[serde(default)]
    pub source: Option<String>,
}

/// 不进入 provider 上下文、但需要稳定显示在会话 transcript 中的 durable 记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum TranscriptArtifactView {
    Error {
        id: String,
        message: String,
        seq: u64,
    },
    SystemNote {
        id: String,
        text: String,
        seq: u64,
    },
}

impl TranscriptArtifactView {
    pub fn seq(&self) -> u64 {
        match self {
            Self::Error { seq, .. } | Self::SystemNote { seq, .. } => *seq,
        }
    }
}

/// 尚未得到 Tool 结果的 assistant tool call。
///
/// 这是从读模型推导出来的协议状态，用于 abort / repair 时补齐 provider
/// 要求的 tool result。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnansweredToolCall {
    pub call_id: String,
    pub tool_name: String,
}

/// 工具调用等待核心权限审批时的投影快照。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingToolApprovalView {
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_key: Option<String>,
}

/// 已持久接受、但尚未进入 transcript 的用户输入。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingInput {
    pub accepted_seq: u64,
    pub input: UserInput,
}

/// 会话事件流的内部读模型。
///
/// 这是 storage/domain 边界类型，不是 wire DTO。它只能由事件日志重建，并由
/// server 映射到具体传输协议。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionReadModel {
    pub identity: SessionIdentity,
    pub stats: SessionEventStats,
    pub system_prompt: SessionSystemPrompt,
    pub transcript: SessionTranscript,
    pub execution: SessionExecutionState,
    /// 父会话派生的子 Agent 会话列表。
    pub agent_sessions: Vec<AgentSessionLinkView>,
    /// compaction 元数据列表，按 seq 递增排列。
    pub compactions: Vec<CompactionView>,
    /// 最近一次可作为当前 transcript 前缀锚点的 provider 上下文用量。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_usage: Option<ContextUsageView>,
}

/// Provider 用量覆盖的 transcript 前缀。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextUsageView {
    pub context_tokens: usize,
    pub model_context_window: usize,
    pub covered_message_count: usize,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionSystemPrompt {
    pub text: String,
    pub extra: Option<String>,
    pub fingerprint: String,
    pub source: SystemPromptSource,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionTranscript {
    pub first_user_message: Option<String>,
    pub messages: Vec<SequencedLlmMessage>,
    pub artifacts: Vec<TranscriptArtifactView>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionExecutionState {
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsettled_turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_inputs: Vec<PendingInput>,
    pub pending_tool_calls: HashSet<ToolCallId>,
    pub pending_tool_approvals: BTreeMap<ToolCallId, PendingToolApprovalView>,
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
            transcript: SessionTranscript::default(),
            execution: SessionExecutionState::default(),
            agent_sessions: Vec::new(),
            compactions: Vec::new(),
            context_usage: None,
        }
    }

    /// 是否已有普通 transcript 消息。
    pub fn has_messages(&self) -> bool {
        !self.transcript.messages.is_empty()
    }

    /// 当前快照 cursor。
    pub fn cursor(&self) -> Cursor {
        self.stats.last_seq.to_string()
    }

    /// 首条用户消息的文本内容，无用户消息时返回 None。
    pub fn first_user_message(&self) -> Option<&str> {
        self.transcript.first_user_message.as_deref()
    }

    /// 生成会话列表摘要，只复制列表接口需要的少量字段。
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
    ///
    /// 优先使用投影中的 `pending_tool_calls`；同时检查 transcript 尾部，覆盖
    /// 旧日志或异常恢复时 assistant tool call 已存在、pending 集合不完整的情况。
    pub fn tool_calls_needing_interruption(&self) -> Vec<UnansweredToolCall> {
        let mut pending = self.pending_requested_tool_calls();
        let mut seen = pending
            .iter()
            .map(|call| call.call_id.clone())
            .collect::<HashSet<_>>();

        for call in self.tail_unanswered_tool_calls() {
            if seen.insert(call.call_id.clone()) {
                pending.push(call);
            }
        }

        pending
    }

    fn pending_requested_tool_calls(&self) -> Vec<UnansweredToolCall> {
        let mut seen = HashSet::new();
        let mut pending = Vec::new();

        for message in &self.transcript.messages {
            if message.message.role != LlmRole::Assistant {
                continue;
            }
            for content in &message.message.content {
                let LlmContent::ToolCall { call_id, name, .. } = content else {
                    continue;
                };
                if self.execution.pending_tool_calls.contains(call_id.as_str())
                    && seen.insert(call_id.as_str())
                {
                    pending.push(UnansweredToolCall {
                        call_id: call_id.clone(),
                        tool_name: name.clone(),
                    });
                }
            }
        }

        pending
    }

    fn tail_unanswered_tool_calls(&self) -> Vec<UnansweredToolCall> {
        let Some(last_assistant_index) = self.transcript.messages.iter().rposition(|message| {
            message.message.role == LlmRole::Assistant
                && message
                    .message
                    .content
                    .iter()
                    .any(|content| matches!(content, LlmContent::ToolCall { .. }))
        }) else {
            return Vec::new();
        };

        let assistant = &self.transcript.messages[last_assistant_index].message;
        let mut answered = HashSet::new();

        for message in self
            .transcript
            .messages
            .iter()
            .skip(last_assistant_index + 1)
        {
            if message.message.role != LlmRole::Tool {
                return Vec::new();
            }
            for content in &message.message.content {
                let LlmContent::ToolResult { tool_call_id, .. } = content else {
                    continue;
                };
                answered.insert(tool_call_id.as_str());
            }
        }

        let mut seen = HashSet::new();
        assistant
            .content
            .iter()
            .filter_map(|content| match content {
                LlmContent::ToolCall { call_id, name, .. }
                    if !answered.contains(call_id.as_str()) && seen.insert(call_id.as_str()) =>
                {
                    Some(UnansweredToolCall {
                        call_id: call_id.clone(),
                        tool_name: name.clone(),
                    })
                },
                _ => None,
            })
            .collect()
    }
}

/// 会话列表摘要读模型。
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    /// 会话唯一标识。
    pub session_id: SessionId,
    /// 创建时间（ISO 8601）。
    pub created_at: String,
    /// 更新时间（ISO 8601）。
    pub updated_at: String,
    /// 工作目录。
    pub working_dir: String,
    /// 模型标识。
    pub model_id: String,
    /// 父会话 ID。
    pub parent_session_id: Option<SessionId>,
    /// 当前执行阶段。
    pub phase: Phase,
    /// 最新 durable cursor。
    pub latest_cursor: Cursor,
    /// 首条用户消息内容，无消息时为 None。
    pub first_user_message: Option<String>,
    /// 创建该子 session 的扩展 ID。
    pub source_extension: Option<String>,
}
