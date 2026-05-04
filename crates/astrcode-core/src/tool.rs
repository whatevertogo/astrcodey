//! 工具 trait 及关联类型。
//!
//! 工具是 Agent 与外部世界交互的主要方式。
//! 扩展可以在内置工具集之外注册额外的工具。
//!
//! 本模块定义了：
//! - [`Tool`] trait：所有工具（内置和扩展注册）的核心接口
//! - [`ToolDefinition`]：发送给 LLM 的工具函数调用 schema
//! - [`ToolResult`]：工具执行结果
//! - [`ToolExecutionContext`]：每次工具调用的上下文

use std::{collections::BTreeMap, sync::Arc};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::{event::EventPayload, storage::ToolResultArtifactReader};

/// 工具定义，作为函数调用 schema 发送给 LLM。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOrigin {
    /// First-party core tools required by the coding runtime.
    Builtin,
    /// First-party tool packs shipped with the server but not fundamental to the tool trait.
    Bundled,
    /// Tools contributed by user or project extensions.
    Extension,
    /// Tools registered by a future SDK surface.
    Sdk
}

/// 工具定义，作为函数调用 schema 发送给 LLM。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// 唯一工具名称（如 "read"、"shell"）。
    pub name: String,
    /// 工具功能的人类可读描述。
    pub description: String,
    /// 工具参数的 JSON Schema 定义。
    pub parameters: serde_json::Value,
    /// 工具来源。来源只影响诊断、策略和优先级，不创建额外执行路径。
    pub origin: ToolOrigin,
}

/// 工具执行结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    /// 此结果对应的工具调用 ID。
    pub call_id: String,
    /// 工具输出的内容文本。
    pub content: String,
    /// 此结果是否表示错误。
    pub is_error: bool,
    /// 可选的规范化错误消息，供需要结构化错误展示的消费者使用。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 可选的元数据键值对（如文件路径、行数等）。
    pub metadata: BTreeMap<String, serde_json::Value>,
    /// 工具执行耗时（毫秒），由调用方测量。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// 工具执行过程中可能发生的错误。
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// 找不到指定的工具。
    #[error("Tool not found: {0}")]
    NotFound(String),
    /// 工具参数无效。
    #[error("Invalid arguments: {0}")]
    InvalidArguments(String),
    /// 工具执行出错。
    #[error("Execution error: {0}")]
    Execution(String),
    /// 工具执行被钩子阻止。
    #[error("Tool execution blocked by hook: {0}")]
    Blocked(String),
    /// 工具执行超时。
    #[error("Timeout after {0}ms")]
    Timeout(u64),
}

/// 工具的执行模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionMode {
    /// 顺序执行——一次只执行一个工具。
    Sequential,
    /// 并行执行——与其他并行模式工具同时执行。
    Parallel,
}

/// 每次工具调用时传递的上下文。
///
/// 由 Agent 在每次工具调用开始时创建，携带工具（尤其是扩展工具）
/// 所需的当前会话状态。
#[derive(Clone)]
pub struct ToolExecutionContext {
    /// 当前会话 ID。
    pub session_id: String,
    /// 工作目录路径。
    pub working_dir: String,
    /// 当前使用的模型标识。
    pub model_id: String,
    /// 当前可用的工具定义列表。
    pub available_tools: Vec<ToolDefinition>,
    /// 当前工具调用 ID，用于工具发出隶属于自身调用的进度事件。
    pub tool_call_id: Option<String>,
    /// 当前回合事件发送器，用于工具发出非持久化进度事件。
    pub event_tx: Option<mpsc::UnboundedSender<EventPayload>>,
    /// 当前 session 的工具结果 artifact 读取能力。
    pub tool_result_reader: Option<Arc<dyn ToolResultArtifactReader>>,
}

impl std::fmt::Debug for ToolExecutionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolExecutionContext")
            .field("session_id", &self.session_id)
            .field("working_dir", &self.working_dir)
            .field("model_id", &self.model_id)
            .field("available_tools", &self.available_tools)
            .field("tool_call_id", &self.tool_call_id)
            .field("event_tx", &self.event_tx.as_ref().map(|_| "<event_tx>"))
            .field(
                "tool_result_reader",
                &self.tool_result_reader.as_ref().map(|_| "<reader>"),
            )
            .finish()
    }
}

/// `Tool` trait——所有工具（内置和扩展注册）都必须实现此接口。
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// 返回工具的定义，用于 LLM 函数调用。
    fn definition(&self) -> ToolDefinition;

    /// 返回工具的执行模式偏好。
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }

    /// 使用给定参数和调用上下文执行工具。
    ///
    /// 内置工具通常忽略 `ctx`。扩展工具通过它访问会话状态，
    /// 并可通过 [`crate::extension::ExtensionToolOutcome::RunSession`]
    /// 请求创建子会话。
    async fn execute(
        &self,
        arguments: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolResult, ToolError>;
}
