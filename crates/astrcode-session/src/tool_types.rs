//! Agent 工具调用的数据类型定义。
//!
//! 包含工具调用从 LLM 流式响应中积累、预处理、到最终执行各阶段的类型。

use std::collections::BTreeMap;

use astrcode_core::{
    event::EventPayload,
    llm::{LlmContent, LlmMessage, LlmRole},
    storage::ToolResultArtifactReader,
    tool::{
        AgentSessionControl, BackgroundTaskReader, ExecutionMode, ToolDefinition, ToolResult,
    },
    types::*,
};
use tokio::sync::mpsc;

use super::{
    background::BackgroundTaskManager,
    turn_context::{AgentSignal, send_event},
};

/// 等待执行的工具调用，在 LLM 流式响应中逐步积累参数。
pub struct PendingToolCall {
    /// 工具调用的唯一标识
    pub call_id: String,
    /// 工具名称
    pub name: String,
    /// 工具调用的 JSON 参数（可能跨多个 delta 事件拼接）
    pub arguments: String,
}

pub struct PreparedToolCall {
    pub index: usize,
    pub call_id: String,
    pub name: String,
    pub tool_input: serde_json::Value,
    pub mode: ExecutionMode,
    pub outcome: PreparedToolOutcome,
}

pub struct ExecuteToolCalls<'a> {
    pub prepared: &'a [PreparedToolCall],
    pub tools: &'a [ToolDefinition],
    pub messages: &'a mut Vec<LlmMessage>,
    pub all_tool_results: &'a mut Vec<ToolResult>,
    pub event_tx: &'a Option<mpsc::UnboundedSender<AgentSignal>>,
}

pub struct CommitToolResults<'a> {
    pub prepared: &'a [PreparedToolCall],
    pub results: BTreeMap<usize, ToolResult>,
    pub messages: &'a mut Vec<LlmMessage>,
    pub all_tool_results: &'a mut Vec<ToolResult>,
    pub event_tx: &'a Option<mpsc::UnboundedSender<AgentSignal>>,
}

pub struct PendingCommittedToolResult {
    pub call_id: String,
    pub tool_name: String,
    pub result: ToolResult,
}

pub enum ToolExecutionStep {
    Blocked(ToolResult),
    Parallel(ExecutableToolCall),
    Sequential(ExecutableToolCall),
}

pub enum PreparedToolOutcome {
    Ready,
    Blocked(ToolResult),
}

#[derive(Clone)]
pub struct ExecutableToolCall {
    pub index: usize,
    pub call_id: String,
    pub name: String,
    pub tool_input: serde_json::Value,
}

pub struct ToolCallRuntimeContext {
    pub session_id: SessionId,
    pub working_dir: String,
    pub model_id: String,
    pub tools: Vec<ToolDefinition>,
    pub tool_result_reader: Option<Arc<dyn ToolResultArtifactReader>>,
    pub event_tx: Option<mpsc::UnboundedSender<AgentSignal>>,
    pub capabilities: ToolRuntimeCapabilities,
}

impl PreparedToolCall {
    /// 将预处理后的工具调用转换为可执行任务输入。
    pub fn to_executable(&self) -> ExecutableToolCall {
        ExecutableToolCall {
            index: self.index,
            call_id: self.call_id.clone(),
            name: self.name.clone(),
            tool_input: self.tool_input.clone(),
        }
    }
}

/// 向客户端报告工具调用已经通过预处理并准备执行。
pub fn send_tool_requested(
    event_tx: &Option<mpsc::UnboundedSender<AgentSignal>>,
    tc: &PendingToolCall,
    arguments: &serde_json::Value,
) {
    send_event(
        event_tx,
        EventPayload::ToolCallRequested {
            call_id: tc.call_id.clone().into(),
            tool_name: tc.name.clone(),
            arguments: arguments.clone(),
        },
    );
}

/// 将本轮 assistant 产生的工具调用整理成 LLM 历史消息。
pub fn assistant_tool_call_message(
    prepared: &[PreparedToolCall],
    text: &str,
    reasoning_content: Option<String>,
) -> LlmMessage {
    let mut content = Vec::with_capacity(prepared.len() + usize::from(!text.is_empty()));
    if !text.is_empty() {
        content.push(LlmContent::Text {
            text: text.to_string(),
        });
    }
    content.extend(prepared.iter().map(|call| LlmContent::ToolCall {
        call_id: call.call_id.clone(),
        name: call.name.clone(),
        arguments: call.tool_input.clone(),
    }));

    LlmMessage {
        role: LlmRole::Assistant,
        content,
        name: None,
        reasoning_content,
    }
}

pub fn committed_tool_result_content_len(messages: &[LlmMessage]) -> usize {
    messages
        .iter()
        .filter(|message| message.role == LlmRole::Tool)
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            LlmContent::ToolResult { content, .. } => Some(content.len()),
            _ => None,
        })
        .sum()
}

/// 为没有产出结果的工具调用生成占位错误结果。
pub fn missing_tool_result(call: &PreparedToolCall) -> ToolResult {
    let message = format!("Tool '{}' did not produce a result", call.name);
    ToolResult {
        call_id: call.call_id.clone(),
        content: message.clone(),
        is_error: true,
        error: Some(message),
        metadata: Default::default(),
        duration_ms: None,
    }
}

use std::sync::Arc;

// ─── Tool runtime capabilities ──────────────────────────────────────────

/// 会话级工具运行时能力，从 ToolPipeline 透传到 ToolExecutionContext。
///
/// 整合了后台任务、文件观察、agent 会话控制等按 session 生命周期存在的能力。
#[derive(Clone)]
pub struct ToolRuntimeCapabilities {
    /// 后台任务完成后的通知通道。
    pub background_result_tx: Option<mpsc::UnboundedSender<crate::background::BackgroundTaskCompletion>>,
    /// 后台任务管理器，用于注册 watcher handle 以支持取消。
    pub background_tasks: Arc<parking_lot::Mutex<BackgroundTaskManager>>,
    /// 后台任务只读接口，注入到 ToolExecutionContext 供 TaskTool 使用。
    pub background_task_reader: Option<Arc<dyn BackgroundTaskReader>>,
    /// 文件观察存储，用于 read/edit 协作的 read-before-edit 守卫。
    pub file_observation_store: Option<Arc<dyn astrcode_core::tool::FileObservationStore>>,
    /// Agent 会话操控能力，用于 send 等工具与子 session 交互。
    pub agent_session_control: Option<Arc<dyn AgentSessionControl>>,
}
