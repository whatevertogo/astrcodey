//! Agent 工具调用的数据类型定义。
//!
//! 包含工具调用从 LLM 流式响应中积累、预处理、到最终执行各阶段的类型。

use std::collections::BTreeMap;

use astrcode_core::{
    event::EventPayload,
    llm::{LlmContent, LlmMessage, LlmRole},
    storage::ToolResultArtifactReader,
    tool::{ExecutionMode, ToolDefinition, ToolResult},
    types::*,
};
use tokio::sync::mpsc;

use super::{
    background::BackgroundTaskManager,
    shared_context::{AgentSignal, send_event},
};

/// 等待执行的工具调用，在 LLM 流式响应中逐步积累参数。
pub(crate) struct PendingToolCall {
    /// 工具调用的唯一标识
    pub(crate) call_id: String,
    /// 工具名称
    pub(crate) name: String,
    /// 工具调用的 JSON 参数（可能跨多个 delta 事件拼接）
    pub(crate) arguments: String,
}

pub(crate) struct PreparedToolCall {
    pub(crate) index: usize,
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) tool_input: serde_json::Value,
    pub(crate) mode: ExecutionMode,
    pub(crate) outcome: PreparedToolOutcome,
}

pub(super) struct ExecuteToolCalls<'a> {
    pub(super) prepared: &'a [PreparedToolCall],
    pub(super) tools: &'a [ToolDefinition],
    pub(super) messages: &'a mut Vec<LlmMessage>,
    pub(super) all_tool_results: &'a mut Vec<ToolResult>,
    pub(super) event_tx: &'a Option<mpsc::UnboundedSender<AgentSignal>>,
}

pub(super) struct CommitToolResults<'a> {
    pub(super) prepared: &'a [PreparedToolCall],
    pub(super) results: BTreeMap<usize, ToolResult>,
    pub(super) tools: &'a [ToolDefinition],
    pub(super) messages: &'a mut Vec<LlmMessage>,
    pub(super) all_tool_results: &'a mut Vec<ToolResult>,
    pub(super) event_tx: &'a Option<mpsc::UnboundedSender<AgentSignal>>,
}

pub(super) struct PendingCommittedToolResult {
    pub(super) call_id: String,
    pub(super) tool_name: String,
    pub(super) result: ToolResult,
}

pub(super) enum ToolExecutionStep {
    Blocked(ToolResult),
    Parallel(ExecutableToolCall),
    Sequential(ExecutableToolCall),
}

pub(crate) enum PreparedToolOutcome {
    Ready,
    Blocked(ToolResult),
}

#[derive(Clone)]
pub(crate) struct ExecutableToolCall {
    pub(crate) index: usize,
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) tool_input: serde_json::Value,
}

/// 后台任务完成通知的载荷。
pub struct BackgroundTaskCompletion {
    pub(crate) session_id: SessionId,
    pub(crate) task_id: BackgroundTaskId,
    pub(crate) call_id: ToolCallId,
    pub(crate) tool_name: String,
    pub(crate) result: ToolResult,
}

pub(crate) struct ToolCallRuntimeContext {
    pub(super) session_id: SessionId,
    pub(super) working_dir: String,
    pub(super) model_id: String,
    pub(super) tools: Vec<ToolDefinition>,
    pub(super) tool_result_reader: Option<Arc<dyn ToolResultArtifactReader>>,
    pub(super) event_tx: Option<mpsc::UnboundedSender<AgentSignal>>,
    /// 后台任务完成后的通知通道。
    pub(super) background_result_tx: Option<mpsc::UnboundedSender<BackgroundTaskCompletion>>,
    /// 后台任务管理器，用于注册 watcher handle 以支持取消。
    pub(super) background_tasks: Arc<std::sync::Mutex<BackgroundTaskManager>>,
}

impl PreparedToolCall {
    /// 将预处理后的工具调用转换为可执行任务输入。
    pub(crate) fn to_executable(&self) -> ExecutableToolCall {
        ExecutableToolCall {
            index: self.index,
            call_id: self.call_id.clone(),
            name: self.name.clone(),
            tool_input: self.tool_input.clone(),
        }
    }
}

/// 向客户端报告工具调用已经通过预处理并准备执行。
pub(crate) fn send_tool_requested(
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
pub(crate) fn assistant_tool_call_message(prepared: &[PreparedToolCall]) -> LlmMessage {
    LlmMessage {
        role: LlmRole::Assistant,
        content: prepared
            .iter()
            .map(|call| LlmContent::ToolCall {
                call_id: call.call_id.clone(),
                name: call.name.clone(),
                arguments: call.tool_input.clone(),
            })
            .collect(),
        name: None,
    }
}

pub(super) fn committed_tool_result_content_len(messages: &[LlmMessage]) -> usize {
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
pub(crate) fn missing_tool_result(call: &PreparedToolCall) -> ToolResult {
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
