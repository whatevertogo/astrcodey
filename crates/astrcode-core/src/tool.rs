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

use std::{collections::BTreeMap, path::Path, sync::Arc};

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::{
    event::EventPayload, storage::ToolResultArtifactReader, tool_access::ResourceAccess,
    types::SessionId,
};

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

/// 工具提示词元数据，**仅服务于 system prompt 中的"详细工具指引"段落**。
///
/// # 实际渲染规则（务必先读这段再修改字段）
///
/// LLM 看到的工具说明有两条独立通道：
///
/// 1. **原生 tool API**：`ToolDefinition.description` + 参数 schema description。
///    - 所有工具一视同仁，每次都发给 LLM。
///    - 这是工具用法的**主要载体**。约束、参数语义、与其它工具的关系都应写在这里。
///
/// 2. **System prompt 详细指引**：本结构的 `guide` / `caveats` / `examples`。
///    - 仅当 `prompt_tags` 含 [`ToolPromptTag::Discovery`] 或 [`ToolPromptTag::Collaboration`]
///      时才会被渲染。 具体见 [`Self::should_render_detailed_guide`]。
///    - 用于解释**使用策略**（什么时候用、什么时候别用），而非工具自身的语义。
///    - 当前只服务于 `tool_search_tool`（MCP discovery）、`Skill`、`agent` 三类工具。
///
/// # 不要
///
/// - 不要往 builtin（filesystem/system/planning 标签）工具的 `caveats` 里写约束 —— 它**不会**进
///   system prompt。把这类信息写到 `ToolDefinition.description` 或 参数 schema 的 description 里。
/// - 如果 builtin 工具确实需要 system prompt 级别的策略指引，扩展
///   [`Self::should_render_detailed_guide`]，而不是新增字段。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ToolPromptMetadata {
    /// 详细使用说明，仅当 `prompt_tags` 含 Discovery/Collaboration 时进 system prompt。
    #[serde(default)]
    pub guide: String,
    /// 注意事项，渲染条件同 `guide`。
    #[serde(default)]
    pub caveats: Vec<String>,
    /// 使用示例，渲染条件同 `guide`。
    #[serde(default)]
    pub examples: Vec<String>,
    /// 分类标签。决定渲染行为：[`ToolPromptTag::Discovery`] /
    /// [`ToolPromptTag::Collaboration`] 触发详细指引；其它标签仅作为分类。
    #[serde(default)]
    pub prompt_tags: Vec<ToolPromptTag>,
    /// Deferred discovery group. Tools in the same group are hidden from the
    /// provider until a matching discovery gate returns them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred_discovery_group: Option<String>,
    /// Discovery group unlocked by this tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred_discovery_gate: Option<String>,
}

/// 工具的渲染分类标签。
///
/// 序列化时使用 snake_case（例如 `Discovery` → `"discovery"`），
/// 与历史的字符串标签保持 wire 兼容。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ToolPromptTag {
    /// 文件系统类工具（read/write/edit/grep/glob/patch）。
    Filesystem,
    /// 系统类工具（shell/task）。
    System,
    /// 计划类工具（todoWrite/switchMode/upsertSessionPlan）。
    Planning,
    /// 工具发现入口（tool_search_tool/Skill）。会触发 system prompt 详细指引。
    Discovery,
    /// 协作/委派类工具（agent）。会触发 system prompt 详细指引并归入独立列表。
    Collaboration,
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

    pub fn prompt_tag(mut self, tag: ToolPromptTag) -> Self {
        self.prompt_tags.push(tag);
        self
    }

    pub fn deferred_discovery_group(mut self, group: impl Into<String>) -> Self {
        self.deferred_discovery_group = Some(group.into());
        self
    }

    pub fn deferred_discovery_gate(mut self, group: impl Into<String>) -> Self {
        self.deferred_discovery_gate = Some(group.into());
        self
    }

    /// 是否含指定标签。
    pub fn has_tag(&self, tag: ToolPromptTag) -> bool {
        self.prompt_tags.contains(&tag)
    }

    /// 是否触发 system prompt 中的"详细工具指引"渲染。
    ///
    /// 仅 [`ToolPromptTag::Discovery`] 和 [`ToolPromptTag::Collaboration`] 触发，
    /// 用于把 `guide` / `caveats` / `examples` 渲染到 system prompt。
    pub fn should_render_detailed_guide(&self) -> bool {
        self.has_tag(ToolPromptTag::Discovery) || self.has_tag(ToolPromptTag::Collaboration)
    }
}

pub const DEFERRED_TOOLS_METADATA_KEY: &str = "deferredTools";

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
    #[error("Tool execution blocked by hook: {reason}")]
    Blocked { reason: String },
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
/// 由 agent loop 创建并以 `Arc` 共享注入到 [`ToolFileServices::observation_store`]。
/// `read` 和 `edit` 工具通过它协作实现 read-before-edit 守卫。
pub trait FileObservationStore: Send + Sync {
    /// 记录一次文件观察。
    fn remember(&self, observation: FileObservation);
    /// 获取指定路径的最近一次观察快照。
    fn load(&self, path: &str) -> Option<FileObservation>;
}

// ─── SessionOperations：会话原子操作 trait ────────────────────────────────

/// 会话原子操作 trait。
///
/// 由 server 层实现，通过 [`ToolSessionControl::ops`] 暴露给工具/插件。
/// 插件在 `ToolHandler::execute` 中通过此接口自主编排子会话生命周期。
#[async_trait::async_trait]
pub trait SessionOperations: Send + Sync {
    /// 创建顶层会话。
    ///
    /// 供可信宿主入口（例如外部消息通道）把新的外部会话映射到 AstrCode
    /// session。普通子 agent 编排应继续使用 [`Self::create_session`]。
    async fn create_root_session(
        &self,
        request: CreateRootSessionRequest,
    ) -> Result<SessionHandle, SessionApiError>;

    /// 创建子会话。
    async fn create_session(
        &self,
        parent_session_id: &str,
        request: CreateSessionRequest,
    ) -> Result<SessionHandle, SessionApiError>;

    /// 向目标 session 注入一条 UserMessage。
    async fn inject_message(
        &self,
        access: SessionAccess<'_>,
        content: String,
    ) -> Result<(), SessionApiError>;

    /// 向目标 session 提交一个 turn。
    async fn submit_turn(
        &self,
        request: SubmitTurnRequest,
    ) -> Result<SubmitTurnResult, SessionApiError>;

    /// 查询目标 session 状态。
    async fn query_session(
        &self,
        access: SessionAccess<'_>,
    ) -> Result<SessionStatus, SessionApiError>;

    /// 回收目标 session 到 .recycled/ 目录（默认的清理方式）。
    ///
    /// 数据保留用于调试/审计，可通过 `restore_session` 恢复。
    async fn recycle_session(&self, access: SessionAccess<'_>) -> Result<(), SessionApiError>;

    /// 永久删除目标 session 及其所有数据。
    async fn delete_session(&self, access: SessionAccess<'_>) -> Result<(), SessionApiError>;

    /// 从 .recycled/ 恢复一个已回收的 session。
    async fn restore_session(&self, access: SessionAccess<'_>) -> Result<(), SessionApiError>;

    /// 解析目标 session 上挂起的工具审批。
    async fn resolve_tool_approval(
        &self,
        target_session_id: &str,
        call_id: &str,
        decision: crate::permission::ApprovalDecision,
    ) -> Result<(), SessionApiError>;
}

/// 创建顶层会话请求。
#[derive(Debug, Clone)]
pub struct CreateRootSessionRequest {
    /// 工作目录。
    pub working_dir: String,
    /// 创建该 session 的扩展 ID。
    pub source_extension: Option<String>,
}

/// 创建子会话请求。
#[derive(Debug, Clone, Default)]
pub struct CreateSessionRequest {
    /// 子会话显示名称。
    pub name: String,
    /// 工作目录。`None` 表示继承父 session。
    pub working_dir: Option<String>,
    /// 额外系统提示词。
    pub system_prompt: Option<String>,
    /// 模型偏好。`None` 表示继承父 session。
    pub model_preference: Option<String>,
    /// 子会话工具集策略。
    pub tool_policy: Option<crate::extension::ChildToolPolicy>,
    /// 创建该子 session 的扩展 ID。
    pub source_extension: Option<String>,
    /// 一次性子 session，首个 turn 完成后自动回收。
    pub ephemeral: bool,
    /// 触发创建子 session 的工具调用 ID，写入 AgentSessionSpawned 供 TUI 路由。
    pub tool_call_id: String,
}

/// 创建成功后返回的句柄。
#[derive(Debug, Clone)]
pub struct SessionHandle {
    pub session_id: String,
}

/// 跨 session 操作的调用方与目标（借用视图，用于 trait 方法参数）。
///
/// `caller` 须与 `target` 相同，或是 `target` 在 session 树中的祖先。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionAccess<'a> {
    pub caller_session_id: &'a str,
    pub target_session_id: &'a str,
}

impl<'a> SessionAccess<'a> {
    pub const fn new(caller_session_id: &'a str, target_session_id: &'a str) -> Self {
        Self {
            caller_session_id,
            target_session_id,
        }
    }

    /// 在同一 session 上操作（调用方即目标）。
    pub const fn same(session_id: &'a str) -> Self {
        Self::new(session_id, session_id)
    }
}

/// 跨 session 操作的调用方与目标（拥有所有权，用于请求 DTO）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAccessPair {
    pub caller_session_id: String,
    pub target_session_id: String,
}

impl SessionAccessPair {
    pub fn new(caller_session_id: impl Into<String>, target_session_id: impl Into<String>) -> Self {
        Self {
            caller_session_id: caller_session_id.into(),
            target_session_id: target_session_id.into(),
        }
    }

    /// 在同一 session 上操作（调用方即目标）。
    pub fn same(session_id: impl Into<String>) -> Self {
        let session_id = session_id.into();
        Self {
            caller_session_id: session_id.clone(),
            target_session_id: session_id,
        }
    }

    pub fn as_access(&self) -> SessionAccess<'_> {
        SessionAccess::new(
            self.caller_session_id.as_str(),
            self.target_session_id.as_str(),
        )
    }
}

/// 提交 turn 请求。
#[derive(Debug, Clone)]
pub struct SubmitTurnRequest {
    pub access: SessionAccessPair,
    /// 用户提示词。
    pub user_prompt: String,
    /// 是否同步阻塞等待 turn 完成。
    pub wait_for_result: bool,
    /// 异步模式完成后向父 session 注入的通知文本。
    pub notify_parent_on_complete: Option<String>,
    /// 异步模式 turn 完成后自动回收目标 session。
    pub recycle_on_complete: bool,
    /// 触发此次 turn 的工具调用 ID。
    pub tool_call_id: Option<String>,
}

impl SubmitTurnRequest {
    fn with_access(access: SessionAccessPair, user_prompt: impl Into<String>) -> Self {
        Self {
            access,
            user_prompt: user_prompt.into(),
            wait_for_result: true,
            notify_parent_on_complete: None,
            recycle_on_complete: false,
            tool_call_id: None,
        }
    }

    /// 在同一 session 上提交 turn（例如外部通道 → 顶层会话）。
    pub fn for_session(session_id: impl Into<String>, user_prompt: impl Into<String>) -> Self {
        Self::with_access(SessionAccessPair::same(session_id), user_prompt)
    }

    /// 父 session 向子 session 提交 turn。
    pub fn for_child(
        caller_session_id: impl Into<String>,
        child_session_id: impl Into<String>,
        user_prompt: impl Into<String>,
    ) -> Self {
        Self::with_access(
            SessionAccessPair::new(caller_session_id, child_session_id),
            user_prompt,
        )
    }

    pub fn wait_for_result(mut self, wait_for_result: bool) -> Self {
        self.wait_for_result = wait_for_result;
        self
    }

    pub fn notify_parent_on_complete(mut self, message: Option<String>) -> Self {
        self.notify_parent_on_complete = message;
        self
    }

    pub fn recycle_on_complete(mut self, recycle_on_complete: bool) -> Self {
        self.recycle_on_complete = recycle_on_complete;
        self
    }

    pub fn tool_call_id(mut self, tool_call_id: Option<String>) -> Self {
        self.tool_call_id = tool_call_id;
        self
    }
}

/// 提交 turn 结果。
#[derive(Debug, Clone)]
pub enum SubmitTurnResult {
    /// 同步完成。
    Completed { content: String },
    /// 异步后台执行。
    Backgrounded { task_id: String, session_id: String },
}

/// 会话状态查询结果。
#[derive(Debug, Clone)]
pub struct SessionStatus {
    pub alive: bool,
    pub has_active_turn: bool,
    pub last_finish_reason: Option<String>,
    pub message_count: usize,
}

/// Session API 错误。
#[derive(Debug, thiserror::Error)]
pub enum SessionApiError {
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("session busy: {0}")]
    SessionBusy(String),
    #[error("max depth exceeded: current={current}, max={max}")]
    MaxDepthExceeded { current: usize, max: usize },
    #[error(transparent)]
    Internal(#[from] SessionApiInternalError),
}

/// 保留 `source` 链的内部错误，避免 API 边界 stringify 结构化错误。
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SessionApiInternalError(Box<dyn std::error::Error + Send + Sync>);

impl SessionApiInternalError {
    fn message(text: impl Into<String>) -> Self {
        Self(Box::new(SessionApiInternalMessage(text.into())))
    }
}

#[derive(Debug)]
struct SessionApiInternalMessage(String);

impl std::fmt::Display for SessionApiInternalMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for SessionApiInternalMessage {}

impl SessionApiError {
    pub fn internal<E: std::error::Error + Send + Sync + 'static>(error: E) -> Self {
        Self::Internal(SessionApiInternalError(Box::new(error)))
    }

    pub fn internal_msg(msg: impl Into<String>) -> Self {
        Self::Internal(SessionApiInternalError::message(msg))
    }
}

/// 按档位暴露的 LLM model id（与 effective config 对齐）。
///
/// 后续可增加 `middle` 等字段；子 agent / 插件应显式选择档位，避免硬编码字段名。
#[derive(Clone, Debug, Default)]
pub struct LlmModelIds {
    /// 父 session 主模型（`activeModel`）。
    pub main: Option<String>,
    /// 配置的小模型（`activeSmallModel`）。
    pub small: Option<String>,
}

/// 模型档位访问（须在扩展 manifest 声明对应能力后才有值）。
#[derive(Clone, Default)]
pub struct ToolModelAccess {
    /// 与 [`Self::main`] 相同；保留供既有调用方。
    pub model_id: Option<String>,
    /// 主模型 id（`main_model` 能力）。
    pub main: Option<String>,
    /// 小模型 id（`small_model` 能力）。
    pub small: Option<String>,
    /// 分档模型 id 快照（未声明对应能力时，各档可能为 `None`）。
    pub tiers: LlmModelIds,
}

/// 会话存储路径（`session_state` 能力）。
#[derive(Clone, Debug, Default)]
pub struct ToolSessionPaths {
    /// 当前 session 在存储层中的真实目录路径。
    ///
    /// 子 session 的真实目录可能在 `subagents/{extension}/` 下，
    /// 无法从 session_id + working_dir 推断。工具需要写附属数据时
    /// 应使用此路径，而非自行拼接。
    pub store_dir: Option<std::path::PathBuf>,
}

/// 会话编排能力（`session_control` 能力）。
#[derive(Clone, Default)]
pub struct ToolSessionControl {
    pub ops: Option<Arc<dyn SessionOperations>>,
}

/// 文件 read/edit 协作服务。
#[derive(Clone, Default)]
pub struct ToolFileServices {
    /// `read` 与 `edit` 共享的观察存储（由 agent loop 注入）。
    pub observation_store: Option<Arc<dyn FileObservationStore>>,
}

/// 宿主侧服务：artifact 读取、FFI 工具目录、扩展事件。
#[derive(Clone, Default)]
pub struct ToolHostServices {
    /// 当前 session 的工具结果 artifact 读取能力（仅 `read` 工具需要）。
    pub result_reader: Option<Arc<dyn ToolResultArtifactReader>>,
    /// 当前可用的工具定义列表（仅 FFI bridge 需要）。
    pub available_tools: Option<Vec<ToolDefinition>>,
    /// 插件事件发射器（仅声明 `emit_events` 的扩展工具会有值）。
    pub extension_event_sink: Option<Arc<dyn crate::extension::ExtensionEventSink>>,
}

/// 工具调用时按需注入的能力集合。
///
/// 按职责拆分为子结构，工具只依赖自己需要的那一组。`Default::default()`
/// 产生全部为 `None` 的空集；生产环境由 agent loop 在构建
/// [`ToolExecutionContext`] 时按需填充。
#[derive(Clone, Default)]
pub struct ToolCapabilities {
    pub models: ToolModelAccess,
    pub paths: ToolSessionPaths,
    pub session: ToolSessionControl,
    pub files: ToolFileServices,
    pub host: ToolHostServices,
}

/// 每次工具调用的强制上下文（会话标识与 I/O 通道）。
#[derive(Clone)]
pub struct ToolCallScope {
    pub session_id: SessionId,
    pub working_dir: String,
    /// 当前工具调用 ID，用于工具发出隶属于自身调用的进度事件。
    pub tool_call_id: Option<String>,
    /// 当前回合事件发送器，用于工具发出非持久化进度事件。
    pub event_tx: Option<mpsc::UnboundedSender<EventPayload>>,
}

/// 每次工具调用时传递的上下文。
///
/// 由 Agent 在每次工具调用开始时创建。[`ToolCallScope`] 为每次调用
/// 都不同的会话标识与通道；[`ToolCapabilities`] 为特定工具才需要的
/// 可选能力，默认为空。
#[derive(Clone)]
pub struct ToolExecutionContext {
    pub scope: ToolCallScope,
    pub capabilities: ToolCapabilities,
}

impl ToolExecutionContext {
    pub fn new(
        session_id: SessionId,
        working_dir: impl Into<String>,
        tool_call_id: Option<String>,
        event_tx: Option<mpsc::UnboundedSender<EventPayload>>,
        capabilities: ToolCapabilities,
    ) -> Self {
        Self {
            scope: ToolCallScope {
                session_id,
                working_dir: working_dir.into(),
                tool_call_id,
                event_tx,
            },
            capabilities,
        }
    }
}

impl std::ops::Deref for ToolExecutionContext {
    type Target = ToolCallScope;

    fn deref(&self) -> &Self::Target {
        &self.scope
    }
}

impl std::ops::DerefMut for ToolExecutionContext {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.scope
    }
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
        let error = is_error.then(|| content.clone());
        Self {
            call_id: String::new(),
            content,
            is_error,
            error,
            metadata,
            duration_ms: None,
        }
    }
}

impl std::fmt::Debug for ToolCallScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCallScope")
            .field("session_id", &self.session_id)
            .field("working_dir", &self.working_dir)
            .field("tool_call_id", &self.tool_call_id)
            .field("event_tx", &self.event_tx.as_ref().map(|_| "<event_tx>"))
            .finish()
    }
}

impl std::fmt::Debug for ToolExecutionContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolExecutionContext")
            .field("scope", &self.scope)
            .field("capabilities", &self.capabilities)
            .finish()
    }
}

impl std::fmt::Debug for ToolCapabilities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCapabilities")
            .field("models", &self.models)
            .field("paths", &self.paths)
            .field("session", &self.session)
            .field("files", &self.files)
            .field("host", &self.host)
            .finish()
    }
}

impl std::fmt::Debug for ToolSessionControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolSessionControl")
            .field("ops", &self.ops.as_ref().map(|_| "<session_ops>"))
            .finish()
    }
}

impl std::fmt::Debug for ToolFileServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolFileServices")
            .field(
                "observation_store",
                &self.observation_store.as_ref().map(|_| "<store>"),
            )
            .finish()
    }
}

impl std::fmt::Debug for ToolModelAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolModelAccess")
            .field("model_id", &self.model_id)
            .field("main", &self.main)
            .field("small", &self.small)
            .field("tiers", &self.tiers)
            .finish()
    }
}

impl std::fmt::Debug for ToolHostServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolHostServices")
            .field(
                "available_tools",
                &self.available_tools.as_ref().map(|t| t.len()),
            )
            .field(
                "result_reader",
                &self.result_reader.as_ref().map(|_| "<reader>"),
            )
            .finish()
    }
}

/// `Tool` trait——所有工具（内置和扩展注册）都必须实现此接口。
///
/// 使用 `async_trait` 是因为注册表以 [`Arc<dyn Tool>`] 做类型擦除；
/// 稳定版 Rust 的 trait 内 `async fn` 尚不支持 `dyn` 兼容（需消除
/// `dyn Tool` 后才能迁移到原生 async fn in trait）。
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// 返回工具的定义，用于 LLM 函数调用。
    fn definition(&self) -> ToolDefinition;

    /// 返回工具的执行模式偏好。
    fn execution_mode(&self) -> ExecutionMode {
        self.definition().execution_mode
    }

    /// 声明本次调用将访问的资源，供冲突图调度器判定并行性。
    ///
    /// 默认保守返回 [`ResourceAccess::All`]。内置工具应基于参数动态解析路径。
    fn resource_accesses(
        &self,
        _arguments: &serde_json::Value,
        _working_dir: &Path,
    ) -> Result<Vec<ResourceAccess>, ToolError> {
        Ok(vec![ResourceAccess::all()])
    }

    /// 返回工具的结构化提示词元数据。
    ///
    /// **多数工具不需要实现此方法**——它的渲染规则非常窄，详见
    /// [`ToolPromptMetadata`] 的 doc。简单来说：
    /// - 想让 LLM 看到工具用法、参数语义、约束 → 写在 `definition().description` 或参数 schema 里；
    /// - 仅当工具属于 discovery（如 `tool_search_tool`、`Skill`）或 collaboration （如
    ///   `agent`），需要在 system prompt 里给出**使用策略**指引时，才填本字段。
    ///
    /// 默认返回 `None`。
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
