//! Agent 相关类型定义：回合输出、错误、工具调用中间数据结构。

use std::{collections::HashSet, sync::Arc, time::Instant};

use astrcode_core::{
    event::EventPayload,
    llm::{LlmContent, LlmMessage, LlmRole},
    tool::{ExecutionMode, ToolDefinition, ToolExecutionContext, ToolResult},
};
use astrcode_tools::registry::ToolRegistry;
use tokio::sync::mpsc;

/// Agent 回合的输出结果。
pub struct AgentTurnOutput {
    /// 助手回复的文本内容
    pub text: String,
    /// 结束原因（如 "stop"、"tool_calls"）
    pub finish_reason: String,
    /// 本回合中所有工具调用的结果
    pub tool_results: Vec<ToolResult>,
}

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

impl PreparedToolCall {
    pub(crate) fn to_executable(&self) -> ExecutableToolCall {
        ExecutableToolCall {
            index: self.index,
            call_id: self.call_id.clone(),
            name: self.name.clone(),
            tool_input: self.tool_input.clone(),
        }
    }
}

pub(crate) fn send_tool_requested(
    event_tx: &Option<mpsc::UnboundedSender<EventPayload>>,
    tc: &PendingToolCall,
    arguments: &serde_json::Value,
) {
    if let Some(tx) = event_tx {
        let _ = tx.send(EventPayload::ToolCallRequested {
            call_id: tc.call_id.clone(),
            tool_name: tc.name.clone(),
            arguments: arguments.clone(),
        });
    }
}

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

pub(crate) async fn execute_tool_call(
    tool_registry: Arc<ToolRegistry>,
    session_id: String,
    working_dir: String,
    model_id: String,
    tools: Vec<ToolDefinition>,
    event_tx: Option<mpsc::UnboundedSender<EventPayload>>,
    call: ExecutableToolCall,
) -> (usize, ToolResult) {
    let started_at = Instant::now();
    let tool_ctx = ToolExecutionContext {
        session_id,
        working_dir,
        model_id,
        available_tools: tools,
        tool_call_id: Some(call.call_id.clone()),
        event_tx,
    };

    let mut result = match tool_registry
        .execute(&call.name, call.tool_input.clone(), &tool_ctx)
        .await
    {
        Ok(mut result) => {
            result.call_id = call.call_id.clone();
            result.duration_ms = Some(started_at.elapsed().as_millis() as u64);
            result
        },
        Err(e) => {
            let err_msg = format!("Error: {}", e);
            ToolResult {
                call_id: call.call_id.clone(),
                content: err_msg.clone(),
                is_error: true,
                error: Some(err_msg),
                metadata: Default::default(),
                duration_ms: Some(started_at.elapsed().as_millis() as u64),
            }
        },
    };

    if result.call_id.is_empty() {
        result.call_id = call.call_id.clone();
    }

    (call.index, result)
}

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

/// Agent 处理过程中可能出现的错误类型。
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("LLM error: {0}")]
    Llm(String),
    #[error("Tool error: {0}")]
    Tool(#[from] astrcode_core::tool::ToolError),
    #[error("Extension error: {0}")]
    Extension(#[from] astrcode_core::extension::ExtensionError),
}

impl From<astrcode_core::llm::LlmError> for AgentError {
    fn from(e: astrcode_core::llm::LlmError) -> Self {
        AgentError::Llm(e.to_string())
    }
}

/// 检查工具名是否匹配白名单，支持 Claude 风格的别名映射。
/// 例如白名单中有 "Read"，则 "readFile" 也能匹配。
pub(crate) fn tool_name_matches_allowlist(allowed: &HashSet<String>, tool_name: &str) -> bool {
    allowed.iter().any(|allowed_name| {
        allowed_name == tool_name
            || claude_tool_alias(allowed_name)
                .is_some_and(|alias| alias.eq_ignore_ascii_case(tool_name))
    })
}

/// 将简短的工具名映射为 Claude 风格的实际工具名。
/// 例如 "read" → "readFile"，"bash" → "shell"。
fn claude_tool_alias(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "read" => Some("readFile"),
        "write" => Some("writeFile"),
        "edit" | "multiedit" => Some("editFile"),
        "grep" => Some("grep"),
        "glob" => Some("findFiles"),
        "bash" => Some("shell"),
        _ => None,
    }
}
