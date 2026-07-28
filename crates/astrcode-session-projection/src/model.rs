//! Session read-model state derived from durable events.

use std::collections::{BTreeMap, HashMap, HashSet};

use astrcode_core::{
    event::Phase,
    llm::{LlmContent, LlmMessage, LlmRole},
    types::*,
};
use serde::{Deserialize, Serialize};

/// 子 Agent 会话的运行状态。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionStatus {
    /// 正在运行。
    #[default]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<ToolCallId>,
    /// 子 Agent 名称（来自 RunSession 的 name）。
    pub agent_name: String,
    /// 子 Agent 任务描述（来自 RunSession 的 user_prompt）。
    pub task: String,
    /// 子会话运行状态。
    #[serde(default)]
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
    /// live-only 阶段投影，持久快照允许为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    /// live-only 当前工具名，持久快照允许为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_tool: Option<String>,
}

/// compact boundary 在会话投影中的元数据。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompactBoundaryView {
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
    /// boundary 事件的 seq。
    pub seq: u64,
    /// compact 基于的事件 seq（幂等校验键）。
    pub base_event_seq: u64,
    /// compact 策略。
    pub strategy: astrcode_core::compaction::CompactStrategy,
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
    /// 最近一次将该消息更新到 durable 时序中的 seq。
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

// ─── extension Event Index ────────────────────────────────────────────────

/// 插件事件索引条目——不存 payload，按需从 event log 取。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ExtensionEventEntry {
    /// 事件在 event log 中的 seq。
    pub seq: u64,
    /// 插件 ID。
    pub extension_id: String,
    /// 事件类型名。
    pub event_type: String,
    /// payload schema 版本。
    pub schema_version: u32,
}

/// 插件事件索引，由核心 reducer 在遇到 extensionEvent 时自动填充。
///
/// 不理解插件语义，只提供按 `extension_id` + `event_type` 的结构化查询。
/// payload 需要时通过 seq 从 event log 读取。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExtensionEventIndex {
    entries: Vec<ExtensionEventEntry>,
    by_extension: HashMap<String, Vec<usize>>,
}

impl ExtensionEventIndex {
    /// 追加一条索引。
    pub fn push(
        &mut self,
        seq: u64,
        extension_id: String,
        event_type: String,
        schema_version: u32,
    ) {
        let idx = self.entries.len();
        match self.by_extension.get_mut(&extension_id) {
            Some(indices) => indices.push(idx),
            None => {
                self.by_extension.insert(extension_id.clone(), vec![idx]);
            },
        }
        self.entries.push(ExtensionEventEntry {
            seq,
            extension_id,
            event_type,
            schema_version,
        });
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

/// 会话事件流的内部读模型。
///
/// 这是 storage/domain 边界类型，不是 wire DTO。它只能由事件日志重建，并由
/// server 映射到具体传输协议。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionReadModel {
    /// 会话唯一标识。
    pub session_id: SessionId,
    /// 普通对话消息历史。
    pub messages: Vec<SequencedLlmMessage>,
    /// provider 可见但不展示给普通 transcript 的上下文消息。
    pub context_messages: Vec<SequencedLlmMessage>,
    /// 仅用于 transcript 展示、不进入 provider 上下文的 durable 记录。
    #[serde(default)]
    pub transcript_artifacts: Vec<TranscriptArtifactView>,
    /// 会话工作目录。
    pub working_dir: String,
    /// 模型标识。
    pub model_id: String,
    /// 当前执行阶段。
    pub phase: Phase,
    /// 会话级 system prompt。
    pub system_prompt: Option<String>,
    /// 会话额外 system prompt（子会话场景）。
    #[serde(default)]
    pub extra_system_prompt: Option<String>,
    /// 最近一次 system prompt 的 fingerprint，用于检测工具/skill/agents.md 变化。
    #[serde(default)]
    pub system_prompt_fingerprint: Option<String>,
    /// 尚未完成的工具调用。
    pub pending_tool_calls: HashSet<ToolCallId>,
    /// 等待核心权限审批的工具调用（call_id → 审批提示）。
    #[serde(default)]
    pub pending_tool_approvals: BTreeMap<ToolCallId, PendingToolApprovalView>,
    /// 创建时间（ISO 8601）。
    pub created_at: String,
    /// 更新时间（ISO 8601）。
    pub updated_at: String,
    /// 父会话 ID。
    pub parent_session_id: Option<SessionId>,
    /// Session 生效的工具集策略。
    ///
    /// 初始值来自 `SessionStarted.tool_selection`，后续可由
    /// `SessionToolsConfigured` 更新。`None` 表示不限制工具。
    #[serde(default)]
    pub tool_selection: Option<astrcode_core::tool::SessionToolSelection>,
    /// 创建该子 session 的扩展 ID。
    #[serde(default)]
    pub source_extension: Option<String>,
    /// 父会话派生的子 Agent 会话列表。
    #[serde(default)]
    pub agent_sessions: Vec<AgentSessionLinkView>,
    /// compact boundary 元数据列表，按 seq 递增排列。
    #[serde(default)]
    pub compact_boundaries: Vec<CompactBoundaryView>,
    /// 最新 durable 事件 seq。
    pub latest_seq: Option<u64>,
    /// 插件事件索引，不存 payload，按需从 event log 取。
    #[serde(default)]
    pub extension_events: ExtensionEventIndex,
}

impl SessionReadModel {
    /// 创建空读模型。
    pub fn empty(session_id: SessionId) -> Self {
        Self {
            session_id,
            messages: Vec::new(),
            context_messages: Vec::new(),
            transcript_artifacts: Vec::new(),
            working_dir: String::new(),
            model_id: String::new(),
            phase: Phase::Idle,
            system_prompt: None,
            extra_system_prompt: None,
            system_prompt_fingerprint: None,
            pending_tool_calls: HashSet::new(),
            pending_tool_approvals: BTreeMap::new(),
            created_at: String::new(),
            updated_at: String::new(),
            parent_session_id: None,
            tool_selection: None,
            source_extension: None,
            agent_sessions: Vec::new(),
            compact_boundaries: Vec::new(),
            latest_seq: None,
            extension_events: ExtensionEventIndex::default(),
        }
    }

    /// 返回 provider 可见消息。
    ///
    /// 包含防御性归一化：
    /// 1. 将连续的 assistant+tool_calls 消息合并为一条
    /// 2. 截断不完整的 tool 协议轮，避免 DeepSeek 等严格 provider 拒绝请求
    pub fn provider_messages(&self) -> Vec<LlmMessage> {
        let mut messages = Vec::with_capacity(
            self.context_messages
                .len()
                .saturating_add(self.messages.len()),
        );
        messages.extend(
            self.context_messages
                .iter()
                .chain(self.messages.iter())
                .map(|sequenced| sequenced.message.clone()),
        );
        astrcode_core::llm::provider_visible_messages(messages)
    }

    /// 是否已有普通 transcript 消息。
    pub fn has_messages(&self) -> bool {
        !self.messages.is_empty()
    }

    /// 当前快照 cursor。
    pub fn cursor(&self) -> Cursor {
        self.latest_seq
            .map(|seq| seq.to_string())
            .unwrap_or_else(|| "0".into())
    }

    /// 首条用户消息的文本内容，无用户消息时返回 None。
    pub fn first_user_message(&self) -> Option<String> {
        self.messages
            .iter()
            .find(|m| m.source.is_none() && matches!(m.message.role, LlmRole::User))
            .and_then(|m| m.message.content.iter().find_map(LlmContent::as_text))
            .map(str::to_owned)
    }

    /// 统计 provider 可见的非合成 user 消息条数。
    pub fn visible_user_message_count(&self) -> usize {
        self.messages
            .iter()
            .filter(|entry| {
                entry.message.role == LlmRole::User
                    && !astrcode_core::context::is_synthetic_context_message(&entry.message)
            })
            .count()
    }

    /// 生成会话列表摘要，只复制列表接口需要的少量字段。
    pub fn to_summary(&self) -> SessionSummary {
        SessionSummary {
            session_id: self.session_id.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            working_dir: self.working_dir.clone(),
            model_id: self.model_id.clone(),
            parent_session_id: self.parent_session_id.clone(),
            phase: self.phase,
            latest_cursor: self.cursor(),
            first_user_message: self.first_user_message(),
            source_extension: self.source_extension.clone(),
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
        let mut remaining = self.pending_tool_calls.clone();
        let mut pending = Vec::new();

        for message in &self.messages {
            if message.message.role != LlmRole::Assistant {
                continue;
            }
            for content in &message.message.content {
                let LlmContent::ToolCall { call_id, name, .. } = content else {
                    continue;
                };
                if remaining.remove(call_id.as_str()) {
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
        let Some(last_assistant_index) = self.messages.iter().rposition(|message| {
            message.message.role == LlmRole::Assistant
                && message
                    .message
                    .content
                    .iter()
                    .any(|content| matches!(content, LlmContent::ToolCall { .. }))
        }) else {
            return Vec::new();
        };

        let assistant = &self.messages[last_assistant_index].message;
        let mut pending = assistant
            .content
            .iter()
            .filter_map(|content| match content {
                LlmContent::ToolCall { call_id, name, .. } => Some((
                    call_id.clone(),
                    UnansweredToolCall {
                        call_id: call_id.clone(),
                        tool_name: name.clone(),
                    },
                )),
                _ => None,
            })
            .collect::<HashMap<_, _>>();

        for message in self.messages.iter().skip(last_assistant_index + 1) {
            if message.message.role != LlmRole::Tool {
                return Vec::new();
            }
            for content in &message.message.content {
                let LlmContent::ToolResult { tool_call_id, .. } = content else {
                    continue;
                };
                pending.remove(tool_call_id);
            }
        }

        pending.into_values().collect()
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

impl From<SessionReadModel> for SessionSummary {
    fn from(model: SessionReadModel) -> Self {
        let latest_cursor = model.cursor();
        let first_user_message = model.first_user_message();
        Self {
            session_id: model.session_id,
            created_at: model.created_at,
            updated_at: model.updated_at,
            working_dir: model.working_dir,
            model_id: model.model_id,
            parent_session_id: model.parent_session_id,
            phase: model.phase,
            latest_cursor,
            first_user_message,
            source_extension: model.source_extension,
        }
    }
}
