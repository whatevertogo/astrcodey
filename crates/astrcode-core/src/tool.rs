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
//! - [`ToolPromptMetadata`]：结构化工具提示词元数据

use std::{collections::BTreeMap, sync::Arc};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::{event::EventPayload, storage::ToolResultArtifactReader, types::SessionId};

// Re-export BackgroundTaskReader from astrcode-tools via a forward declaration.
// The actual trait lives in astrcode-tools::task_tool, but ToolExecutionContext
// references it by Arc<dyn>. We use a minimal local trait to avoid the dependency.

/// 后台任务的只读查询能力。
///
/// 工具通过此 trait 查询当前会话的后台任务状态。
/// 由 agent loop 在构建 ToolExecutionContext 时注入。
pub trait BackgroundTaskReader: Send + Sync {
    /// 列出指定会话的所有活跃后台任务 ID。
    fn list_active(&self, session_id: &SessionId) -> Vec<crate::types::BackgroundTaskId>;

    /// 取消指定任务。返回 true 表示成功取消。
    fn cancel(&self, session_id: &SessionId, task_id: &crate::types::BackgroundTaskId) -> bool;
}

/// 工具来源分类，影响诊断日志和策略优先级，不改变执行路径。
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
    Sdk,
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
    /// 工具执行模式。运行时用它判断该工具能否和其他并行工具同批执行。
    #[serde(default)]
    pub execution_mode: ExecutionMode,
}

/// 结构化工具提示词元数据，用于 system prompt 的分层展示。
///
/// 补充 `ToolDefinition.description`（单行摘要）之外的结构化指导信息：
/// guide、caveats、examples 和分类标签。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ToolPromptMetadata {
    /// 详细使用说明，仅 discovery/collaboration 工具展示。
    #[serde(default)]
    pub guide: String,
    /// 注意事项和限制。
    #[serde(default)]
    pub caveats: Vec<String>,
    /// 使用示例。
    #[serde(default)]
    pub examples: Vec<String>,
    /// 分类标签（"filesystem", "collaboration", "discovery", "planning", "system"）。
    #[serde(default)]
    pub prompt_tags: Vec<String>,
    /// 是否始终出现在 prompt 中。
    #[serde(default)]
    pub always_include: bool,
}

impl ToolPromptMetadata {
    pub fn new(guide: impl Into<String>) -> Self {
        Self {
            guide: guide.into(),
            ..Default::default()
        }
    }

    pub fn caveat(mut self, caveat: impl Into<String>) -> Self {
        self.caveats.push(caveat.into());
        self
    }

    pub fn example(mut self, example: impl Into<String>) -> Self {
        self.examples.push(example.into());
        self
    }

    pub fn prompt_tag(mut self, tag: impl Into<String>) -> Self {
        self.prompt_tags.push(tag.into());
        self
    }

    pub fn always_include(mut self, val: bool) -> Self {
        self.always_include = val;
        self
    }
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// 顺序执行——一次只执行一个工具。
    #[default]
    Sequential,
    /// 并行执行——与其他并行模式工具同时执行。
    Parallel,
}

/// 工具的后台化策略。
///
/// 由 agent loop 的工具执行调度层查询，决定是否在执行超过阈值后
/// 将工具调用自动转入后台，不阻塞 agent loop 继续推进。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundPolicy {
    /// 不自动后台化（默认）。
    #[default]
    Never,
    /// 执行超过阈值秒数后自动后台化。
    AutoAfter { threshold_secs: u64 },
}

/// 文件观察快照，用于 read-before-edit 的乐观并发保护。
///
/// `read` 成功后记录当前文件版本，`edit` 写入前用它检测文件是否已被外部修改。
#[derive(Debug, Clone)]
pub struct FileObservation {
    /// 规范化后的文件路径。
    pub path: String,
    /// 文件大小（字节）。
    pub bytes: u64,
    /// 文件修改时间（Unix 纳秒）。
    pub modified_unix_nanos: Option<u64>,
    /// 文件内容的哈希指纹。
    pub content_fingerprint: String,
}

/// 文件观察快照的进程内存储。
///
/// 由 agent loop 创建并以 `Arc` 共享注入到 `ToolCapabilities`。
/// `read` 和 `edit` 工具通过它协作实现 read-before-edit 守卫。
pub trait FileObservationStore: Send + Sync {
    /// 记录一次文件观察。
    fn remember(&self, observation: FileObservation);
    /// 获取指定路径的最近一次观察快照。
    fn load(&self, path: &str) -> Option<FileObservation>;
}

/// Agent 工具执行时需要的会话操控能力。
///
/// 由 server 实现，通过 [`ToolCapabilities`] 注入。提供子会话的发送、中止、
/// 查询等原子操作，供 agent-tools 扩展消费。
#[async_trait::async_trait]
pub trait AgentSessionControl: Send + Sync {
    /// 向子 session 提交 prompt 并阻塞等待完成，返回带实际输出的结果。
    async fn send_and_wait(
        &self,
        child_session_id: &str,
        message: String,
    ) -> Result<TurnResult, String>;

    /// 中止单个 session。
    async fn abort_session(&self, session_id: &str) -> Result<(), String>;

    /// 查询子 agent 列表。
    async fn list_children(&self, session_id: &str) -> Result<Vec<AgentSessionInfo>, String>;
}

/// Turn 完成结果。
#[derive(Debug, Clone)]
pub enum TurnResult {
    /// 正常完成，携带输出文本。
    Completed { output: String },
    /// 执行失败。
    Failed { error: String },
    /// 被中止。
    Aborted,
}

/// 子 agent 信息（用于 list_children）。
#[derive(Debug, Clone)]
pub struct AgentSessionInfo {
    /// 子会话 ID。
    pub session_id: String,
    /// Agent 名称。
    pub agent_name: String,
    /// 任务描述。
    pub task: String,
    /// 当前状态。
    pub status: crate::storage::AgentSessionStatus,
}

/// 工具调用时按需注入的会话能力。
///
/// 大多数工具不需要这些能力；`Default::default()` 产生全部为 `None` 的空集。
/// 生产环境由 agent loop 在构建 `ToolExecutionContext` 时按需填充。
#[derive(Clone, Default)]
pub struct ToolCapabilities {
    /// 当前使用的模型标识（仅 FFI bridge 需要）。
    pub model_id: Option<String>,
    /// 当前可用的工具定义列表（仅 FFI bridge 需要）。
    pub available_tools: Option<Vec<ToolDefinition>>,
    /// 当前 session 的工具结果 artifact 读取能力（仅 `read` 工具需要）。
    pub tool_result_reader: Option<Arc<dyn ToolResultArtifactReader>>,
    /// 当前 session 的后台任务查询能力（仅 `task` 工具需要）。
    pub background_task_reader: Option<Arc<dyn BackgroundTaskReader>>,
    /// 当前 session 的文件观察存储（`read` 和 `edit` 工具协作使用）。
    pub file_observation_store: Option<Arc<dyn FileObservationStore>>,
    /// Agent 会话操控能力（`send` 工具使用）。
    pub agent_session_control: Option<Arc<dyn AgentSessionControl>>,
}

/// 每次工具调用时传递的上下文。
///
/// 由 Agent 在每次工具调用开始时创建。核心字段是每次调用都不同的
/// 会话标识和工具调用元数据；`capabilities` 携带特定工具才需要的
/// 可选能力，默认为空。
#[derive(Clone)]
pub struct ToolExecutionContext {
    /// 当前会话 ID。
    pub session_id: SessionId,
    /// 工作目录路径。
    pub working_dir: String,
    /// 当前工具调用 ID，用于工具发出隶属于自身调用的进度事件。
    pub tool_call_id: Option<String>,
    /// 当前回合事件发送器，用于工具发出非持久化进度事件。
    pub event_tx: Option<mpsc::UnboundedSender<EventPayload>>,
    /// 按需注入的会话能力。
    pub capabilities: ToolCapabilities,
}

/// Build a metadata map from key-value pairs.
pub fn tool_metadata<const N: usize>(
    entries: [(&str, serde_json::Value); N],
) -> BTreeMap<String, serde_json::Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

impl ToolResult {
    /// Convenience constructor for a text ToolResult.
    ///
    /// When `is_error` is true, `error` is automatically set to a clone of `content`.
    pub fn text(
        content: String,
        is_error: bool,
        metadata: BTreeMap<String, serde_json::Value>,
    ) -> Self {
        Self {
            call_id: String::new(),
            content: content.clone(),
            is_error,
            error: is_error.then_some(content),
            metadata,
            duration_ms: None,
        }
    }
}

impl std::fmt::Debug for ToolExecutionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolExecutionContext")
            .field("session_id", &self.session_id)
            .field("working_dir", &self.working_dir)
            .field("tool_call_id", &self.tool_call_id)
            .field("event_tx", &self.event_tx.as_ref().map(|_| "<event_tx>"))
            .field("capabilities", &self.capabilities)
            .finish()
    }
}

impl std::fmt::Debug for ToolCapabilities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCapabilities")
            .field("model_id", &self.model_id)
            .field(
                "available_tools",
                &self.available_tools.as_ref().map(|t| t.len()),
            )
            .field(
                "tool_result_reader",
                &self.tool_result_reader.as_ref().map(|_| "<reader>"),
            )
            .field(
                "background_task_reader",
                &self.background_task_reader.as_ref().map(|_| "<bg_reader>"),
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
        self.definition().execution_mode
    }

    /// 返回工具的后台化策略。
    ///
    /// 默认为 [`BackgroundPolicy::Never`]。工具可以覆写此方法声明
    /// 自己在执行时间过长时可以自动转入后台。
    fn background_policy(&self) -> BackgroundPolicy {
        BackgroundPolicy::Never
    }

    /// 返回工具的结构化提示词元数据。
    ///
    /// 用于 system prompt 的分层展示（summary、guide、caveats、examples、tags）。
    /// 默认返回 `None`，不提供元数据的工具回退到 `definition().description`。
    fn prompt_metadata(&self) -> Option<ToolPromptMetadata> {
        None
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
