//! 工具 trait 及关联类型。
//!
//! 工具是 Agent 与外部世界交互的主要方式。
//! 所有工具都由扩展注册；core 只定义共同领域契约。
//!
//! 本模块定义了：
//! - [`Tool`] trait：所有工具（内置和扩展注册）的核心接口
//! - [`ToolDefinition`]：发送给 LLM 的工具函数调用 schema
//! - [`ToolResult`]：工具执行结果
//! - [`ToolExecutionContext`]：每次工具调用的上下文
//! - [`ToolPromptMetadata`]：结构化工具提示词元数据
//!
//! 本模块不含具体工具实现与调度逻辑（注册表、并行调度、权限门禁位于
//! `astrcode-session` / 各工具 crate）。

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};

use crate::types::{SessionId, TurnId};

pub mod access;
pub mod read_image;
pub mod selection;

use access::{ResourceLease, ToolPlan};
pub use selection::{EmptyToolNameError, SessionToolSelection, validated_tool_names};

/// 工具来源分类，影响诊断日志和策略优先级，不改变执行路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOrigin {
    /// First-party tools shipped as bundled extensions.
    Bundled,
    /// Tools contributed by user or project extensions.
    Extension,
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
    /// 是否要求支持该能力的 provider 严格遵守参数 schema。
    ///
    /// Provider profile 还必须声明 `supportsStrictToolUse`；否则该字段不会映射到 wire。
    #[serde(default)]
    pub strict: bool,
    /// 工具来源。来源只影响诊断、策略和优先级，不创建额外执行路径。
    pub origin: ToolOrigin,
}

/// 宿主执行工具时采用的静态策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolExecutionPolicy {
    /// 该工具能否与同一批次中的其它工具并行执行。
    pub mode: ExecutionMode,
    /// `execute` 阶段的有效超时；`None` 表示宿主不额外限制执行时长。
    pub timeout: Option<Duration>,
}

impl ToolExecutionPolicy {
    pub const PARALLEL: Self = Self {
        mode: ExecutionMode::Parallel,
        timeout: None,
    };

    pub const SEQUENTIAL: Self = Self {
        mode: ExecutionMode::Sequential,
        timeout: None,
    };
}

impl Default for ToolExecutionPolicy {
    fn default() -> Self {
        Self::SEQUENTIAL
    }
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
/// - 不要往普通 filesystem/system/planning 工具的 `caveats` 里写约束 —— 它**不会**进 system
///   prompt。把这类信息写到 `ToolDefinition.description` 或 参数 schema 的 description 里。
/// - 如果普通工具确实需要 system prompt 级别的策略指引，扩展
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

/// 工具执行结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolResultArtifactSlice {
    pub artifact_id: String,
    pub bytes: usize,
    pub byte_offset: usize,
    pub returned_bytes: usize,
    pub next_byte_offset: Option<usize>,
    pub has_more: bool,
    pub content: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ToolResultArtifactError {
    #[error("invalid tool result artifact id: {0}")]
    InvalidId(String),
    #[error("invalid tool result artifact read request: {0}")]
    InvalidRequest(String),
    #[error("tool result artifact not found: {0}")]
    NotFound(String),
    #[error("tool result artifact reading is unsupported: {0}")]
    Unsupported(String),
    #[error("tool result artifact read failed: {0}")]
    Read(String),
}

#[async_trait::async_trait]
pub trait ToolResultArtifactReader: Send + Sync {
    async fn read_tool_result_artifact(
        &self,
        session_id: &SessionId,
        artifact_id: &str,
        byte_offset: usize,
        max_bytes: usize,
    ) -> Result<ToolResultArtifactSlice, ToolResultArtifactError>;
}

impl ToolResult {
    /// 构造成功的文本结果。
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            error: None,
            metadata: BTreeMap::new(),
            duration_ms: None,
        }
    }

    /// 构造已正常返回的错误结果。
    ///
    /// 这表示工具执行完成，但业务结果为错误；执行基础设施失败由 session
    /// 的终态模型单独表达。
    pub fn error(content: impl Into<String>) -> Self {
        let content = content.into();
        Self {
            error: Some(content.clone()),
            content,
            is_error: true,
            metadata: BTreeMap::new(),
            duration_ms: None,
        }
    }

    /// 构造带元数据的文本结果。
    ///
    /// `is_error` 为 `true` 时，同时填充结构化错误文本。
    pub fn text(
        content: String,
        is_error: bool,
        metadata: BTreeMap<String, serde_json::Value>,
    ) -> Self {
        let error = is_error.then(|| content.clone());
        Self {
            content,
            is_error,
            error,
            metadata,
            duration_ms: None,
        }
    }

    pub fn with_metadata(mut self, metadata: BTreeMap<String, serde_json::Value>) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn with_duration_ms(mut self, duration_ms: Option<u64>) -> Self {
        self.duration_ms = duration_ms;
        self
    }

    /// 声明本次结果的呈现 intent，供 UI 选择对应的内置渲染。
    ///
    /// 只写入 [`PRESENTATION_METADATA_KEY`] 元数据；metadata 不进 LLM prompt，
    /// 运行时不得据此改变控制流。
    pub fn with_presentation(mut self, presentation: ToolPresentation) -> Self {
        self.metadata.insert(
            PRESENTATION_METADATA_KEY.to_owned(),
            serde_json::Value::String(presentation.as_str().to_owned()),
        );
        self
    }

    /// 读取结果声明的呈现 intent；未声明或值无法识别时返回 `None`。
    pub fn presentation(&self) -> Option<ToolPresentation> {
        serde_json::from_value(self.metadata.get(PRESENTATION_METADATA_KEY)?.clone()).ok()
    }
}

/// `ToolResult.metadata` 中呈现 intent 的键。前端/TUI 按此键拾取 intent。
pub const PRESENTATION_METADATA_KEY: &str = "presentation";

/// 工具结果的呈现 intent。
///
/// 每个变体对应 UI 的一种内置渲染（与前端/TUI 注册表中的渲染种类一一对应），
/// 序列化为 snake_case 字符串。未知字符串由消费方按未声明处理，保证向前兼容。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPresentation {
    /// 默认通用渲染（与不声明 intent 等价）。
    Generic,
    /// 终端/命令输出风格渲染。
    Terminal,
    /// 文件变更/diff 风格渲染。
    Diff,
    /// 搜索结果风格渲染。
    Search,
    /// 文件读取风格渲染。
    Read,
}

impl ToolPresentation {
    /// wire 字符串值，与 serde 的 snake_case 表示一致（由测试保证）。
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Terminal => "terminal",
            Self::Diff => "diff",
            Self::Search => "search",
            Self::Read => "read",
        }
    }
}

/// 工具执行的显式终态。
///
/// 工具发现是唯一会改变当前 turn 工具可见性的执行结果。普通 metadata
/// 只用于展示和诊断，运行时不得据此改变控制流。
#[derive(Debug, Clone, PartialEq)]
pub enum ToolExecutionResult {
    Completed(ToolResult),
    CompletedWithDiscoveredTools {
        result: ToolResult,
        tool_names: Vec<String>,
    },
}

impl ToolExecutionResult {
    pub fn completed(result: ToolResult) -> Self {
        Self::Completed(result)
    }

    pub fn completed_with_discovered_tools(result: ToolResult, tool_names: Vec<String>) -> Self {
        Self::CompletedWithDiscoveredTools { result, tool_names }
    }

    pub fn result_mut(&mut self) -> &mut ToolResult {
        match self {
            Self::Completed(result) | Self::CompletedWithDiscoveredTools { result, .. } => result,
        }
    }

    pub fn into_parts(self) -> (ToolResult, Vec<String>) {
        match self {
            Self::Completed(result) => (result, Vec::new()),
            Self::CompletedWithDiscoveredTools { result, tool_names } => (result, tool_names),
        }
    }
}

impl From<ToolResult> for ToolExecutionResult {
    fn from(result: ToolResult) -> Self {
        Self::Completed(result)
    }
}

impl std::ops::Deref for ToolExecutionResult {
    type Target = ToolResult;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Completed(result) | Self::CompletedWithDiscoveredTools { result, .. } => result,
        }
    }
}

impl std::ops::DerefMut for ToolExecutionResult {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.result_mut()
    }
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
    ) -> Result<SessionDeliveryOutcome, SessionApiError>;

    /// 目标 session 运行中时将输入排入 FIFO 队列（当前 turn 结束后自动开新 turn），
    /// idle 时直接开新 turn。
    async fn queue_or_start(
        &self,
        _access: SessionAccess<'_>,
        _content: String,
    ) -> Result<SessionDeliveryOutcome, SessionApiError> {
        Err(SessionApiError::Unsupported(
            "queue_or_start is not supported by this host".into(),
        ))
    }

    /// 仅向目标 session 的活跃 turn 注入一条 UserMessage（下一 step 边界吸收）；
    /// 无活跃 turn 时返回 [`SessionApiError::NoActiveTurn`]，不排队也不开新 turn。
    async fn defer_context(
        &self,
        _access: SessionAccess<'_>,
        _content: String,
    ) -> Result<SessionDeliveryOutcome, SessionApiError> {
        Err(SessionApiError::Unsupported(
            "defer_context is not supported by this host".into(),
        ))
    }

    /// 中断目标会话的活跃 turn，并提交新的用户输入。
    async fn interrupt_and_submit(
        &self,
        _access: SessionAccess<'_>,
        _content: String,
    ) -> Result<SessionDeliveryOutcome, SessionApiError> {
        Err(SessionApiError::Unsupported(
            "interrupt_and_submit is not supported by this host".into(),
        ))
    }

    /// 取消目标会话的活跃 turn。
    async fn cancel_turn(&self, _access: SessionAccess<'_>) -> Result<bool, SessionApiError> {
        Err(SessionApiError::Unsupported(
            "cancel_turn is not supported by this host".into(),
        ))
    }

    /// 查询目标会话的热执行状态。
    async fn execution_view(
        &self,
        _access: SessionAccess<'_>,
    ) -> Result<SessionExecutionView, SessionApiError> {
        Err(SessionApiError::Unsupported(
            "execution_view is not supported by this host".into(),
        ))
    }

    /// 配置目标 session 后续 turn 使用的工具边界。
    ///
    /// 当前活跃 turn 继续使用已经固定的工具快照。子 session 的请求不能扩大
    /// 当前父 session 的工具边界。
    async fn configure_tools(
        &self,
        _access: SessionAccess<'_>,
        _selection: SessionToolSelection,
    ) -> Result<SessionToolSelection, SessionApiError> {
        Err(SessionApiError::Unsupported(
            "session tool configuration is not supported by this host".into(),
        ))
    }

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

    /// 查询活跃或已回收 session 的生命周期与执行快照。
    async fn session_state(
        &self,
        _access: SessionAccess<'_>,
    ) -> Result<SessionState, SessionApiError> {
        Err(SessionApiError::Unsupported(
            "session state is not supported by this host".into(),
        ))
    }

    /// 回收目标 session 到 .recycled/ 目录（默认的清理方式）。
    ///
    /// 数据保留用于调试/审计，可通过 `restore_session` 恢复。
    async fn recycle_session(&self, access: SessionAccess<'_>) -> Result<(), SessionApiError>;

    /// 永久删除目标 session 及其所有数据。
    async fn delete_session(&self, access: SessionAccess<'_>) -> Result<(), SessionApiError>;

    /// 从 .recycled/ 恢复一个已回收的 session。
    async fn restore_session(&self, access: SessionAccess<'_>) -> Result<(), SessionApiError>;

    /// 完整激活一个已回收的直属子 session。
    ///
    /// 与仅恢复存储位置的 [`Self::restore_session`] 不同，该操作还必须恢复运行时并
    /// 重新建立父 session 的活跃子会话关系。
    async fn reactivate_session(
        &self,
        _access: SessionAccess<'_>,
    ) -> Result<SessionReactivation, SessionApiError> {
        Err(SessionApiError::Unsupported(
            "session reactivation is not supported by this host".into(),
        ))
    }

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
    /// 额外系统提示词。
    pub system_prompt: Option<String>,
    /// 模型偏好。`None` 表示继承父 session。
    pub model_preference: Option<String>,
    /// 子会话工具集策略。
    pub tool_selection: Option<SessionToolSelection>,
    /// 创建该子 session 的扩展 ID。
    pub source_extension: Option<String>,
    /// 一次性子 session，首个 turn 完成后自动回收。
    pub ephemeral: bool,
    /// 触发创建子 session 的工具调用 ID。
    pub tool_call_id: Option<String>,
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

/// Session 数据当前所处的生命周期位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLifecycleState {
    Active,
    Recycled,
}

/// 活跃或已回收 session 的只读状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionState {
    pub lifecycle: SessionLifecycleState,
    pub phase: crate::event::Phase,
    pub active_turn_id: Option<String>,
    pub queued_inputs: usize,
    pub message_count: usize,
}

/// Session 激活结果；重复激活是成功的幂等 no-op。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionReactivation {
    pub reactivated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionDeliveryOutcome {
    Started { turn_id: String },
    Injected { turn_id: String },
    Queued { queue_len: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionExecutionView {
    pub phase: crate::event::Phase,
    pub active_turn_id: Option<String>,
    pub queued_inputs: usize,
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
    #[error("no active turn in session: {0}")]
    NoActiveTurn(String),
    #[error("max depth exceeded: current={current}, max={max}")]
    MaxDepthExceeded { current: usize, max: usize },
    #[error("unsupported session operation: {0}")]
    Unsupported(String),
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
///
/// 主/小模型 id 由 [`LlmModelIds`] 统一承载，避免同一模型 id 在两处重复存储、
/// 需要手动保持同步。
#[derive(Clone, Debug, Default)]
pub struct ToolModelAccess {
    /// 分档模型 id 快照（未声明对应能力时，各档可能为 `None`）。
    pub tiers: LlmModelIds,
}

/// 当前 session 的命名空间状态基础路径。
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

/// 宿主侧服务：turn 模型绑定、artifact 读取与 FFI 工具目录。
#[derive(Clone, Default)]
pub struct ToolHostServices {
    /// 拥有此工具调用的 turn 固定的 provider generation。
    pub llm_providers: Option<crate::llm::LlmProviderBindings>,
    /// 当前 session 的工具结果 artifact 读取能力（仅 `read` 工具需要）。
    pub result_reader: Option<Arc<dyn ToolResultArtifactReader>>,
    /// 当前可用的工具定义列表（仅 FFI bridge 需要）。
    pub available_tools: Option<Vec<ToolDefinition>>,
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
    /// 当前工具调用所属 turn；会话外调用不存在该事实。
    pub turn_id: Option<TurnId>,
    pub working_dir: String,
    /// 当前工具调用 ID，用于工具发出隶属于自身调用的进度事件。
    pub tool_call_id: Option<String>,
    /// 当前回合事件发送器，用于工具发出非持久化进度事件。
    pub event_tx: Option<crate::event::EventSender>,
}

/// Host-internal facts available while planning one tool invocation.
///
/// This context deliberately excludes executable capabilities and event channels. Adapters may
/// project it into an author-facing planning context, but planning cannot perform Host I/O.
#[derive(Clone, Debug)]
pub struct ToolPlanningContext {
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub working_dir: String,
    pub tool_call_id: Option<String>,
    cancellation: tokio_util::sync::CancellationToken,
}

impl ToolPlanningContext {
    pub fn new(
        session_id: SessionId,
        working_dir: impl Into<String>,
        tool_call_id: Option<String>,
    ) -> Self {
        Self {
            session_id,
            turn_id: None,
            working_dir: working_dir.into(),
            tool_call_id,
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }

    pub fn with_turn_id(mut self, turn_id: TurnId) -> Self {
        self.turn_id = Some(turn_id);
        self
    }

    pub fn with_cancellation(mut self, cancellation: tokio_util::sync::CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub fn cancellation(&self) -> &tokio_util::sync::CancellationToken {
        &self.cancellation
    }
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
    resource_lease: Option<ResourceLease>,
    cancellation: tokio_util::sync::CancellationToken,
}

impl ToolExecutionContext {
    pub fn new(
        session_id: SessionId,
        working_dir: impl Into<String>,
        tool_call_id: Option<String>,
        event_tx: Option<crate::event::EventSender>,
        capabilities: ToolCapabilities,
    ) -> Self {
        Self {
            scope: ToolCallScope {
                session_id,
                turn_id: None,
                working_dir: working_dir.into(),
                tool_call_id,
                event_tx,
            },
            capabilities,
            resource_lease: None,
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// Attach the resource lease approved for this exact tool invocation.
    pub fn with_resource_lease(mut self, resource_lease: ResourceLease) -> Self {
        self.resource_lease = Some(resource_lease);
        self
    }

    pub fn resource_lease(&self) -> Option<&ResourceLease> {
        self.resource_lease.as_ref()
    }

    /// Attach the turn cancellation signal that owns this tool call.
    pub fn with_cancellation(mut self, cancellation: tokio_util::sync::CancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }

    /// Attach the strongly typed turn that owns this tool invocation.
    pub fn with_turn_id(mut self, turn_id: TurnId) -> Self {
        self.scope.turn_id = Some(turn_id);
        self
    }

    pub fn turn_id(&self) -> Option<&TurnId> {
        self.scope.turn_id.as_ref()
    }

    /// Cancellation of the turn or request that owns this tool invocation.
    pub fn cancellation(&self) -> &tokio_util::sync::CancellationToken {
        &self.cancellation
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

impl std::fmt::Debug for ToolCallScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolCallScope")
            .field("session_id", &self.session_id)
            .field("turn_id", &self.turn_id)
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
            .field("resource_lease", &self.resource_lease)
            .field("cancelled", &self.cancellation.is_cancelled())
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

impl std::fmt::Debug for ToolHostServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolHostServices")
            .field("llm_providers", &self.llm_providers)
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

    /// 返回工具的宿主执行策略。
    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::default()
    }

    /// Plan the resources required by the final, immutable tool arguments.
    async fn plan(
        &self,
        arguments: &serde_json::Value,
        ctx: &ToolPlanningContext,
    ) -> Result<ToolPlan, ToolError>;

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
    /// 工具通常只使用自己声明过的窄能力。
    async fn execute(
        &self,
        arguments: serde_json::Value,
        ctx: &ToolExecutionContext,
    ) -> Result<ToolExecutionResult, ToolError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presentation_as_str_matches_serde_wire_values() {
        for presentation in [
            ToolPresentation::Generic,
            ToolPresentation::Terminal,
            ToolPresentation::Diff,
            ToolPresentation::Search,
            ToolPresentation::Read,
        ] {
            let wire = serde_json::to_value(presentation).unwrap();
            assert_eq!(wire, serde_json::json!(presentation.as_str()));
        }
    }

    #[test]
    fn presentation_intent_roundtrips_through_metadata() {
        let result = ToolResult::success("ok").with_presentation(ToolPresentation::Terminal);
        assert_eq!(result.presentation(), Some(ToolPresentation::Terminal));
        assert_eq!(
            result.metadata[PRESENTATION_METADATA_KEY],
            serde_json::json!("terminal")
        );

        assert_eq!(ToolResult::success("ok").presentation(), None);

        let unknown = ToolResult::success("ok").with_metadata(tool_metadata([(
            PRESENTATION_METADATA_KEY,
            serde_json::json!("future_intent"),
        )]));
        assert_eq!(unknown.presentation(), None);
    }
}
